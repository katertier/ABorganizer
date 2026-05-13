//! `extract-setting` pipeline stage (slice 3K.8).
//!
//! Two-output extractor: produces a one-paragraph setting
//! summary for `books.setting` PLUS a list of `$`-prefixed
//! tags spanning 10 categories that land in `book_tags` with
//! `source='setting_llm'` (ADR-0021 + ADR-0022).
//!
//! Per ADR-0022 the ten categories are:
//!
//! | Category | Answers | Example |
//! |---|---|---|
//! | `magic` | "what magic system" | `$magic-hard`, `$magic-soft` |
//! | `tech` | "what tech level" | `$tech-cyberpunk`, `$tech-steampunk` |
//! | `tone` | "how does it feel" | `$tone-grimdark`, `$tone-cosy` |
//! | `pace` | "tempo" | `$pace-slow-burn`, `$pace-breakneck` |
//! | `theme` | "what's it about" | `$theme-found-family`, `$theme-revenge` |
//! | `era` | "when" | `$era-victorian`, `$era-post-apocalyptic` |
//! | `world` | "what kind of world" | `$world-urban`, `$world-fantasy-medieval` |
//! | `location` | "where specifically" | `$location-london`, `$location-mars` |
//! | `race` | "collective identities" | `$race-elves`, `$race-martians` |
//! | `group` | "factions" | `$group-imperial-navy`, `$group-the-rebels` |
//!
//! `$world` vs `$location` is the boundary the prompt
//! explicitly disambiguates (archetype vs. specific named
//! place); see ADR-0022 § "World-archetype vs. specific
//! location" for the policy. Per-character `species` on the
//! `characters` table coexists with book-level `$race-*` per
//! the same ADR.
//!
//! ## Idempotency
//!
//! Skip when an `ai_cache` row exists at the current
//! `extractor_version` for `CacheKey::Setting`. Bump
//! `LlmTunables::extractor_version` to force re-extract
//! library-wide.
//!
//! ## Self-checks
//!
//! - Locale (`setting_lang` primary-subtag matches
//!   `books.language`) — warn + skip promotion on mismatch.
//! - Tag normalisation: every `$`-prefixed tag from the model
//!   is run through [`normalise_tag`] (slugified) before
//!   landing in `book_tags`. Tags without the `$` prefix are
//!   dropped with a tracing warning; bare-prefix output is a
//!   model bug, not user-facing data.
//! - Tag cap: defensively truncated at `setting_max_tags`
//!   after normalisation + dedup.
//!
//! ## Spoiler handling
//!
//! Setting tags are NEVER spoiler-gated. They describe the
//! world, not the plot. No `!`-prefixed output here.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use ab_core::tags::{TAG_PREFIX_SETTING, TagKind};
use ab_core::tunables::LlmTunables;
use ab_core::{BookId, CacheKey, Error, Result};
use ab_db::LibraryDb;
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};

use ab_foundation_models::{BridgeError, complete_structured};

/// Typed identifier for this stage.
pub const STAGE_ID: StageId = StageId::new("extract-setting");

/// Stage name written to `pipeline_progress`. Derives from `STAGE_ID`.
pub const STAGE_NAME: &str = STAGE_ID.as_str();

/// `book_tags.source` for rows produced by this stage. Lets
/// the per-stage cleanup query target only setting tags
/// without disturbing DNA / genre rows for the same book.
pub const TAG_SOURCE_SETTING_LLM: &str = "setting_llm";

/// JSON Schema passed to `complete_structured`.
///
/// `setting_tags` is a flat array of strings — categorisation
/// is baked into the body convention (`<category>-<value>`),
/// not into the schema structure. Keeping it flat means
/// `book_tags` storage stays uniform across all four prefix
/// classes (genre / DNA / spoiler / setting) and downstream
/// filtering treats setting tags identically.
pub const SETTING_SCHEMA_JSON: &str = r#"{
    "type": "object",
    "properties": {
        "setting": {"type": "string"},
        "setting_lang": {"type": "string"},
        "setting_tags": {
            "type": "array",
            "items": {"type": "string"}
        }
    },
    "required": ["setting", "setting_lang", "setting_tags"]
}"#;

/// Stage that asks the on-device LLM for a setting paragraph +
/// `$`-prefixed tags spanning 10 categories.
pub struct ExtractSettingStage {
    tunables: Arc<LlmTunables>,
}

