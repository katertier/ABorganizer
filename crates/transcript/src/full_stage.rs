//! `transcribe-full` pipeline stage (slice 3B).
//!
//! Transcribes the whole book at Idle priority and stores the
//! result in `ai_cache` with `cache_type = "transcript_full"`.
//! Designed to drain only when interactive + background queues
//! are quiet (see [`ab_pipeline::Priority::Idle`] semantics).
//!
//! ## Why a separate stage from head/tail
//!
//! Head + tail (`transcribe-head-tail`) runs at Interactive /
//! Background priority because downstream extractors (language
//! pre-pick, audiologo, title/author confirm) need the head
//! transcript on the first scan to fan out the rest of the
//! pipeline. Full-book transcribe is for the deeper-reading
//! extractors (DNA tags, summary, story arc, person/FTS) and
//! takes orders of magnitude longer to run; it earns its own
//! lifecycle.
//!
//! ## Rust-side chunking
//!
//! `transcribe_window` materialises the entire PCM window in RAM
//! before feeding `SpeechAnalyzer` (slice 3A.3 stub — proper
//! `AVAssetReader` streaming is a future Swift rewrite). For a
//! 30-hour book a one-shot call would need ~7 GB. This stage
//! splits into `chunk_secs`-sized windows (default 300 s =
//! 5 min, ~200 MB per chunk after Float32 conversion) and
//! concatenates the resulting segments.
//!
//! Segment timestamps are absolute file-time because the
//! `bufferStartTime` we pass on the Swift side maps each chunk
//! into the original file's time-base. No timestamp arithmetic
//! needed on the Rust concat — segments from later chunks
//! already have larger `start_ms` / `end_ms` values.
//!
//! ## Locale source
//!
//! Reads the cached head transcript's embedded `locale` and
//! reuses it. If the head row is missing, the stage returns
//! `Skipped` — `requires = ["transcribe-head-tail"]` should
//! prevent this in practice, but the explicit check protects
//! against scheduler races.
//!
//! ## Failure modes
//!
//! - No head transcript yet → `Skipped` (transcribe-head-tail
//!   hasn't run / didn't complete for this book).
//! - Duration below `min_duration_secs` → `Skipped`.
//! - `ModelNotInstalled` on any chunk → log + return `Skipped`.
//!   Same idle-installer pathway handles the re-queue.
//! - Other bridge / DB errors propagate.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;

use ab_core::tunables::TranscribeTunables;
use ab_core::{BookId, Error, Result};
use ab_db::LibraryDb;
use ab_pipeline::{Stage, StageContext, StageOutcome};

use crate::bridge::{BridgeError, TranscriptSegment, transcribe_window_typed};
use crate::stage::{CACHE_TYPE_HEAD, STAGE_NAME as HEAD_TAIL_STAGE};

/// Stage name written to `pipeline_progress` and registered with
/// the daemon.
pub const STAGE_NAME: &str = "transcribe-full";

/// `ai_cache.cache_type` value for the whole-book transcript.
/// Reuses the existing schema's documented value
/// (`schema.sql` lists `transcript_full`).
pub const CACHE_TYPE_FULL: &str = "transcript_full";

/// Idle-priority full-book transcribe stage.
pub struct TranscribeFullStage {
    transcribe: Arc<TranscribeTunables>,
}

impl TranscribeFullStage {
    /// Construct.
    #[must_use]
    pub fn new(transcribe: &TranscribeTunables) -> Self {
        Self {
            transcribe: Arc::new(transcribe.clone()),
        }
    }
}

