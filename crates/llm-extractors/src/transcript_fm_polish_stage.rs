//! ADR-0057 S57.1b — `transcript-fm-polish` stage, single-shot.
//!
//! Apple-FM per-book transcript polish. S57.1a landed the scaffold +
//! every skip-condition branch; this slice (S57.1b) wires the actual
//! `complete_structured` call on the would-call-FM branch. The
//! output is the input transcript with on-device-LLM corrections
//! (re-casing, sentence-boundary repair, mid-sentence-noise removal)
//! cached at `ai_cache.cache_type = 'transcript_fm_polished'`. The
//! input is taken **whole**; per-chapter slicing for long
//! transcripts is deferred to S57.1c.
//!
//! ## Source-of-truth for the input transcript
//!
//! 1. `books.transcript_corrected` — written by the C.5
//!    `transcript-correct-via-epub` stage (ADR-0043). Preferred
//!    when present.
//! 2. `ai_cache` row `cache_type = 'transcript_full'` — written by
//!    `transcribe-full`. Fallback when C.5 didn't fire (no EPUB
//!    companion, language mismatch, or `books.abridged = true`).
//!
//! ## Skip conditions (every one tested at the unit level)
//!
//! * `books.abridged = 1` — the FM polish prompt assumes the
//!   transcript reflects the full text. Abridged readings produce
//!   misleading "corrections" against publisher prose, and the
//!   C.5 stage already short-circuits the same way.
//! * No transcript available (neither `transcript_corrected` nor
//!   the `transcript_full` cache row, or both are empty).
//! * Input text under [`MIN_TRANSCRIPT_BYTES`] — too short for FM
//!   to do meaningful work; matches the sanity floors in the DNA /
//!   summary / setting stages.
//! * `books.language` is NULL — without it we can't tell the model
//!   what locale to polish in. The `transcribe-head-tail` stage
//!   seeds this column; missing language means the pipeline
//!   isn't ready yet.
//! * Idempotency hit: an `ai_cache` row already exists at
//!   `cache_type = 'transcript_fm_polished'` for the current
//!   `extractor_version`. Bump
//!   [`LlmTunables::extractor_version`] to force a re-run
//!   library-wide.
//! * Locale mismatch on the FM response: the model returned a
//!   `polished_lang` whose primary subtag doesn't match
//!   `books.language`. The cache row still lands (so re-runs at
//!   the same `extractor_version` are no-ops) but the result is
//!   not exposed as a "polished" transcript.
//!
//! ## What this slice still does NOT do
//!
//! * No per-chapter slicing — long transcripts get
//!   [`build_prompt`]'s 30k-char defensive truncation; the
//!   per-chapter loop lands in S57.1c.
//! * No promotion to a `books.transcript_polished` column. The
//!   polished text is consumed downstream from the cache row
//!   (matches the `epub_name_dict` posture).

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use ab_core::tunables::LlmTunables;
use ab_core::{BookId, CacheKey, Error, Result};
use ab_db::LibraryDb;
use ab_foundation_models::{BridgeError, GenerationOptions, complete_structured};
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};

/// Typed identifier for this stage.
pub const STAGE_ID: StageId = StageId::new("transcript-fm-polish");

/// Stage name written to `pipeline_progress`. Derives from `STAGE_ID`.
pub const STAGE_NAME: &str = STAGE_ID.as_str();

/// Lower bound on input transcript size, in bytes.
///
/// Below this floor the FM polish call doesn't have enough signal
/// to be useful — small transcripts on short books +
/// extremely-quiet recordings end up here. Same posture as the
/// summary / DNA / setting stages' sanity floors.
pub const MIN_TRANSCRIPT_BYTES: usize = 200;

/// JSON Schema passed to `complete_structured`.
///
/// Constrains the model's output at decode time. Maps to a
/// `DynamicGenerationSchema` on the Swift side and matches the
/// [`PolishResponse`] Rust shape one-to-one so any drift surfaces
/// as a parse failure in the schema-parity test.
pub const POLISH_SCHEMA_JSON: &str = r#"{
    "type": "object",
    "properties": {
        "polished_text": {"type": "string"},
        "polished_lang": {"type": "string"}
    },
    "required": ["polished_text", "polished_lang"]
}"#;