impl ExtractSettingStage {
    /// Construct.
    #[must_use]
    pub fn new(tunables: &LlmTunables) -> Self {
        Self {
            tunables: Arc::new(tunables.clone()),
        }
    }
}

#[async_trait]
impl Stage for ExtractSettingStage {
    fn name(&self) -> &'static str {
        STAGE_NAME
    }

    fn requires(&self) -> &'static [StageId] {
        &[
            ab_transcript::full_stage::STAGE_ID,
            crate::summary_stage::STAGE_ID,
        ]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        // 1. Idempotency.
        if setting_cache_fresh(&ctx.library, book_id, &self.tunables.extractor_version).await? {
            return Ok(StageOutcome::Skipped);
        }

        // 2. Inputs.
        let Some(book_lang) = load_book_language(&ctx.library, book_id).await? else {
            return Ok(StageOutcome::Skipped);
        };
        let Some(transcript) = load_full_transcript(&ctx.library, book_id).await? else {
            return Ok(StageOutcome::Skipped);
        };
        if transcript.trim().len() < 200 {
            return Ok(StageOutcome::Skipped);
        }

        // 3. Call the bridge.
        let prompt = build_prompt(
            &transcript,
            &book_lang,
            PromptShape {
                paragraph_words_low: self.tunables.setting_target_words_low,
                paragraph_words_high: self.tunables.setting_target_words_high,
                max_tags: self.tunables.setting_max_tags,
            },
        );
        let raw = match complete_structured(
            &prompt,
            SETTING_SCHEMA_JSON,
            self.tunables.setting_max_tokens,
        )
        .await
        {
            Ok(s) => s,
            Err(BridgeError::PromptEmpty) => return Ok(StageOutcome::Skipped),
            Err(e) => return Err(bridge_to_stage_error(&e)),
        };

        // 4. Parse + locale-check.
        let parsed = parse_setting(&raw, book_id)?;
        if !validate_locale(&parsed, &book_lang, book_id) {
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

        // 5. Paragraph sanity floor.
        let paragraph = parsed.setting.trim();
        if paragraph.is_empty() {
            tracing::warn!(book_id = book_id.0, "fm.setting.empty_paragraph");
            return Err(Error::stage(STAGE_NAME, "model returned empty setting"));
        }

        // 6. Normalise + dedup tags. Anything without the `$`
        //    prefix is a model bug; warn + drop. Tags that
        //    normalise to empty after slugification are also
        //    dropped.
        let tags = normalise_tags(
            &parsed.setting_tags,
            book_id,
            self.tunables.setting_max_tags,
        );

        // 7. Promote: paragraph to `books`, tags to `book_tags`.
        promote_setting(
            &ctx.library,
            book_id,
            SettingPromotion {
                paragraph,
                lang: &book_lang,
                extractor_version: &self.tunables.extractor_version,
                tags: &tags,
            },
        )
        .await?;
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
            paragraph_words = paragraph.split_whitespace().count(),
            tags = tags.len(),
            "fm.setting.extracted"
        );
        Ok(StageOutcome::Done)
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct SettingResponse {
    setting: String,
    setting_lang: String,
    setting_tags: Vec<String>,
}

/// Shape parameters for [`build_prompt`].
#[derive(Debug, Clone, Copy)]
pub struct PromptShape {
    /// Target floor for paragraph word count.
    pub paragraph_words_low: usize,
    /// Target cap for paragraph word count.
    pub paragraph_words_high: usize,
    /// Soft cap for total `$`-prefixed tags.
    pub max_tags: usize,
}

fn parse_setting(raw: &str, book_id: BookId) -> Result<SettingResponse> {
    match serde_json::from_str::<SettingResponse>(raw) {
        Ok(p) => Ok(p),
        Err(e) => {
            tracing::warn!(
                book_id = book_id.0,
                error = %e,
                raw_len = raw.len(),
                "fm.setting.parse_failed"
            );
            Err(Error::stage(STAGE_NAME, format!("model JSON parse: {e}")))
        }
    }
}

