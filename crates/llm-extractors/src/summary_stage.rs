//! `extract-summary-spoiler-free` pipeline stage (slice 3K.4).
//!
//! For each book this stage:
//!
//! 1. Loads the cached `transcript_full` row from `ai_cache`
//!    (written by `transcribe-full`). Skips if absent.
//! 2. Reads `books.language` — the output locale. **Per project
//!    policy, summaries stay in the book's native language, not
//!    in `library_locale`.** A German book gets a German summary
//!    even for a user who set `library_locale=en`. Library locale
//!    is reserved for genre vocabulary; description / title /
//!    summary stay in `books.language`.
//! 3. Builds a prompt asking the on-device LLM for a spoiler-
//!    free summary, with the configured word range and the
//!    book's language as the target. The schema constraint
//!    (via [`ab_foundation_models::complete_structured`]) forces
//!    the model to emit `{summary, summary_lang}` shape.
//! 4. Self-checks the response: `summary_lang` must match
//!    `books.language` (case-insensitive); on mismatch, log a
//!    warning and skip promotion (don't write a wrong-locale
//!    value into the user-visible column).
//! 5. Writes the summary into `books.summary_spoiler_free` +
//!    `books.summary_spoiler_free_lang` (both columns landed in
//!    migration 003 and were unfilled until this slice).
//! 6. Caches the raw response in `ai_cache` keyed
//!    `(book_id, "summary_spoiler_free")` with `locale = books.language`
//!    and the current `extractor_version` for idempotency.
//!
//! ## Idempotency
//!
//! Skip when an `ai_cache` row exists at the current
//! `extractor_version`. Bump `LlmTunables::extractor_version` to
//! force re-extract library-wide.
//!
//! ## Failure modes
//!
//! - No cached full transcript → `Skipped` (waits for
//!   `transcribe-full` to land first).
//! - Transcript empty / below sanity floor → `Skipped`.
//! - `books.language` is NULL → `Skipped` (the
//!   `transcribe-head-tail` stage seeds it; we don't infer here).
//! - Foundation Models unavailable → `Err` (user-fixable, surfaced
//!   by `aborg doctor llm`).
//! - Model returned wrong-locale summary → log warn + skip
//!   promotion (the cache still records the response so a
//!   diagnostic dump shows what happened).
//! - Schema parse failure → `Err`. Should be impossible at
//!   runtime since the schema is a compile-time constant.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use ab_core::tunables::LlmTunables;
use ab_core::{BookId, CacheKey, Error, Result};
use ab_db::LibraryDb;
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};

use ab_foundation_models::{BridgeError, GenerationOptions, complete_structured};

/// Typed identifier for this stage.
pub const STAGE_ID: StageId = StageId::new("extract-summary-spoiler-free");

/// Stage name written to `pipeline_progress`. Derives from `STAGE_ID`.
pub const STAGE_NAME: &str = STAGE_ID.as_str();

/// JSON Schema passed to `complete_structured`.
///
/// Constrains the model's output at decode time. Maps to a
/// `DynamicGenerationSchema` on the Swift side; matches the
/// `SummaryResponse` Rust shape one-to-one so any drift surfaces
/// as a parse failure in the schema-parity test.
pub const SUMMARY_SCHEMA_JSON: &str = r#"{
    "type": "object",
    "properties": {
        "summary": {"type": "string"},
        "summary_lang": {"type": "string"}
    },
    "required": ["summary", "summary_lang"]
}"#;

/// Stage that asks the on-device LLM for a spoiler-free book
/// summary in the book's native language.
pub struct ExtractSummaryStage {
    tunables: Arc<LlmTunables>,
}

impl ExtractSummaryStage {
    /// Construct.
    #[must_use]
    pub fn new(tunables: &LlmTunables) -> Self {
        Self {
            tunables: Arc::new(tunables.clone()),
        }
    }
}