#[async_trait]
impl Stage for TranscribeFullStage {
    fn name(&self) -> &'static str {
        STAGE_NAME
    }

    fn requires(&self) -> &'static [&'static str] {
        // We pull `locale` out of the head row's embedded JSON;
        // declaring the dep makes the requirement explicit + lets
        // future scheduler features (e.g. gap analysis) flag the
        // ordering.
        &[HEAD_TAIL_STAGE]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        let Some(plan) = plan_full(&ctx.library, book_id, &self.transcribe).await? else {
            return Ok(StageOutcome::Skipped);
        };

        let mut all_segments: Vec<TranscriptSegment> = Vec::new();
        // Integer chunk count avoids the float-comparison loop
        // clippy correctly flags as fragile near boundaries.
        // `chunks_total = ceil(total_secs / chunk_secs)`.
        let chunk_secs = self.transcribe.full_chunk_secs;
        // f64 → u64 floor cast: total_secs is non-negative and
        // bounded above by the longest plausible audiobook
        // (~30 hr = 108_000 s, well inside u64). The +1 handles
        // the trailing partial chunk.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let chunks_total = (plan.total_secs / chunk_secs).ceil().max(0.0) as u64;
        for chunk_idx in 0..chunks_total {
            // u64 → f64: chunk_idx is bounded by chunks_total,
            // which we just computed from total_secs / chunk_secs;
            // the multiplication won't exceed total_secs and so
            // can't overflow the f64 mantissa for any reasonable
            // book length.
            #[allow(clippy::cast_precision_loss)]
            let chunk_start = chunk_idx as f64 * chunk_secs;
            let chunk_end = (chunk_start + chunk_secs).min(plan.total_secs);
            tracing::debug!(
                book = %book_id,
                chunk_start,
                chunk_end,
                total = plan.total_secs,
                "transcribe.full.chunk"
            );
            match transcribe_window_typed(&plan.file_path, chunk_start, chunk_end, &plan.locale)
                .await
            {
                Ok(mut segs) => {
                    all_segments.append(&mut segs);
                }
                Err(BridgeError::ModelNotInstalled) => {
                    // The head/tail stage already queued this
                    // locale for the idle installer (3A.4.1); a
                    // second insert is a no-op. Just bail and
                    // wait for the install loop to re-queue us.
                    tracing::warn!(
                        locale = %plan.locale,
                        book = %book_id,
                        "transcribe.full.skip.model_not_installed"
                    );
                    return Ok(StageOutcome::Skipped);
                }
                Err(e) => return Err(e.into()),
            }
        }

        if all_segments.is_empty() {
            // Engine returned nothing for the whole file —
            // unusual but possible for ambient-only audio. Don't
            // write an empty row; let the next run retry.
            tracing::warn!(book = %book_id, "transcribe.full.no_segments");
            return Ok(StageOutcome::Skipped);
        }

        write_full_cache(
            &ctx.library,
            book_id,
            &all_segments,
            &plan.locale,
            &self.transcribe.model_version,
        )
        .await?;
        Ok(StageOutcome::Done)
    }
}

#[derive(Debug)]
struct FullPlan {
    file_path: PathBuf,
    total_secs: f64,
    locale: String,
}

#[derive(serde::Deserialize)]
struct HeadPayload {
    locale: String,
}