/// Apple-FM transcript polish stage.
///
/// Construction is owned-`LlmTunables`-clone identical to the
/// summary stage; the daemon-side wiring is the same shape.
pub struct TranscriptFmPolishStage {
    tunables: Arc<LlmTunables>,
}

impl TranscriptFmPolishStage {
    /// Construct a stage that reads its `extractor_version` from `tunables`.
    #[must_use]
    pub fn new(tunables: &LlmTunables) -> Self {
        Self {
            tunables: Arc::new(tunables.clone()),
        }
    }
}

#[async_trait]
impl Stage for TranscriptFmPolishStage {
    fn name(&self) -> &'static str {
        STAGE_NAME
    }

    fn requires(&self) -> &'static [StageId] {
        // Full transcript must exist (transcribe-full's
        // ai_cache row is the fallback when transcript_corrected
        // is NULL); C.5's transcript-correct-via-epub is the
        // preferred input. Per ADR-0057 § Downstream order:
        // transcribe-full → c5-correct-via-epub → transcript-fm-polish.
        &[ab_transcript::full_stage::STAGE_ID]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        // 1. Idempotency: already polished at this extractor_version?
        if fm_polish_cache_fresh(&ctx.library, book_id, &self.tunables.extractor_version).await? {
            return Ok(StageOutcome::Skipped);
        }

        // 2. Abridged short-circuit.
        if book_is_abridged(&ctx.library, book_id).await? {
            tracing::debug!(book_id = book_id.0, "fm.polish.skip_abridged");
            return Ok(StageOutcome::Skipped);
        }

        // 3. Resolve the input transcript text:
        //    transcript_corrected > transcript_full > skip.
        let Some(transcript) = load_input_transcript(&ctx.library, book_id).await? else {
            tracing::debug!(book_id = book_id.0, "fm.polish.skip_no_transcript");
            return Ok(StageOutcome::Skipped);
        };
        if transcript.trim().len() < MIN_TRANSCRIPT_BYTES {
            tracing::debug!(
                book_id = book_id.0,
                bytes = transcript.trim().len(),
                "fm.polish.skip_too_short"
            );
            return Ok(StageOutcome::Skipped);
        }

        // 4. Resolve the book's language (output locale). NULL →
        //    pipeline isn't ready; the transcribe-head-tail stage
        //    seeds this column.
        let Some(book_lang) = load_book_language(&ctx.library, book_id).await? else {
            tracing::debug!(book_id = book_id.0, "fm.polish.skip_no_language");
            return Ok(StageOutcome::Skipped);
        };

        // 5. Build prompt + call the bridge.
        let prompt = build_prompt(&transcript, &book_lang);
        let opts = GenerationOptions::new(self.tunables.polish_max_tokens);
        let raw = match complete_structured(&prompt, POLISH_SCHEMA_JSON, &opts).await {
            Ok(s) => s,
            Err(BridgeError::PromptEmpty) => return Ok(StageOutcome::Skipped),
            Err(e) => return Err(bridge_to_stage_error(&e)),
        };

        // 6. Parse JSON.
        let parsed: PolishResponse = match serde_json::from_str(&raw) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    book_id = book_id.0,
                    error = %e,
                    raw_len = raw.len(),
                    "fm.polish.parse_failed"
                );
                return Err(Error::stage(STAGE_NAME, format!("model JSON parse: {e}")));
            }
        };

        // 7. Locale self-check. The schema constrains output shape,
        //    not semantics — the model can still emit
        //    `polished_lang = "en"` when we asked for German.
        //    Cache the raw response either way (so re-runs are
        //    no-ops) but only return Done when the locale matches.
        let polished_trimmed = parsed.polished_text.trim();
        let locale_ok = parsed.polished_lang.eq_ignore_ascii_case(&book_lang)
            || primary_subtag(&parsed.polished_lang) == primary_subtag(&book_lang);
        if !locale_ok {
            tracing::warn!(
                book_id = book_id.0,
                expected = %book_lang,
                got = %parsed.polished_lang,
                "fm.polish.locale_mismatch"
            );
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

        if polished_trimmed.is_empty() {
            tracing::warn!(book_id = book_id.0, "fm.polish.empty_output");
            return Err(Error::stage(STAGE_NAME, "model returned empty polish"));
        }

        // 8. Write the polished cache row. No `books` column gets
        //    promoted — downstream stages read from `ai_cache`
        //    directly (same posture as `epub_name_dict`).
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
            input_bytes = transcript.trim().len(),
            output_bytes = polished_trimmed.len(),
            "fm.polish.done"
        );
        Ok(StageOutcome::Done)
    }
}

