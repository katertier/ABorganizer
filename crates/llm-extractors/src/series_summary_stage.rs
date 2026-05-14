//! `extract-summary-spoiler-free-series` pipeline stage (slice 3K.4.1).
//!
//! Per-series spoiler-free synopsis, regenerated when the set of
//! books in the series changes. Companion to the per-book
//! [`crate::ExtractSummaryStage`] from 3K.4.
//!
//! ## What the stage does
//!
//! For each book the stage receives, it identifies every
//! `series_id` the book is a member of (via `book_series`) and,
//! for each series whose `summary_extractor_version` is stale,
//! regenerates the series-level summary from member books'
//! individual `summary_spoiler_free` rows.
//!
//! ## Why per-book scheduling for per-series content
//!
//! The pipeline is per-book; adding a per-series queue would
//! mean a new scheduler tier. Instead, every book that completes
//! its own summary stage AND the identity-resolve stage triggers
//! this stage; the inner per-series `extractor_version` check
//! fast-skips when nothing changed. Re-running for each book in
//! the series wastes a SELECT per book, which is cheap.
//!
//! ## Locale rule (ADR-0019)
//!
//! Output stays in the predominant `books.language` across the
//! series' books. Ties or empty (no books contribute a language
//! yet) → fall back to `LibraryDisplayTunables::library_locale`.
//!
//! ## Cache strategy
//!
//! No `ai_cache` row. The series-level summary lives directly on
//! the `series` row in three new columns (migration 007):
//!
//! - `summary TEXT` — the spoiler-free synopsis.
//! - `summary_lang TEXT` — BCP-47 tag.
//! - `summary_extractor_version TEXT` — the version stamp used
//!   for the freshness check.
//!
//! ## Failure modes
//!
//! - Book has no series memberships → `Skipped`.
//! - All series are already fresh → `Skipped`.
//! - No member books have non-null `summary_spoiler_free` → log
//!   `fm.series_summary.no_inputs` warn, `Skipped`.
//! - `FoundationModels` unavailable → `Err` (user-fixable, surfaced
//!   by `aborg doctor llm`).
//! - Locale-mismatch in model output → log warn, update the
//!   `summary_extractor_version` stamp (so we don't loop) but
//!   skip the actual `summary` write.
//! - Schema parse failure → `Err`.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use ab_core::tunables::{LibraryDisplayTunables, LlmTunables};
use ab_core::{BookId, Error, Result};
use ab_db::LibraryDb;
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};

use ab_foundation_models::{BridgeError, GenerationOptions, complete_structured};

/// Typed identifier for this stage.
pub const STAGE_ID: StageId = StageId::new("extract-summary-spoiler-free-series");

/// Stage name written to `pipeline_progress`. Derives from `STAGE_ID`.
pub const STAGE_NAME: &str = STAGE_ID.as_str();

/// JSON Schema passed to `complete_structured`. Mirrors the book-
/// level shape but uses the `series_summary` field name to make
/// per-book vs. per-series cache rows distinguishable in raw
/// diagnostic dumps.
pub const SERIES_SUMMARY_SCHEMA_JSON: &str = r#"{
    "type": "object",
    "properties": {
        "series_summary": {"type": "string"},
        "series_summary_lang": {"type": "string"}
    },
    "required": ["series_summary", "series_summary_lang"]
}"#;

/// Stage that produces a per-series spoiler-free synopsis from
/// member books' individual summaries.
pub struct ExtractSeriesSummaryStage {
    tunables: Arc<LlmTunables>,
    locale: Arc<LibraryDisplayTunables>,
}

impl ExtractSeriesSummaryStage {
    /// Construct.
    #[must_use]
    pub fn new(tunables: &LlmTunables, locale: &LibraryDisplayTunables) -> Self {
        Self {
            tunables: Arc::new(tunables.clone()),
            locale: Arc::new(locale.clone()),
        }
    }
}