#[async_trait]
impl Stage for ExtractSummaryStage {
    fn name(&self) -> &'static str {
        STAGE_NAME
    }

    fn requires(&self) -> &'static [StageId] {
        // Full transcript is the input — wait for it, not for
        // head+samples. The user policy is "summarise from the
        // whole book" so we do not run earlier.
        &[ab_transcript::full_stage::STAGE_ID]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        // 1. Idempotency.
        if summary_cache_fresh(&ctx.library, book_id, &self.tunables.extractor_version).await? {
            return Ok(StageOutcome::Skipped);
        }

        // 2. Resolve the book's language. Without it we can't
        //    instruct the model what locale to write in. The
        //    transcribe-head-tail stage seeds books.language;
        //    if it's still NULL the pipeline isn't ready.
        let Some(book_lang) = load_book_language(&ctx.library, book_id).await? else {
            return Ok(StageOutcome::Skipped);
        };

        // 3. Load full transcript.
        let Some(transcript) = load_full_transcript(&ctx.library, book_id).await? else {
            return Ok(StageOutcome::Skipped);
        };
        if transcript.trim().len() < 200 {
            // Defensive floor — see DNA stage for rationale.
            return Ok(StageOutcome::Skipped);
        }

        // 4. Build prompt + call the bridge.
        let prompt = build_prompt(
            &transcript,
            &book_lang,
            self.tunables.summary_target_words_low,
            self.tunables.summary_target_words_high,
        );
        let opts = GenerationOptions::new(self.tunables.summary_max_tokens);
        let raw = match complete_structured(&prompt, SUMMARY_SCHEMA_JSON, &opts).await {
            Ok(s) => s,
            Err(BridgeError::PromptEmpty) => return Ok(StageOutcome::Skipped),
            Err(e) => return Err(bridge_to_stage_error(&e)),
        };

        // 5. Parse JSON.
        let parsed: SummaryResponse = match serde_json::from_str(&raw) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    book_id = book_id.0,
                    error = %e,
                    raw_len = raw.len(),
                    "fm.summary.parse_failed"
                );
                return Err(Error::stage(STAGE_NAME, format!("model JSON parse: {e}")));
            }
        };

        // 6. Locale self-check. Schema-constrained generation
        //    enforces the *shape* but not the value semantics;
        //    the model can still emit `{"summary": "...", "summary_lang": "en"}`
        //    when we asked for German. Skip promotion on mismatch
        //    but still record the cache so a diagnostic dump
        //    surfaces the bad output.
        let summary_trimmed = parsed.summary.trim();
        let locale_ok = parsed.summary_lang.eq_ignore_ascii_case(&book_lang)
            // Permit shorthand: model returns "en" when book is "en-US",
            // or "de" when book is "de-DE". Match on the primary subtag.
            || primary_subtag(&parsed.summary_lang) == primary_subtag(&book_lang);
        if !locale_ok {
            tracing::warn!(
                book_id = book_id.0,
                expected = %book_lang,
                got = %parsed.summary_lang,
                "fm.summary.locale_mismatch"
            );
            // Still cache (so re-runs at the same extractor_version
            // are no-ops), but skip the promotion to books.
            write_cache(
                &ctx.library,
                book_id,
                &raw,
                &book_lang,
                &self.tunables.extractor_version,
            )
            .await?;
            return Ok(StageOutcome::Skipped);
        }

        if summary_trimmed.is_empty() {
            tracing::warn!(book_id = book_id.0, "fm.summary.empty_output");
            return Err(Error::stage(STAGE_NAME, "model returned empty summary"));
        }

        // 7. Write to books + cache.
        promote_summary(&ctx.library, book_id, summary_trimmed, &book_lang).await?;
        write_cache(
            &ctx.library,
            book_id,
            &raw,
            &book_lang,
            &self.tunables.extractor_version,
        )
        .await?;

        tracing::info!(
            book_id = book_id.0,
            lang = %book_lang,
            word_count = summary_trimmed.split_whitespace().count(),
            "fm.summary.extracted"
        );
        Ok(StageOutcome::Done)
    }
}

/// JSON shape produced by the LLM. `summary_lang` is a self-check
/// — the prompt asks for the book's BCP-47 tag and the locale-
/// mismatch path above rejects bad outputs.
#[derive(Debug, Deserialize)]
struct SummaryResponse {
    summary: String,
    summary_lang: String,
}

/// Read the book's native language (BCP-47). NULL → not ready
/// — the transcribe-head-tail stage seeds this column from the
/// detected speech locale, falling back to tag metadata.
async fn load_book_language(library: &LibraryDb, book_id: BookId) -> Result<Option<String>> {
    let id = book_id.0;
    let row = sqlx::query!("SELECT language FROM books WHERE book_id = ?", id)
        .fetch_optional(library.pool())
        .await
        .map_err(|e| Error::Database(format!("summary load lang: {e}")))?;
    let Some(row) = row else { return Ok(None) };
    let trimmed = row
        .language
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    Ok(trimmed.map(str::to_owned))
}