/// JSON shape produced by the LLM. `polished_lang` is a self-check
/// — the prompt asks for the book's BCP-47 tag and the locale-
/// mismatch path rejects bad outputs.
#[derive(Debug, Deserialize)]
struct PolishResponse {
    polished_text: String,
    polished_lang: String,
}

/// True when an `ai_cache` row exists at the current
/// `extractor_version` for this book + the polish cache key.
async fn fm_polish_cache_fresh(
    library: &LibraryDb,
    book_id: BookId,
    extractor_version: &str,
) -> Result<bool> {
    let id = book_id.0;
    let cache = CacheKey::TranscriptFmPolished.as_str();
    let row = sqlx::query!(
        "SELECT extractor_version FROM ai_cache WHERE book_id = ? AND cache_type = ?",
        id,
        cache,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("fm_polish cache lookup: {e}")))?;
    let Some(row) = row else { return Ok(false) };
    Ok(row.extractor_version.as_deref() == Some(extractor_version))
}

/// Read `books.abridged` — TRUE shortcuts the polish.
async fn book_is_abridged(library: &LibraryDb, book_id: BookId) -> Result<bool> {
    let id = book_id.0;
    let row = sqlx::query!("SELECT abridged FROM books WHERE book_id = ?", id)
        .fetch_optional(library.pool())
        .await
        .map_err(|e| Error::Database(format!("fm_polish abridged lookup: {e}")))?;
    Ok(row.is_some_and(|r| r.abridged == Some(1)))
}

/// Prefer `books.transcript_corrected`; fall back to the
/// `transcript_full` cache row's concatenated segment text.
/// Returns `None` when neither source is available or both are
/// blank.
async fn load_input_transcript(library: &LibraryDb, book_id: BookId) -> Result<Option<String>> {
    let id = book_id.0;
    // 1. transcript_corrected from books.
    let row = sqlx::query!(
        "SELECT transcript_corrected FROM books WHERE book_id = ?",
        id
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("fm_polish corrected lookup: {e}")))?;
    if let Some(row) = row {
        if let Some(text) = row.transcript_corrected {
            if !text.trim().is_empty() {
                return Ok(Some(text));
            }
        }
    }

    // 2. Fallback: transcript_full cache row.
    let cache = CacheKey::TranscriptFull.as_str();
    let cache_row = sqlx::query!(
        "SELECT content FROM ai_cache WHERE book_id = ? AND cache_type = ?",
        id,
        cache,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("fm_polish full lookup: {e}")))?;
    let Some(row) = cache_row else {
        return Ok(None);
    };
    let Some(bytes) = row.content else {
        return Ok(None);
    };
    let cached: CachedTranscript = match ab_core::cache::deserialize_cache_content(&bytes) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(book_id = id, error = %e, "fm.polish.transcript_unparseable");
            return Ok(None);
        }
    };
    let mut text = String::with_capacity(
        cached
            .segments
            .iter()
            .map(|s| s.text.len() + 1)
            .sum::<usize>(),
    );
    for seg in &cached.segments {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&seg.text);
    }
    if text.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(text))
}

#[derive(Debug, serde::Deserialize)]
struct CachedTranscript {
    segments: Vec<Segment>,
}

#[derive(Debug, serde::Deserialize)]
struct Segment {
    text: String,
}

