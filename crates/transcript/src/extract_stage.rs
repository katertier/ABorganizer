//! `run-transcript-extractors` pipeline stage (slice 3C).
//!
//! Loads the cached head transcript from `ai_cache`, runs every
//! registered [`crate::Extractor`] over it, writes each
//! [`crate::Candidate`] to `book_field_provenance` with the
//! extractor's name as the `source`.
//!
//! ## What ships in v1
//!
//! The default extractor set
//! ([`crate::extractors::built_in_extractors`]) covers:
//!
//! - `transcript_title_author` — title / author / narrator
//!   heuristics ("by <Author>", "read by <Narrator>", "This is
//!   <Title>").
//! - `transcript_publisher` — tier-4 audiologo (publisher
//!   keyword match in transcript text).
//!
//! Future extractors (DNA tags, spoiler-free summary, story
//! arc, person/FTS) need Apple `FoundationModels` FFI; those land
//! in a separate slice.
//!
//! ## Idempotency
//!
//! The stage reads `book_field_provenance` for each
//! `(book_id, source)` it would write. Existing rows are
//! preserved; the stage only inserts new ones. Re-running on
//! the same head transcript is a no-op.
//!
//! ## Failure modes
//!
//! - No cached head transcript → `Skipped`.
//! - Cached payload unparseable → log warning + `Skipped`.
//! - DB write failures propagate as `Err`.

use std::sync::Arc;

use async_trait::async_trait;

use ab_core::{BookId, Error, Result};
use ab_db::LibraryDb;
use ab_pipeline::{Stage, StageContext, StageOutcome};
use serde::Deserialize;

use crate::bridge::TranscriptSegment;
use crate::extractors::built_in_extractors;
use crate::stage::{CACHE_TYPE_HEAD, STAGE_NAME as HEAD_TAIL_STAGE};
use crate::{Candidate, Extractor};

/// Stage name written to `pipeline_progress` and registered with
/// the daemon.
pub const STAGE_NAME: &str = "run-transcript-extractors";

/// Runs every built-in [`Extractor`] over the cached head
/// transcript.
pub struct RunExtractorsStage {
    extractors: Vec<Arc<dyn Extractor>>,
}

impl RunExtractorsStage {
    /// Construct with the default extractor set.
    #[must_use]
    pub fn new() -> Self {
        Self {
            extractors: built_in_extractors(),
        }
    }

    /// Construct with a custom extractor list (used by tests).
    #[must_use]
    #[allow(dead_code)]
    pub fn with_extractors(extractors: Vec<Arc<dyn Extractor>>) -> Self {
        Self { extractors }
    }
}

impl Default for RunExtractorsStage {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Stage for RunExtractorsStage {
    fn name(&self) -> &'static str {
        STAGE_NAME
    }

    fn requires(&self) -> &'static [&'static str] {
        &[HEAD_TAIL_STAGE]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        let Some(text) = load_head_text(&ctx.library, book_id).await? else {
            return Ok(StageOutcome::Skipped);
        };

        for extractor in &self.extractors {
            let candidates = extractor.extract(&text);
            let source = extractor.name();
            for c in candidates {
                write_candidate(&ctx.library, book_id, source, &c).await?;
            }
        }
        Ok(StageOutcome::Done)
    }
}

/// Owned shape of the head transcript JSON payload. Mirrors
/// `TranscriptPayload` in `stage.rs` for read-side use.
#[derive(Deserialize)]
struct CachedHead {
    #[allow(dead_code)]
    locale: String,
    segments: Vec<TranscriptSegment>,
}

/// Load the cached head transcript and join its segment texts
/// with single spaces. Returns `None` when there's no row, the
/// content blob is missing, or the JSON shape is wrong (the
/// latter logs a warning so a stale schema is debuggable).
async fn load_head_text(library: &LibraryDb, book_id: BookId) -> Result<Option<String>> {
    let id = book_id.0;
    let row = sqlx::query!(
        "SELECT content FROM ai_cache WHERE book_id = ? AND cache_type = ?",
        id,
        CACHE_TYPE_HEAD,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("extractors head lookup: {e}")))?;
    let Some(row) = row else { return Ok(None) };
    let Some(bytes) = row.content else {
        return Ok(None);
    };
    let parsed: CachedHead = match serde_json::from_slice(&bytes) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(book = %book_id, error = %e, "extractors.head_parse_failed");
            return Ok(None);
        }
    };
    let mut text = String::new();
    for seg in parsed.segments {
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

/// Insert a single candidate row. Idempotent by content: we
/// SELECT-then-INSERT instead of INSERT-or-ignore because the
/// PK is auto-increment and we'd otherwise pile up duplicates
/// across re-runs.
async fn write_candidate(
    library: &LibraryDb,
    book_id: BookId,
    source: &str,
    candidate: &Candidate,
) -> Result<()> {
    let id = book_id.0;
    let exists = sqlx::query_scalar!(
        "SELECT 1 FROM book_field_provenance \
         WHERE book_id = ? AND field = ? AND value = ? AND source = ?",
        id,
        candidate.field,
        candidate.value,
        source,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("extractors candidate dedup: {e}")))?;
    if exists.is_some() {
        return Ok(());
    }
    let conf = f64::from(candidate.confidence);
    sqlx::query!(
        "INSERT INTO book_field_provenance \
         (book_id, field, value, source, confidence, is_winner) \
         VALUES (?, ?, ?, ?, ?, 0)",
        id,
        candidate.field,
        candidate.value,
        source,
        conf,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("extractors candidate write: {e}")))?;
    Ok(())
}