fn validate_locale(parsed: &SettingResponse, book_lang: &str, book_id: BookId) -> bool {
    let locale_ok = parsed.setting_lang.eq_ignore_ascii_case(book_lang)
        || primary_subtag(&parsed.setting_lang) == primary_subtag(book_lang);
    if !locale_ok {
        tracing::warn!(
            book_id = book_id.0,
            expected = %book_lang,
            got = %parsed.setting_lang,
            "fm.setting.locale_mismatch"
        );
        return false;
    }
    true
}

/// Normalise `$`-prefixed tags: strip the prefix, slugify the
/// body, dedup, cap. Tags without the `$` prefix are warned +
/// dropped (bare-prefix output is a model bug). Returns the
/// re-prefixed strings ready for `book_tags`.
fn normalise_tags(raw: &[String], book_id: BookId, max_tags: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(raw.len().min(max_tags));
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for tag in raw {
        let trimmed = tag.trim();
        let Some(body) = trimmed.strip_prefix(TAG_PREFIX_SETTING) else {
            tracing::warn!(
                book_id = book_id.0,
                tag = %trimmed,
                "fm.setting.tag_missing_prefix"
            );
            continue;
        };
        let slug = normalise_setting_body(body);
        if slug.is_empty() {
            continue;
        }
        if !seen.insert(slug.clone()) {
            continue;
        }
        out.push(TagKind::Setting.format_tag(&slug));
        if out.len() >= max_tags {
            break;
        }
    }
    out
}

/// Slugify a tag body. Same rules as the DNA stage's
/// `normalise_tag` but without the prefix concerns:
/// - lowercase
/// - whitespace + underscores → `-`
/// - drop any character that isn't `a-z 0-9 -`
/// - collapse runs of `-`
/// - trim leading/trailing `-`
///
/// Keeps the `<category>-<value>` shape intact because the
/// hyphen inside (e.g. `world-fantasy-medieval`) survives the
/// slug pass — the bound-by-bound conversion never substitutes
/// hyphens.
#[must_use]
pub fn normalise_setting_body(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut prev_dash = false;
    for ch in body.to_lowercase().chars() {
        let mapped = match ch {
            'a'..='z' | '0'..='9' => Some(ch),
            ' ' | '\t' | '_' | '-' => Some('-'),
            _ => None,
        };
        match mapped {
            Some('-') if prev_dash || out.is_empty() => { /* skip leading/duplicate dashes */ }
            Some('-') => {
                out.push('-');
                prev_dash = true;
            }
            Some(c) => {
                out.push(c);
                prev_dash = false;
            }
            None => { /* drop unrepresentable char */ }
        }
    }
    if out.ends_with('-') {
        out.pop();
    }
    out
}

async fn load_book_language(library: &LibraryDb, book_id: BookId) -> Result<Option<String>> {
    let id = book_id.0;
    let row = sqlx::query!("SELECT language FROM books WHERE book_id = ?", id)
        .fetch_optional(library.pool())
        .await
        .map_err(|e| Error::Database(format!("setting load lang: {e}")))?;
    let Some(row) = row else { return Ok(None) };
    let trimmed = row
        .language
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    Ok(trimmed.map(str::to_owned))
}

#[derive(Debug, Deserialize)]
struct CachedTranscript {
    segments: Vec<Segment>,
}

#[derive(Debug, Deserialize)]
struct Segment {
    text: String,
}

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
    .map_err(|e| Error::Database(format!("setting load transcript_full: {e}")))?;
    let Some(row) = row else { return Ok(None) };
    let Some(bytes) = row.content else {
        return Ok(None);
    };
    let cached: CachedTranscript = match serde_json::from_slice(&bytes) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(book_id = id, error = %e, "fm.setting.transcript_unparseable");
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

async fn setting_cache_fresh(
    library: &LibraryDb,
    book_id: BookId,
    extractor_version: &str,
) -> Result<bool> {
    let id = book_id.0;
    let cache = CacheKey::Setting.as_str();
    let row = sqlx::query!(
        "SELECT extractor_version FROM ai_cache WHERE book_id = ? AND cache_type = ?",
        id,
        cache,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("setting cache lookup: {e}")))?;
    let Some(row) = row else { return Ok(false) };
    Ok(row.extractor_version.as_deref() == Some(extractor_version))
}

/// Bundle of values written by [`promote_setting`]. Bundled
/// into a struct so the function stays under the workspace
/// 5-arg ceiling.
struct SettingPromotion<'a> {
    paragraph: &'a str,
    lang: &'a str,
    extractor_version: &'a str,
    tags: &'a [String],
}