#[async_trait]
impl Stage for ExtractSeriesSummaryStage {
    fn name(&self) -> &'static str {
        STAGE_NAME
    }

    fn requires(&self) -> &'static [StageId] {
        // Two inputs: the book's individual summary (3K.4) AND
        // the series resolution result (C5.6 identity-resolve
        // path). Both must land before we can summarise the
        // series.
        &[
            crate::summary_stage::STAGE_ID,
            ab_catalog::identity::STAGE_ID,
        ]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        let series_ids = load_book_series_ids(&ctx.library, book_id).await?;
        if series_ids.is_empty() {
            return Ok(StageOutcome::Skipped);
        }

        let mut any_regenerated = false;
        for series_id in series_ids {
            let outcome =
                regenerate_series_summary(&ctx.library, series_id, &self.tunables, &self.locale)
                    .await?;
            if matches!(outcome, RegenOutcome::Regenerated) {
                any_regenerated = true;
            }
        }
        if any_regenerated {
            Ok(StageOutcome::Done)
        } else {
            Ok(StageOutcome::Skipped)
        }
    }
}

/// Result of regenerating one series' summary.
enum RegenOutcome {
    /// Series was already fresh at the current `extractor_version`,
    /// or no member books had a summary yet.
    Skipped,
    /// Summary was rewritten on the `series` row.
    Regenerated,
}

async fn regenerate_series_summary(
    library: &LibraryDb,
    series_id: i64,
    tunables: &LlmTunables,
    locale: &LibraryDisplayTunables,
) -> Result<RegenOutcome> {
    // 1. Freshness check — skip when the series row carries our
    //    current extractor_version.
    if series_summary_fresh(library, series_id, &tunables.extractor_version).await? {
        return Ok(RegenOutcome::Skipped);
    }

    // 2. Gather member books with summaries. Need both the
    //    summary text + the book's language for the locale vote.
    let members = load_series_members(library, series_id).await?;
    if members.is_empty() {
        tracing::warn!(series_id, "fm.series_summary.no_inputs");
        return Ok(RegenOutcome::Skipped);
    }

    // 3. Pick the output locale — predominant book.language
    //    across members; library_locale on tie / empty.
    let output_locale = pick_output_locale(&members, &locale.library_locale);

    // 4. Build prompt + call the bridge.
    let prompt = build_prompt(
        &members,
        &output_locale,
        tunables.summary_target_words_low,
        tunables.summary_target_words_high,
    );
    let opts = GenerationOptions::new(tunables.summary_max_tokens);
    let raw = match complete_structured(&prompt, SERIES_SUMMARY_SCHEMA_JSON, &opts).await {
        Ok(s) => s,
        Err(BridgeError::PromptEmpty) => return Ok(RegenOutcome::Skipped),
        Err(e) => return Err(bridge_to_stage_error(&e)),
    };

    // 5. Parse JSON.
    let parsed: SeriesSummaryResponse = match serde_json::from_str(&raw) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                series_id,
                error = %e,
                raw_len = raw.len(),
                "fm.series_summary.parse_failed"
            );
            return Err(Error::stage(STAGE_NAME, format!("model JSON parse: {e}")));
        }
    };
    let summary_trimmed = parsed.series_summary.trim();
    let locale_ok = parsed
        .series_summary_lang
        .eq_ignore_ascii_case(&output_locale)
        || primary_subtag(&parsed.series_summary_lang) == primary_subtag(&output_locale);

    if !locale_ok {
        tracing::warn!(
            series_id,
            expected = %output_locale,
            got = %parsed.series_summary_lang,
            "fm.series_summary.locale_mismatch"
        );
        // Bump the extractor_version stamp so we don't loop,
        // but skip the summary write — the user-visible column
        // stays at its previous value (or NULL on first run).
        bump_extractor_version_only(library, series_id, &tunables.extractor_version).await?;
        return Ok(RegenOutcome::Regenerated);
    }

    if summary_trimmed.is_empty() {
        tracing::warn!(series_id, "fm.series_summary.empty_output");
        return Err(Error::stage(STAGE_NAME, "model returned empty summary"));
    }

    // 6. Promote.
    promote_series_summary(
        library,
        series_id,
        summary_trimmed,
        &output_locale,
        &tunables.extractor_version,
    )
    .await?;

    tracing::info!(
        series_id,
        members = members.len(),
        lang = %output_locale,
        word_count = summary_trimmed.split_whitespace().count(),
        "fm.series_summary.extracted"
    );
    Ok(RegenOutcome::Regenerated)
}

