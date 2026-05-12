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

use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;

use ab_core::tunables::TranscribeTunables;
use ab_core::{BookId, Error, Result};
use ab_db::LibraryDb;
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};

use crate::stage::STAGE_ID as HEAD_TAIL_STAGE_ID;
use ab_core::CacheKey;
use ab_speech::{BridgeError, TranscriptSegment, transcribe_window_typed};

/// Typed identifier for this stage.
pub const STAGE_ID: StageId = StageId::new("transcribe-full");

/// Stage name written to `pipeline_progress`. Derives from `STAGE_ID`.
pub const STAGE_NAME: &str = STAGE_ID.as_str();

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

    fn requires(&self) -> &'static [StageId] {
        // We pull `locale` out of the head row's `locale` column;
        // declaring the dep makes the requirement explicit + lets
        // future scheduler features (e.g. gap analysis) flag the
        // ordering.
        &[HEAD_TAIL_STAGE_ID]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        let Some(plan) = plan_full(&ctx.library, book_id, &self.transcribe).await? else {
            return Ok(StageOutcome::Skipped);
        };

        // Per-file transcribe loop. Each active file gets its
        // own analyzer session via AVAssetReader streaming
        // (3D.3); we rebase the file-relative segment timestamps
        // into the book's global time-base using each file's
        // `cumulative_offset_secs`. File boundaries fall at
        // chapter breaks — natural reset points for the
        // transcriber, so per-file sessions don't risk the
        // chunk-boundary artifacts the AVAssetReader rewrite
        // solved for the single-file streaming case.
        tracing::debug!(
            book = %book_id,
            file_count = plan.files.len(),
            total = crate::multi_file::total_duration_secs(&plan.files),
            "transcribe.full.start"
        );
        let mut all_segments: Vec<TranscriptSegment> = Vec::new();
        for file in &plan.files {
            match transcribe_window_typed(&file.path, 0.0, file.duration_secs, &plan.locale).await {
                Ok(mut segs) => {
                    crate::multi_file::rebase_segments(&mut segs, file.cumulative_offset_secs);
                    all_segments.append(&mut segs);
                }
                Err(BridgeError::ModelNotInstalled) => {
                    // Idle installer queued this locale via
                    // head/tail (3A.4.1). Bail; we get re-queued
                    // when the model lands.
                    tracing::warn!(
                        locale = %plan.locale,
                        book = %book_id,
                        path = %file.path.display(),
                        "transcribe.full.skip.model_not_installed"
                    );
                    return Ok(StageOutcome::Skipped);
                }
                Err(e) => return Err(e.into()),
            }
        }

        if all_segments.is_empty() {
            // Engine returned nothing — unusual but possible for
            // ambient-only audio. Don't write an empty row.
            tracing::warn!(book = %book_id, "transcribe.full.no_segments");
            return Ok(StageOutcome::Skipped);
        }

        write_full_cache(
            &ctx.library,
            book_id,
            &all_segments,
            &plan.locale,
            &self.transcribe.extractor_version,
        )
        .await?;
        Ok(StageOutcome::Done)
    }
}

#[derive(Debug)]
struct FullPlan {
    /// Every active file in book order, with cumulative offsets
    /// pre-computed for segment rebasing.
    files: Vec<crate::multi_file::FileEntry>,
    locale: String,
}

/// Pull file path + total duration from the library DB and the
/// locale from the head transcript's `locale` column.
/// Returns `None` when the book should be skipped (no head
/// transcript, no active file, duration below threshold, or the
/// full transcript is already cached at the current
/// `extractor_version` + locale).
async fn plan_full(
    library: &LibraryDb,
    book_id: BookId,
    transcribe: &TranscribeTunables,
) -> Result<Option<FullPlan>> {
    let id = book_id.0;

    // Locale from head transcript's `locale` column (B2 — no
    // longer embedded in the JSON payload).
    let head_cache = CacheKey::TranscriptHead.as_str();
    let head_row = sqlx::query!(
        "SELECT locale FROM ai_cache WHERE book_id = ? AND cache_type = ?",
        id,
        head_cache,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("transcribe-full head lookup: {e}")))?;
    let Some(Some(locale)) = head_row.map(|r| r.locale) else {
        return Ok(None);
    };

    // Idempotency.
    if full_cache_fresh(library, book_id, &transcribe.extractor_version, &locale).await? {
        return Ok(None);
    }

    // Per-file plan via the shared multi_file helper. Each
    // entry carries duration + cumulative offset; we iterate in
    // run() and rebase per-file segments via the offset.
    let files = crate::multi_file::active_files(library, book_id).await?;
    if files.is_empty() {
        return Ok(None);
    }
    let total_secs = crate::multi_file::total_duration_secs(&files);
    if total_secs < transcribe.min_duration_secs {
        return Ok(None);
    }

    Ok(Some(FullPlan { files, locale }))
}

/// Same shape as `stage::cache_fresh` but specialised to the
/// full transcript row. Reads `locale` from the column (B2).
async fn full_cache_fresh(
    library: &LibraryDb,
    book_id: BookId,
    extractor_version: &str,
    current_locale: &str,
) -> Result<bool> {
    let id = book_id.0;
    let full_cache = CacheKey::TranscriptFull.as_str();
    let row = sqlx::query!(
        "SELECT extractor_version, locale FROM ai_cache WHERE book_id = ? AND cache_type = ?",
        id,
        full_cache,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("transcribe-full cache lookup: {e}")))?;
    let Some(row) = row else { return Ok(false) };
    if row.extractor_version.as_deref() != Some(extractor_version) {
        return Ok(false);
    }
    Ok(row.locale.as_deref() == Some(current_locale))
}

#[derive(Debug, Serialize)]
struct FullPayload<'a> {
    segments: &'a [TranscriptSegment],
}

async fn write_full_cache(
    library: &LibraryDb,
    book_id: BookId,
    segments: &[TranscriptSegment],
    locale: &str,
    extractor_version: &str,
) -> Result<()> {
    let payload = FullPayload { segments };
    let bytes = serde_json::to_vec(&payload)
        .map_err(|e| Error::stage("transcribe-full", format!("encode payload: {e}")))?;
    let conf = mean_confidence(segments);
    let id = book_id.0;
    // `compressed = 0`: a 5-hour book is ~3 MB of JSON, a 30-hour
    // book ~18 MB. SQLite BLOBs handle this fine. Turning on zstd
    // compression is a follow-up — wants benchmarking on
    // representative books before changing the storage contract.
    let full_cache = CacheKey::TranscriptFull.as_str();
    sqlx::query!(
        "INSERT OR REPLACE INTO ai_cache \
         (book_id, cache_type, content, compressed, confidence, extractor_version, locale) \
         VALUES (?, ?, ?, 0, ?, ?, ?)",
        id,
        full_cache,
        bytes,
        conf,
        extractor_version,
        locale,
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