/// Segment array from the cached `transcript_full` row. Same
/// shape as DNA stage's `CachedTranscript` (could live in a
/// shared module if a third extractor needs it — keep duplicated
/// while only two callers exist).
#[derive(Debug, Deserialize)]
struct CachedTranscript {
    segments: Vec<Segment>,
}

#[derive(Debug, Deserialize)]
struct Segment {
    text: String,
}

/// Load the `transcript_full` cache row and concatenate the
/// segment text. Returns `None` when there's no row, the content
/// is missing, or the JSON shape is wrong.
async fn load_full_transcript(library: &LibraryDb, book_id: BookId) -> Result<Option<String>> {
    let id = book_id.0;
    let full_cache = CacheKey::TranscriptFull.as_str();
    let row = sqlx::query!(
        "SELECT content FROM ai_cache WHERE book_id = ? AND cache_type = ?",
        id,
        full_cache,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("summary load transcript_full: {e}")))?;
    let Some(row) = row else { return Ok(None) };
    let Some(bytes) = row.content else {
        return Ok(None);
    };
    let cached: CachedTranscript = match serde_json::from_slice(&bytes) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(book_id = id, error = %e, "fm.summary.transcript_unparseable");
            return Ok(None);
        }
    };
    let mut text = String::with_capacity(cached.segments.iter().map(|s| s.text.len() + 1).sum());
    for seg in &cached.segments {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&seg.text);
    }
    Ok(Some(text))
}

async fn summary_cache_fresh(
    library: &LibraryDb,
    book_id: BookId,
    extractor_version: &str,
) -> Result<bool> {
    let id = book_id.0;
    let summary_cache = CacheKey::SummarySpoilerFree.as_str();
    let row = sqlx::query!(
        "SELECT extractor_version FROM ai_cache WHERE book_id = ? AND cache_type = ?",
        id,
        summary_cache,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("summary cache lookup: {e}")))?;
    let Some(row) = row else { return Ok(false) };
    Ok(row.extractor_version.as_deref() == Some(extractor_version))
}

async fn promote_summary(
    library: &LibraryDb,
    book_id: BookId,
    summary: &str,
    lang: &str,
) -> Result<()> {
    let id = book_id.0;
    sqlx::query!(
        "UPDATE books SET summary_spoiler_free = ?, summary_spoiler_free_lang = ? \
         WHERE book_id = ?",
        summary,
        lang,
        id,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("summary promote: {e}")))?;
    Ok(())
}

#[derive(Debug, Serialize)]
struct CachePayload<'a> {
    raw: &'a str,
}

async fn write_cache(
    library: &LibraryDb,
    book_id: BookId,
    raw: &str,
    locale: &str,
    extractor_version: &str,
) -> Result<()> {
    let id = book_id.0;
    let payload = CachePayload { raw };
    let bytes = serde_json::to_vec(&payload)
        .map_err(|e| Error::stage(STAGE_NAME, format!("encode cache: {e}")))?;
    let summary_cache = CacheKey::SummarySpoilerFree.as_str();
    sqlx::query!(
        "INSERT OR REPLACE INTO ai_cache \
         (book_id, cache_type, content, compressed, extractor_version, locale) \
         VALUES (?, ?, ?, 0, ?, ?)",
        id,
        summary_cache,
        bytes,
        extractor_version,
        locale,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("summary cache write: {e}")))?;
    Ok(())
}