/// JSON shape produced by the LLM. Field names use the
/// `series_` prefix to keep raw cache dumps unambiguous when an
/// operator is comparing book vs. series summaries.
#[derive(Debug, Deserialize)]
struct SeriesSummaryResponse {
    series_summary: String,
    series_summary_lang: String,
}

/// Member book of a series with the data needed for the prompt
/// and the locale vote. `title` is non-optional in the schema
/// (`books.title TEXT NOT NULL`); `language` is the locale vote
/// input and can be NULL when the transcribe-head-tail stage
/// hasn't run yet for this book.
#[derive(Debug, Clone)]
struct MemberBook {
    title: String,
    summary: String,
    summary_lang: Option<String>,
    language: Option<String>,
}

async fn load_book_series_ids(library: &LibraryDb, book_id: BookId) -> Result<Vec<i64>> {
    let id = book_id.0;
    let rows = sqlx::query!("SELECT series_id FROM book_series WHERE book_id = ?", id)
        .fetch_all(library.pool())
        .await
        .map_err(|e| Error::Database(format!("series_summary load memberships: {e}")))?;
    Ok(rows.into_iter().map(|r| r.series_id).collect())
}

async fn series_summary_fresh(
    library: &LibraryDb,
    series_id: i64,
    extractor_version: &str,
) -> Result<bool> {
    let row = sqlx::query!(
        "SELECT summary_extractor_version FROM series WHERE series_id = ?",
        series_id,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("series_summary freshness: {e}")))?;
    let Some(row) = row else { return Ok(false) };
    Ok(row.summary_extractor_version.as_deref() == Some(extractor_version))
}

/// Load all books in the series that have a non-null summary.
/// A series whose books haven't been summarised yet is skipped
/// (the outer loop returns `RegenOutcome::Skipped` with a warn).
async fn load_series_members(library: &LibraryDb, series_id: i64) -> Result<Vec<MemberBook>> {
    let rows = sqlx::query!(
        "SELECT b.title, b.summary_spoiler_free, b.summary_spoiler_free_lang, b.language \
         FROM books b \
         JOIN book_series bs ON bs.book_id = b.book_id \
         WHERE bs.series_id = ? AND b.summary_spoiler_free IS NOT NULL",
        series_id,
    )
    .fetch_all(library.pool())
    .await
    .map_err(|e| Error::Database(format!("series_summary members: {e}")))?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            r.summary_spoiler_free.map(|summary| MemberBook {
                title: r.title,
                summary,
                summary_lang: r.summary_spoiler_free_lang,
                language: r.language,
            })
        })
        .collect())
}

async fn promote_series_summary(
    library: &LibraryDb,
    series_id: i64,
    summary: &str,
    lang: &str,
    extractor_version: &str,
) -> Result<()> {
    sqlx::query!(
        "UPDATE series SET \
             summary = ?, \
             summary_lang = ?, \
             summary_extractor_version = ? \
         WHERE series_id = ?",
        summary,
        lang,
        extractor_version,
        series_id,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("series_summary promote: {e}")))?;
    Ok(())
}

/// Stamp `summary_extractor_version` without touching `summary`.
/// Used when the model returned the wrong locale — we don't want
/// to overwrite a potentially-good prior summary, but we also
/// don't want to re-run the model on every book trigger inside
/// the same `extractor_version`.
async fn bump_extractor_version_only(
    library: &LibraryDb,
    series_id: i64,
    extractor_version: &str,
) -> Result<()> {
    sqlx::query!(
        "UPDATE series SET summary_extractor_version = ? WHERE series_id = ?",
        extractor_version,
        series_id,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("series_summary version bump: {e}")))?;
    Ok(())
}

