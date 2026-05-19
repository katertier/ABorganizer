//! ADR-0057 S57.1 — `transcript-fm-polish` stage **scaffold**.
//!
//! Apple-FM per-book transcript polish. This slice (S57.1a) lands
//! the pipeline-level wiring + every skip-condition branch, but
//! returns `Skipped` on the would-call-FM path. The FM call itself
//! lands in S57.1b; per-chapter slicing layers on in S57.1c.
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
//! ## Skip conditions (every one tested)
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
//! * Idempotency hit: an `ai_cache` row already exists at
//!   `cache_type = 'transcript_fm_polished'` for the current
//!   `extractor_version`. Bump
//!   [`LlmTunables::extractor_version`] to force a re-run
//!   library-wide.
//!
//! ## What this scaffold does NOT do (yet)
//!
//! * No FM call. Once every skip-condition branch returns
//!   `Skipped`, the stage logs `fm.polish.fm_call_pending` at
//!   `tracing::info` and returns `Skipped` instead of polishing.
//!   S57.1b replaces that branch with the real call.
//! * No per-chapter slicing — the per-chapter loop lands in S57.1c
//!   once the single-shot path is verified.

use std::sync::Arc;

use async_trait::async_trait;

use ab_core::tunables::LlmTunables;
use ab_core::{BookId, CacheKey, Error, Result};
use ab_db::LibraryDb;
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

        // 4. FM call deferred to S57.1b. Until then, log the
        //    "would-have-called" signal so operators can see the
        //    stage is wired but inert.
        tracing::info!(
            book_id = book_id.0,
            transcript_bytes = transcript.trim().len(),
            "fm.polish.fm_call_pending"
        );
        Ok(StageOutcome::Skipped)
    }
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
    async fn falls_back_to_transcript_full_when_corrected_blank() {
        let tmp = TempDir::new().expect("tmp");
        let ctx = fresh_ctx(tmp.path()).await;
        let id = seed_book(&ctx, None).await;
        // transcript_corrected stays NULL; only transcript_full has text.
        seed_transcript_full(&ctx, id, &long_transcript(1000)).await;
        // Until S57.1b lands the FM call, the would-call branch
        // returns Skipped after logging fm.polish.fm_call_pending.
        let outcome = stage().run(&ctx, BookId(id)).await.expect("run");
        assert_eq!(outcome, StageOutcome::Skipped);
    }

    #[tokio::test]
    async fn would_call_fm_returns_skipped_until_57_1b() {
        let tmp = TempDir::new().expect("tmp");
        let ctx = fresh_ctx(tmp.path()).await;
        let id = seed_book(&ctx, None).await;
        seed_transcript_corrected(&ctx, id, &long_transcript(1000)).await;
        let outcome = stage().run(&ctx, BookId(id)).await.expect("run");
        assert_eq!(
            outcome,
            StageOutcome::Skipped,
            "S57.1a scaffold returns Skipped on would-call-FM path; S57.1b wires the call"
        );
    }
}