/// Promote: update `books.setting` + `_lang` +
/// `_extractor_version`, and replace this book's
/// `setting_llm`-sourced `book_tags` rows.
async fn promote_setting(
    library: &LibraryDb,
    book_id: BookId,
    p: SettingPromotion<'_>,
) -> Result<()> {
    let id = book_id.0;
    let mut tx = library
        .pool()
        .begin()
        .await
        .map_err(|e| Error::Database(format!("setting begin tx: {e}")))?;

    sqlx::query!(
        "UPDATE books \
         SET setting = ?, setting_lang = ?, setting_extractor_version = ? \
         WHERE book_id = ?",
        p.paragraph,
        p.lang,
        p.extractor_version,
        id,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| Error::Database(format!("setting promote books: {e}")))?;

    sqlx::query!(
        "DELETE FROM book_tags WHERE book_id = ? AND source = ?",
        id,
        TAG_SOURCE_SETTING_LLM,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| Error::Database(format!("setting clear book_tags: {e}")))?;

    for tag in p.tags {
        sqlx::query!(
            "INSERT OR IGNORE INTO book_tags (book_id, tag, source) VALUES (?, ?, ?)",
            id,
            tag,
            TAG_SOURCE_SETTING_LLM,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| Error::Database(format!("setting insert book_tag: {e}")))?;
    }

    tx.commit()
        .await
        .map_err(|e| Error::Database(format!("setting commit: {e}")))?;
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
    let cache = CacheKey::Setting.as_str();
    sqlx::query!(
        "INSERT OR REPLACE INTO ai_cache \
         (book_id, cache_type, content, compressed, extractor_version, locale) \
         VALUES (?, ?, ?, 0, ?, ?)",
        id,
        cache,
        bytes,
        extractor_version,
        locale,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("setting cache write: {e}")))?;
    Ok(())
}