/// Read the book's native language (BCP-47). NULL → not ready —
/// the `transcribe-head-tail` stage seeds this column from the
/// detected speech locale, falling back to tag metadata.
async fn load_book_language(library: &LibraryDb, book_id: BookId) -> Result<Option<String>> {
    let id = book_id.0;
    let row = sqlx::query!("SELECT language FROM books WHERE book_id = ?", id)
        .fetch_optional(library.pool())
        .await
        .map_err(|e| Error::Database(format!("fm_polish load lang: {e}")))?;
    let Some(row) = row else { return Ok(None) };
    let trimmed = row
        .language
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    Ok(trimmed.map(str::to_owned))
}

#[derive(Debug, Serialize)]
struct CachePayload<'a> {
    raw: &'a str,
}

/// Write the polished cache row. `INSERT OR REPLACE` so re-runs
/// at a bumped `extractor_version` overwrite the previous attempt;
/// the freshness check in `fm_polish_cache_fresh` keys on the
/// version string.
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
    let polish_cache = CacheKey::TranscriptFmPolished.as_str();
    sqlx::query!(
        "INSERT OR REPLACE INTO ai_cache \
         (book_id, cache_type, content, compressed, extractor_version, locale) \
         VALUES (?, ?, ?, 0, ?, ?)",
        id,
        polish_cache,
        bytes,
        extractor_version,
        locale,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("fm_polish cache write: {e}")))?;
    Ok(())
}

/// Build the prompt sent to the LLM. Public for unit-testing the
/// content (locale instruction, no-rewrite rules, single-shot
/// truncation).
///
/// `transcript` is the concatenated full-or-corrected transcript;
/// truncated defensively at 30k chars (matches the summary stage's
/// budget — past that the on-device model's context cost outpaces
/// the marginal signal). Per-chapter slicing lands in S57.1c and
/// will replace this truncation with structured chunks.
#[must_use]
pub fn build_prompt(transcript: &str, book_locale: &str) -> String {
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
    // The schema shape (`polished_text`, `polished_lang`) is conveyed
    // to the model by complete_structured with includeSchemaInPrompt:
    // true; we don't restate it here. What stays in the prompt:
    // semantics the schema can't express (preserve content,
    // re-case, fix sentence boundaries, drop mid-sentence ASR
    // noise, do NOT rewrite content).
    format!(
        "You are polishing a raw speech-to-text transcript of an audiobook \
for downstream consumption (subtitle export, EPUB generation, mid-book search).\n\
\n\
Rules:\n\
1. Preserve the speaker's words and meaning. Do NOT paraphrase, summarise, \
condense, or rewrite. The polish is mechanical: re-casing, punctuation, \
sentence boundary repair, removal of mid-sentence ASR noise (\"uh\", \"um\", \
\"--\" stutter markers).\n\
2. Output language is `{book_locale}` (BCP-47). Set `polished_lang` to \
`{book_locale}` to confirm. Do not translate.\n\
3. Preserve named-entity casing (people, places, proper nouns) when context \
makes them obvious; leave them lowercase otherwise — do NOT guess.\n\
4. Do not invent dialogue tags, narrator interjections, or chapter headers \
that aren't in the input.\n\
5. Whitespace normalisation: collapse runs of spaces, drop spaces before \
punctuation, preserve paragraph breaks (double-newline).\n\
\n\
TRANSCRIPT (locale={book_locale}):\n\
{excerpt}"
    )
}