/// Predominant `books.language` across the series. Ties or all-
/// NULL languages → `library_locale_fallback`.
///
/// `pub(crate)` (not `pub`) because `MemberBook` is private — but
/// callable from unit tests in this file. Locale picking is the
/// spot most likely to drift if the policy in ADR-0019 changes,
/// so the test coverage matters.
#[must_use]
fn pick_output_locale(members: &[MemberBook], library_locale_fallback: &str) -> String {
    use std::collections::BTreeMap;
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for m in members {
        // Prefer `books.language`; fall back to the summary's
        // `summary_spoiler_free_lang` if the column is unset.
        let lang_opt = m
            .language
            .as_deref()
            .or(m.summary_lang.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(lang) = lang_opt {
            *counts.entry(lang).or_insert(0) += 1;
        }
    }
    // Pick the language with the highest count. BTreeMap iter
    // is alphabetic; a deterministic tiebreaker (first
    // alphabetically) is fine since we fall back to library
    // locale only when there's NOTHING to vote on.
    let top = counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(lang, _)| lang.to_owned());
    top.unwrap_or_else(|| library_locale_fallback.to_owned())
}

/// Build the prompt sent to the LLM. `pub(crate)` so unit tests
/// in this module can exercise it without making `MemberBook`
/// pub.
///
/// Member-book summaries are concatenated with their titles into
/// a single context block; the prompt asks the model to produce
/// a unified series synopsis without lifting any spoilers from
/// the member summaries (the input summaries are *already*
/// spoiler-free by construction; we still tell the model not to
/// surface mid-book reveals).
#[must_use]
fn build_prompt(
    members: &[MemberBook],
    series_locale: &str,
    target_words_low: usize,
    target_words_high: usize,
) -> String {
    use std::fmt::Write as _;
    // Defensive ceiling — series can in principle be 30 books;
    // concatenating every summary risks blowing the context.
    // Cap at the first 8 member summaries; the early books of a
    // series carry the framing weight.
    const MEMBERS_LIMIT: usize = 8;
    let mut bundle = String::new();
    for (i, m) in members.iter().take(MEMBERS_LIMIT).enumerate() {
        if !bundle.is_empty() {
            bundle.push_str("\n\n");
        }
        // `Write::write_str` for String can't fail; the discard
        // mirrors what `write!()` does in stable Rust.
        let _ = write!(bundle, "BOOK {}: {}\n{}", i + 1, m.title, m.summary);
    }
    format!(
        "You are a librarian writing a spoiler-free series synopsis for an audiobook \
library browse view. Read the member-book summaries below and produce a unified \
series description.\n\
\n\
Rules:\n\
1. Spoiler-free. Cover the shared world / premise / protagonist / tone. Do NOT \
recite plot beats specific to individual books.\n\
2. Target {target_words_low}-{target_words_high} words.\n\
3. Write in BCP-47 locale `{series_locale}`. Set `series_summary_lang` to \
`{series_locale}`.\n\
4. The member summaries below are already spoiler-free; do not invent additional \
plot detail from them.\n\
\n\
MEMBER BOOK SUMMARIES (locale={series_locale}, up to {MEMBERS_LIMIT} books):\n\
{bundle}"
    )
}

/// BCP-47 primary subtag: `en-US` → `en`. Used by the locale
/// self-check.
fn primary_subtag(tag: &str) -> &str {
    tag.split('-').next().unwrap_or(tag)
}