/// Build the prompt sent to the LLM. Public for unit-testing
/// the content rules (10-category coverage,
/// `$world` vs `$location` disambiguation, no-spoiler rule for
/// the paragraph).
#[must_use]
pub fn build_prompt(transcript: &str, book_locale: &str, shape: PromptShape) -> String {
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
    let PromptShape {
        paragraph_words_low,
        paragraph_words_high,
        max_tags,
    } = shape;
    format!(
        "You are a worldbuilding analyst writing a SETTING profile for an \
audiobook library browse view. Read the TRANSCRIPT below and produce a \
short paragraph plus a list of structured setting tags.\n\
\n\
Rules for the `setting` paragraph:\n\
1. {paragraph_words_low}-{paragraph_words_high} words describing the world \
the book takes place in (era, place, atmosphere, technology / magic level, \
notable peoples or factions).\n\
2. WORLD only. No plot beats, no character deaths, no twist reveals — this \
is what the world feels like, not what happens in it.\n\
3. Write in the book's native language. The book's BCP-47 locale is \
`{book_locale}`. Set `setting_lang` to `{book_locale}` to confirm.\n\
\n\
Rules for `setting_tags` (array of `$<category>-<value>` strings, at most \
{max_tags} entries total across all categories):\n\
\n\
- Every tag MUST start with `$`. Every tag uses the form \
`$<category>-<value>` with lowercase + hyphens (e.g. \
`$world-fantasy-medieval`, `$location-london`).\n\
- The 10 categories are:\n\
  - `magic`: magic system flavour. `$magic-hard`, `$magic-soft`, \
`$magic-elemental`. Omit for books without magic.\n\
  - `tech`: technology level. `$tech-cyberpunk`, `$tech-steampunk`, \
`$tech-near-future`, `$tech-stone-age`. Omit when contemporary-mundane.\n\
  - `tone`: emotional register. `$tone-grimdark`, `$tone-cosy`, \
`$tone-hopeful`, `$tone-bleak`.\n\
  - `pace`: narrative tempo. `$pace-slow-burn`, `$pace-breakneck`, \
`$pace-measured`.\n\
  - `theme`: dominant thematic concerns. `$theme-found-family`, \
`$theme-revenge`, `$theme-coming-of-age`. Up to 3.\n\
  - `era`: time period (real or relative). `$era-victorian`, \
`$era-post-apocalyptic`, `$era-far-future`, `$era-contemporary`.\n\
  - `world`: world ARCHETYPE — the *kind* of world. \
`$world-urban`, `$world-cyberpunk-megacity`, `$world-fantasy-medieval`, \
`$world-space-opera-far-future`. Up to 2.\n\
  - `location`: SPECIFIC named place (real or fictional). \
`$location-london`, `$location-victorian-london`, `$location-earth`, \
`$location-mars`, `$location-the-shire`, `$location-arrakis`, \
`$location-forest-moon-of-endor`. Emit whichever granularity is salient \
in the text — DO NOT auto-expand (`london` does NOT also emit `uk`, \
`europe`, `earth`).\n\
  - `race`: any collective identity featured prominently. \
`$race-elves`, `$race-martians`, `$race-vikings`. Emit at the BOOK level \
even when individual characters carry the same species tag (per ADR-0022 \
— book-level enables \"all books featuring elves\" queries without joins).\n\
  - `group`: faction / organisation / collective. \
`$group-imperial-navy`, `$group-house-atreides`, `$group-the-rebels`, \
`$group-the-survivors`. Formal AND informal both fit.\n\
\n\
CRITICAL — `$world` vs `$location`:\n\
- `$world-*` answers \"what KIND of world?\" — archetype.\n\
- `$location-*` answers \"WHERE specifically?\" — proper-noun place.\n\
A medieval-fantasy story set in the Shire emits BOTH \
`$world-fantasy-medieval` AND `$location-the-shire`. A modern thriller in \
London emits `$location-london` only (no `$world-*` needed when the world \
is just \"modern Earth, like ours\").\n\
\n\
CRITICAL — `$race` vs `$group`:\n\
- `$race-*` = born into it (elves, dwarves, Martians, Vikings as a people).\n\
- `$group-*` = joined or aligned with it (Imperial Navy, the Foundation, \
the rebels).\n\
\n\
Categories are optional — omit one entirely if the text gives no signal \
(e.g. a contemporary mainstream novel may emit nothing in `$magic-*` or \
`$tech-*`). Do not pad.\n\
\n\
TRANSCRIPT (locale={book_locale}):\n\
{excerpt}"
    )
}

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

    fn default_shape() -> PromptShape {
        PromptShape {
            paragraph_words_low: 30,
            paragraph_words_high: 60,
            max_tags: 25,
        }
    }

    #[test]
    fn build_prompt_includes_locale_caps_and_transcript() {
        let p = build_prompt("Once upon a time…", "de", default_shape());
        assert!(p.contains("`de`"));
        assert!(p.contains("30-60 words"));
        assert!(p.contains("Once upon a time"));
        assert!(p.contains("locale=de"));
    }

    #[test]
    fn build_prompt_lists_all_ten_categories() {
        let p = build_prompt("…", "en", default_shape());
        for category in [
            "`magic`",
            "`tech`",
            "`tone`",
            "`pace`",
            "`theme`",
            "`era`",
            "`world`",
            "`location`",
            "`race`",
            "`group`",
        ] {
            assert!(p.contains(category), "prompt missing category {category}");
        }
    }

    #[test]
    fn build_prompt_disambiguates_world_vs_location() {
        let p = build_prompt("…", "en", default_shape());
        assert!(p.contains("`$world` vs `$location`"));
        assert!(p.contains("ARCHETYPE"));
        assert!(p.contains("WHERE specifically"));
    }

    #[test]
    fn build_prompt_disambiguates_race_vs_group() {
        let p = build_prompt("…", "en", default_shape());
        assert!(p.contains("`$race` vs `$group`"));
        assert!(p.contains("born into it"));
        assert!(p.contains("joined or aligned with"));
    }

    #[test]
    fn build_prompt_forbids_spoiler_content_in_paragraph() {
        let p = build_prompt("…", "en", default_shape());
        assert!(p.contains("WORLD only"));
        assert!(p.contains("No plot beats"));
    }

    #[test]
    fn build_prompt_truncates_long_transcript() {
        let long = "x".repeat(40_000);
        let p = build_prompt(&long, "en", default_shape());
        assert!(p.len() < 35_000, "prompt len was {}", p.len());
    }

    #[test]
    fn parse_setting_response() {
        let json = r#"{
            "setting": "A drowned Victorian London under perpetual fog.",
            "setting_lang": "en",
            "setting_tags": ["$era-victorian", "$location-london", "$tone-bleak"]
        }"#;
        let r: SettingResponse = serde_json::from_str(json).expect("parse");
        assert_eq!(r.setting_lang, "en");
        assert_eq!(r.setting_tags.len(), 3);
        assert!(r.setting_tags.contains(&"$era-victorian".to_owned()));
    }

    #[test]
    fn normalise_setting_body_slugifies_input() {
        assert_eq!(
            normalise_setting_body("Fantasy-Medieval"),
            "fantasy-medieval",
        );
        assert_eq!(normalise_setting_body("Imperial Navy"), "imperial-navy",);
        assert_eq!(
            normalise_setting_body("Forest_Moon of Endor"),
            "forest-moon-of-endor",
        );
        // Punctuation / weird Unicode is dropped, not preserved.
        assert_eq!(
            normalise_setting_body("post.apocalyptic!"),
            "postapocalyptic"
        );
        // Runs of separators collapse.
        assert_eq!(normalise_setting_body("a---b___c"), "a-b-c");
        // Leading + trailing separators trimmed.
        assert_eq!(normalise_setting_body("--london--"), "london");
    }

    #[test]
    fn normalise_tags_strips_prefix_and_reformats() {
        let raw = vec![
            "$world-Fantasy-Medieval".to_owned(),
            "$location-London".to_owned(),
            "$race-Elves".to_owned(),
        ];
        let out = normalise_tags(&raw, BookId(1), 25);
        assert_eq!(out.len(), 3);
        assert!(out.contains(&"$world-fantasy-medieval".to_owned()));
        assert!(out.contains(&"$location-london".to_owned()));
        assert!(out.contains(&"$race-elves".to_owned()));
    }

    #[test]
    fn normalise_tags_dedup_after_slugify() {
        let raw = vec![
            "$world-Urban".to_owned(),
            "$world-urban".to_owned(),
            "$world-URBAN".to_owned(),
        ];
        let out = normalise_tags(&raw, BookId(1), 25);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], "$world-urban");
    }

    #[test]
    fn normalise_tags_drops_bare_tags() {
        let raw = vec![
            "victorian".to_owned(),    // bare body, no $
            "#world-urban".to_owned(), // wrong prefix
            "$location-london".to_owned(),
        ];
        let out = normalise_tags(&raw, BookId(1), 25);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], "$location-london");
    }

    #[test]
    fn normalise_tags_caps_at_max_tags() {
        let raw: Vec<String> = (0..40).map(|i| format!("$theme-theme-{i:03}")).collect();
        let out = normalise_tags(&raw, BookId(1), 10);
        assert_eq!(out.len(), 10);
        assert_eq!(out[0], "$theme-theme-000");
        assert_eq!(out[9], "$theme-theme-009");
    }

    #[test]
    fn primary_subtag_normalises_bcp47() {
        assert_eq!(primary_subtag("en"), "en");
        assert_eq!(primary_subtag("en-US"), "en");
        assert_eq!(primary_subtag("zh-Hans-CN"), "zh");
        assert_eq!(primary_subtag(""), "");
    }

    /// Schema-parity guard. Same pattern as the other LLM
    /// stages — the test asserts that the model's constraint
    /// JSON stays in lock-step with `SettingResponse`.
    #[test]
    fn schema_parses_and_matches_response_shape() {
        let v: serde_json::Value =
            serde_json::from_str(SETTING_SCHEMA_JSON).expect("schema parses");
        assert_eq!(v["type"], "object");
        let props = v["properties"]
            .as_object()
            .expect("properties is an object");

        for field in ["setting", "setting_lang", "setting_tags"] {
            assert!(
                props.contains_key(field),
                "schema missing top-level field `{field}`",
            );
        }

        assert_eq!(props["setting"]["type"], "string");
        assert_eq!(props["setting_lang"]["type"], "string");
        assert_eq!(props["setting_tags"]["type"], "array");
        assert_eq!(props["setting_tags"]["items"]["type"], "string");

        let required = v["required"]
            .as_array()
            .expect("required is an array")
            .iter()
            .map(|x| x.as_str().expect("required entry is string").to_owned())
            .collect::<Vec<_>>();
        for field in ["setting", "setting_lang", "setting_tags"] {
            assert!(
                required.contains(&field.to_owned()),
                "required missing `{field}`",
            );
        }
    }
}