/// Build the prompt sent to the LLM. Public for unit-testing the
/// content (caps, locale instruction, no-spoiler rules).
///
/// `transcript` is the concatenated `transcript_full` segments;
/// truncated defensively at 30k chars (matches DNA stage's
/// budget — past that the model's context cost rises faster than
/// the marginal summary signal). The user's design directive is
/// "feed the full transcript"; in practice the on-device model's
/// context window is the real ceiling.
#[must_use]
pub fn build_prompt(
    transcript: &str,
    book_locale: &str,
    target_words_low: usize,
    target_words_high: usize,
) -> String {
    const TRANSCRIPT_LIMIT: usize = 30_000;
    let excerpt = if transcript.len() > TRANSCRIPT_LIMIT {
        let mut end = TRANSCRIPT_LIMIT;
        while end > 0 && !transcript.is_char_boundary(end) {
            end -= 1;
        }
        &transcript[..end]
    } else {
        transcript
    };
    // Schema shape (`summary`, `summary_lang`) is conveyed to the
    // model by complete_structured with includeSchemaInPrompt:
    // true; we don't restate it here. What stays in the prompt:
    // content guidance the schema can't express (spoiler rules,
    // length range, target locale, what to cover).
    format!(
        "You are a librarian writing spoiler-free summaries for an audiobook \
library browse view. Read the TRANSCRIPT below and produce a brief summary \
suitable for a reader who has not started the book.\n\
\n\
Rules:\n\
1. Spoiler-free. No plot twists, character deaths, romance outcomes, or \
ending revelations. No information about events past the first quarter of \
the book.\n\
2. Cover premise + setting + protagonist motivation + tone in {target_words_low}-{target_words_high} words.\n\
3. Write in the book's native language. The book's BCP-47 locale is \
`{book_locale}`. Your `summary` field MUST be in that language. \
Set `summary_lang` to `{book_locale}` to confirm.\n\
4. The TRANSCRIPT may include mid-book passages; ignore any plot beats \
that fall outside the first-quarter rule above.\n\
\n\
TRANSCRIPT (locale={book_locale}):\n\
{excerpt}"
    )
}

/// BCP-47 primary subtag: `en-US` → `en`, `zh-Hans-CN` → `zh`.
/// Used to permit "en" ↔ "en-US" matches in the locale self-check.
fn primary_subtag(tag: &str) -> &str {
    tag.split('-').next().unwrap_or(tag)
}

/// Map a bridge error into a Stage error.
fn bridge_to_stage_error(err: &BridgeError) -> Error {
    Error::stage(STAGE_NAME, format!("Foundation Models: {err}"))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn build_prompt_includes_locale_caps_and_transcript() {
        let p = build_prompt("Once upon a time…", "de", 100, 150);
        assert!(p.contains("`de`"), "BCP-47 tag must appear in the prompt");
        assert!(p.contains("100-150 words"));
        assert!(p.contains("Once upon a time"));
        assert!(p.contains("locale=de"));
    }

    #[test]
    fn build_prompt_truncates_long_transcript() {
        let long = "x".repeat(40_000);
        let p = build_prompt(&long, "en", 100, 150);
        assert!(p.len() < 32_000, "prompt len was {}", p.len());
    }

    #[test]
    fn primary_subtag_normalises_bcp47() {
        assert_eq!(primary_subtag("en"), "en");
        assert_eq!(primary_subtag("en-US"), "en");
        assert_eq!(primary_subtag("zh-Hans-CN"), "zh");
        assert_eq!(primary_subtag(""), "");
    }

    #[test]
    fn parse_summary_response() {
        let json = r#"{"summary":"A brief tale.","summary_lang":"en"}"#;
        let r: SummaryResponse = serde_json::from_str(json).expect("parse");
        assert_eq!(r.summary, "A brief tale.");
        assert_eq!(r.summary_lang, "en");
    }

    /// `SUMMARY_SCHEMA_JSON` is the JSON Schema the framework
    /// constrains the model to. Verify it parses as JSON and
    /// names exactly the fields the `SummaryResponse` deserialiser
    /// reads. Catches drift between the schema and the Rust
    /// shape. Same pattern as the DNA stage's parity test.
    #[test]
    fn schema_parses_and_matches_response_shape() {
        let v: serde_json::Value =
            serde_json::from_str(SUMMARY_SCHEMA_JSON).expect("schema parses");
        assert_eq!(v["type"], "object");
        let props = v["properties"]
            .as_object()
            .expect("properties is an object");
        for field in ["summary", "summary_lang"] {
            let entry = props
                .get(field)
                .unwrap_or_else(|| panic!("schema missing field {field}"));
            assert_eq!(
                entry["type"], "string",
                "{field} must be `type: string` in schema",
            );
        }
        let required = v["required"]
            .as_array()
            .expect("required is an array")
            .iter()
            .map(|x| x.as_str().expect("required entry is string").to_owned())
            .collect::<Vec<_>>();
        assert!(required.contains(&"summary".to_owned()));
        assert!(required.contains(&"summary_lang".to_owned()));
    }
}