/// BCP-47 primary subtag: `en-US` → `en`, `zh-Hans-CN` → `zh`.
/// Used to permit `en` ↔ `en-US` matches in the locale self-check.
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
    use ab_core::tunables::DbTunables;
    use tempfile::TempDir;

    async fn fresh_ctx(dir: &std::path::Path) -> StageContext {
        let lib = LibraryDb::open(&dir.join("library.db"), &DbTunables::default())
            .await
            .expect("open library");
        let eph = ab_db::EphemeralDb::open(&dir.join("ephemeral.db"), &DbTunables::default())
            .await
            .expect("open ephemeral");
        StageContext {
            library: lib,
            ephemeral: eph,
            cancel: tokio_util::sync::CancellationToken::new(),
            stage_name: STAGE_NAME,
        }
    }

    fn long_transcript(bytes: usize) -> String {
        // Repeat a word until we exceed the requested byte count.
        let mut s = String::with_capacity(bytes + 16);
        while s.len() < bytes {
            s.push_str("Lorem ipsum dolor sit amet ");
        }
        s
    }

    async fn seed_book(ctx: &StageContext, abridged: Option<i64>) -> i64 {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO books (title, duration_ms, raw_duration_ms, abridged) \
             VALUES ('T', 60000, 60000, ?) RETURNING book_id",
        )
        .bind(abridged)
        .fetch_one(ctx.library.pool())
        .await
        .expect("seed book");
        id
    }

    async fn seed_transcript_corrected(ctx: &StageContext, book_id: i64, text: &str) {
        sqlx::query("UPDATE books SET transcript_corrected = ? WHERE book_id = ?")
            .bind(text)
            .bind(book_id)
            .execute(ctx.library.pool())
            .await
            .expect("set transcript_corrected");
    }

    async fn seed_transcript_full(ctx: &StageContext, book_id: i64, text: &str) {
        let payload = serde_json::json!({
            "segments": [{"text": text}],
        });
        let bytes = serde_json::to_vec(&payload).unwrap();
        sqlx::query(
            "INSERT INTO ai_cache (book_id, cache_type, content, extractor_version) \
             VALUES (?, 'transcript_full', ?, 'v1')",
        )
        .bind(book_id)
        .bind(bytes)
        .execute(ctx.library.pool())
        .await
        .expect("seed transcript_full");
    }

    async fn seed_fm_polish_cache(ctx: &StageContext, book_id: i64, ver: &str) {
        let payload = b"{\"raw\":\"placeholder\"}".to_vec();
        sqlx::query(
            "INSERT INTO ai_cache (book_id, cache_type, content, extractor_version) \
             VALUES (?, 'transcript_fm_polished', ?, ?)",
        )
        .bind(book_id)
        .bind(payload)
        .bind(ver)
        .execute(ctx.library.pool())
        .await
        .expect("seed polish cache");
    }

    fn stage() -> TranscriptFmPolishStage {
        let tunables = LlmTunables::default();
        TranscriptFmPolishStage::new(&tunables)
    }

    #[tokio::test]
    async fn skips_when_no_transcript() {
        let tmp = TempDir::new().expect("tmp");
        let ctx = fresh_ctx(tmp.path()).await;
        let id = seed_book(&ctx, None).await;
        let outcome = stage().run(&ctx, BookId(id)).await.expect("run");
        assert_eq!(outcome, StageOutcome::Skipped);
    }

    #[tokio::test]
    async fn skips_when_abridged_even_if_transcript_present() {
        let tmp = TempDir::new().expect("tmp");
        let ctx = fresh_ctx(tmp.path()).await;
        let id = seed_book(&ctx, Some(1)).await;
        seed_transcript_corrected(&ctx, id, &long_transcript(1000)).await;
        let outcome = stage().run(&ctx, BookId(id)).await.expect("run");
        assert_eq!(outcome, StageOutcome::Skipped);
    }

    #[tokio::test]
    async fn skips_when_transcript_too_short() {
        let tmp = TempDir::new().expect("tmp");
        let ctx = fresh_ctx(tmp.path()).await;
        let id = seed_book(&ctx, None).await;
        // Below the 200-byte floor.
        seed_transcript_corrected(&ctx, id, "tiny").await;
        let outcome = stage().run(&ctx, BookId(id)).await.expect("run");
        assert_eq!(outcome, StageOutcome::Skipped);
    }

    #[tokio::test]
    async fn skips_when_cache_fresh_at_extractor_version() {
        let tmp = TempDir::new().expect("tmp");
        let ctx = fresh_ctx(tmp.path()).await;
        let id = seed_book(&ctx, None).await;
        seed_transcript_corrected(&ctx, id, &long_transcript(1000)).await;
        let tunables = LlmTunables::default();
        seed_fm_polish_cache(&ctx, id, &tunables.extractor_version).await;
        let outcome = stage().run(&ctx, BookId(id)).await.expect("run");
        assert_eq!(outcome, StageOutcome::Skipped);
    }

    #[tokio::test]
    async fn skips_when_book_language_missing() {
        // Otherwise-eligible book — no abridged flag, fresh
        // transcript_corrected, no prior polish cache — but no
        // language seed. The pre-FM language check is the last
        // skip gate; it must short-circuit before we try to call
        // the FM bridge (which would fail catastrophically in CI).
        let tmp = TempDir::new().expect("tmp");
        let ctx = fresh_ctx(tmp.path()).await;
        let id = seed_book(&ctx, None).await;
        seed_transcript_corrected(&ctx, id, &long_transcript(1000)).await;
        let outcome = stage().run(&ctx, BookId(id)).await.expect("run");
        assert_eq!(outcome, StageOutcome::Skipped);
    }

    #[tokio::test]
    async fn load_input_transcript_prefers_corrected_over_full() {
        let tmp = TempDir::new().expect("tmp");
        let ctx = fresh_ctx(tmp.path()).await;
        let id = seed_book(&ctx, None).await;
        seed_transcript_corrected(&ctx, id, "corrected wins").await;
        seed_transcript_full(&ctx, id, "full should be ignored").await;
        let got = load_input_transcript(&ctx.library, BookId(id))
            .await
            .expect("load")
            .expect("some");
        assert_eq!(got, "corrected wins");
    }

    #[tokio::test]
    async fn load_input_transcript_falls_back_to_full_when_corrected_blank() {
        let tmp = TempDir::new().expect("tmp");
        let ctx = fresh_ctx(tmp.path()).await;
        let id = seed_book(&ctx, None).await;
        // transcript_corrected stays NULL.
        seed_transcript_full(&ctx, id, "full text").await;
        let got = load_input_transcript(&ctx.library, BookId(id))
            .await
            .expect("load")
            .expect("some");
        assert_eq!(got, "full text");
    }

    #[test]
    fn build_prompt_includes_locale_and_truncates() {
        let p = build_prompt("Once upon a time…", "de");
        assert!(p.contains("`de`"), "BCP-47 tag must appear in the prompt");
        assert!(p.contains("locale=de"));
        assert!(p.contains("Once upon a time"));
        assert!(
            p.contains("Do NOT paraphrase"),
            "preserve-content rule must appear"
        );

        let long = "x".repeat(40_000);
        let truncated = build_prompt(&long, "en");
        assert!(
            truncated.len() < 32_000,
            "prompt was {} chars (input was 40k)",
            truncated.len()
        );
    }

    #[test]
    fn primary_subtag_normalises_bcp47() {
        assert_eq!(primary_subtag("en"), "en");
        assert_eq!(primary_subtag("en-US"), "en");
        assert_eq!(primary_subtag("zh-Hans-CN"), "zh");
        assert_eq!(primary_subtag(""), "");
    }

    #[test]
    fn parse_polish_response() {
        let json = r#"{"polished_text":"Hello.","polished_lang":"en"}"#;
        let r: PolishResponse = serde_json::from_str(json).expect("parse");
        assert_eq!(r.polished_text, "Hello.");
        assert_eq!(r.polished_lang, "en");
    }

    /// Verifies [`POLISH_SCHEMA_JSON`] parses as JSON and names the
    /// exact fields the [`PolishResponse`] deserialiser reads.
    /// Catches drift between the schema and the Rust shape.
    #[test]
    fn schema_parses_and_matches_response_shape() {
        let v: serde_json::Value = serde_json::from_str(POLISH_SCHEMA_JSON).expect("schema parses");
        assert_eq!(v["type"], "object");
        let props = v["properties"]
            .as_object()
            .expect("properties is an object");
        for field in ["polished_text", "polished_lang"] {
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
        assert!(required.contains(&"polished_text".to_owned()));
        assert!(required.contains(&"polished_lang".to_owned()));
    }
}