/// Pull file path + total duration from the library DB and the
/// locale from the head transcript's cached payload.
/// Returns `None` when the book should be skipped (no head
/// transcript, no active file, duration below threshold, or the
/// full transcript is already cached at the current
/// `model_version` + locale).
async fn plan_full(
    library: &LibraryDb,
    book_id: BookId,
    transcribe: &TranscribeTunables,
) -> Result<Option<FullPlan>> {
    let id = book_id.0;

    // Locale from head transcript cache.
    let head_row = sqlx::query!(
        "SELECT content FROM ai_cache WHERE book_id = ? AND cache_type = ?",
        id,
        CACHE_TYPE_HEAD,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("transcribe-full head lookup: {e}")))?;
    let Some(head_row) = head_row else {
        return Ok(None);
    };
    let Some(bytes) = head_row.content else {
        return Ok(None);
    };
    let Ok(parsed) = serde_json::from_slice::<HeadPayload>(&bytes) else {
        // Defensive: head row content unparseable. Wait for the
        // head stage to rewrite it cleanly.
        return Ok(None);
    };
    let locale = parsed.locale;

    // Idempotency.
    if full_cache_fresh(library, book_id, &transcribe.model_version, &locale).await? {
        return Ok(None);
    }

    // Total duration from books.raw_duration_ms (prefer raw so
    // jingles fall inside the windows).
    let book_row = sqlx::query!(
        "SELECT duration_ms, raw_duration_ms FROM books WHERE book_id = ?",
        id,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("transcribe-full book lookup: {e}")))?;
    let Some(book_row) = book_row else {
        return Ok(None);
    };
    let total_ms = book_row
        .raw_duration_ms
        .or(book_row.duration_ms)
        .unwrap_or(0)
        .max(0);
    #[allow(clippy::cast_precision_loss)]
    let total_secs = total_ms as f64 / 1000.0;
    if total_secs < transcribe.min_duration_secs {
        return Ok(None);
    }

    // File path — first active file. Multi-file books need
    // per-file iteration to keep timestamps coherent across
    // files; that's a follow-up (see PROJECT.md "multi-file
    // full transcribe").
    let file_row = sqlx::query!(
        "SELECT file_path FROM book_files \
         WHERE book_id = ? AND is_active = 1 ORDER BY file_id LIMIT 1",
        id,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("transcribe-full file lookup: {e}")))?;
    let Some(file_row) = file_row else {
        return Ok(None);
    };

    Ok(Some(FullPlan {
        file_path: PathBuf::from(file_row.file_path),
        total_secs,
        locale,
    }))
}

/// Same shape as `stage::cache_fresh` but specialised to the
/// full transcript row. Locale comes from the embedded payload
/// (same as the head/tail freshness check).
async fn full_cache_fresh(
    library: &LibraryDb,
    book_id: BookId,
    model_version: &str,
    current_locale: &str,
) -> Result<bool> {
    let id = book_id.0;
    let row = sqlx::query!(
        "SELECT model_version, content FROM ai_cache WHERE book_id = ? AND cache_type = ?",
        id,
        CACHE_TYPE_FULL,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("transcribe-full cache lookup: {e}")))?;
    let Some(row) = row else { return Ok(false) };
    if row.model_version.as_deref() != Some(model_version) {
        return Ok(false);
    }
    let Some(bytes) = row.content else {
        return Ok(false);
    };
    let Ok(parsed) = serde_json::from_slice::<HeadPayload>(&bytes) else {
        return Ok(false);
    };
    Ok(parsed.locale == current_locale)
}

#[derive(Debug, Serialize)]
struct FullPayload<'a> {
    locale: &'a str,
    segments: &'a [TranscriptSegment],
}

async fn write_full_cache(
    library: &LibraryDb,
    book_id: BookId,
    segments: &[TranscriptSegment],
    locale: &str,
    model_version: &str,
) -> Result<()> {
    let payload = FullPayload { locale, segments };
    let bytes = serde_json::to_vec(&payload)
        .map_err(|e| Error::stage("transcribe-full", format!("encode payload: {e}")))?;
    let conf = mean_confidence(segments);
    let id = book_id.0;
    // `compressed = 0`: a 5-hour book is ~3 MB of JSON, a 30-hour
    // book ~18 MB. SQLite BLOBs handle this fine. Turning on zstd
    // compression is a follow-up — wants benchmarking on
    // representative books before changing the storage contract.
    sqlx::query!(
        "INSERT OR REPLACE INTO ai_cache \
         (book_id, cache_type, content, compressed, confidence, model_version) \
         VALUES (?, ?, ?, 0, ?, ?)",
        id,
        CACHE_TYPE_FULL,
        bytes,
        conf,
        model_version,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("transcribe-full write cache: {e}")))?;
    Ok(())
}

fn mean_confidence(segments: &[TranscriptSegment]) -> f64 {
    if segments.is_empty() {
        return 0.0;
    }
    let sum: f64 = segments.iter().map(|s| f64::from(s.confidence)).sum();
    #[allow(clippy::cast_precision_loss)]
    let n = segments.len() as f64;
    sum / n
}