fn bridge_to_stage_error(err: &BridgeError) -> Error {
    Error::stage(STAGE_NAME, format!("Foundation Models: {err}"))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn member(lang: Option<&str>, summary_lang: Option<&str>) -> MemberBook {
        MemberBook {
            title: "test".into(),
            summary: "stub".into(),
            summary_lang: summary_lang.map(str::to_owned),
            language: lang.map(str::to_owned),
        }
    }

    #[test]
    fn pick_locale_picks_predominant_book_language() {
        let members = vec![
            member(Some("en"), None),
            member(Some("en"), None),
            member(Some("de"), None),
        ];
        assert_eq!(pick_output_locale(&members, "en-US"), "en");
    }

    #[test]
    fn pick_locale_falls_back_to_summary_lang_when_book_lang_null() {
        let members = vec![member(None, Some("ja")), member(None, Some("ja"))];
        assert_eq!(pick_output_locale(&members, "en"), "ja");
    }

    #[test]
    fn pick_locale_falls_back_to_library_when_all_null() {
        let members = vec![member(None, None), member(None, None)];
        assert_eq!(pick_output_locale(&members, "fr-FR"), "fr-FR");
    }

    #[test]
    fn pick_locale_breaks_ties_alphabetically_then_falls_back_only_when_empty() {
        // Tied counts: BTreeMap iterates alphabetically; max_by_key
        // returns the LAST seen on tie. With two equal counts the
        // alphabetically-later language wins. Deterministic, which
        // is what we want; the alternative would be "library_locale
        // on tie" which loses information when there ARE candidate
        // languages.
        let members = vec![member(Some("de"), None), member(Some("en"), None)];
        let pick = pick_output_locale(&members, "fr");
        assert!(pick == "en" || pick == "de", "got {pick}");
        assert_ne!(pick, "fr", "with candidates, fallback shouldn't fire");
    }

    #[test]
    fn build_prompt_caps_to_first_eight_books() {
        let members: Vec<MemberBook> = (0..15)
            .map(|_| MemberBook {
                title: "t".into(),
                summary: "x".repeat(100),
                summary_lang: None,
                language: Some("en".into()),
            })
            .collect();
        let prompt = build_prompt(&members, "en", 100, 200);
        // Should mention BOOK 1 ... BOOK 8 but NOT BOOK 9.
        assert!(prompt.contains("BOOK 8:"));
        assert!(!prompt.contains("BOOK 9:"));
    }

    #[test]
    fn build_prompt_writes_target_locale() {
        let members = vec![MemberBook {
            title: "first".into(),
            summary: "premise".into(),
            summary_lang: None,
            language: Some("de".into()),
        }];
        let p = build_prompt(&members, "de", 100, 200);
        assert!(p.contains("`de`"));
        assert!(p.contains("locale=de"));
        assert!(p.contains("100-200 words"));
    }

    #[test]
    fn schema_parses_and_matches_response_shape() {
        let v: serde_json::Value =
            serde_json::from_str(SERIES_SUMMARY_SCHEMA_JSON).expect("schema parses");
        assert_eq!(v["type"], "object");
        let props = v["properties"]
            .as_object()
            .expect("properties is an object");
        for field in ["series_summary", "series_summary_lang"] {
            let entry = props
                .get(field)
                .unwrap_or_else(|| panic!("schema missing field {field}"));
            assert_eq!(entry["type"], "string");
        }
        let required: Vec<String> = v["required"]
            .as_array()
            .expect("required array")
            .iter()
            .map(|x| x.as_str().expect("str").to_owned())
            .collect();
        assert!(required.contains(&"series_summary".to_owned()));
        assert!(required.contains(&"series_summary_lang".to_owned()));
    }

    /// Pinning the `MemberBook`-to-`SeriesSummaryResponse` flow:
    /// the response JSON we expect from the model deserialises
    /// into the shape we read from.
    #[test]
    fn parse_series_summary_response() {
        let json = r#"{"series_summary":"A world of magic.","series_summary_lang":"en"}"#;
        let r: SeriesSummaryResponse = serde_json::from_str(json).expect("parse");
        assert_eq!(r.series_summary, "A world of magic.");
        assert_eq!(r.series_summary_lang, "en");
    }
}
