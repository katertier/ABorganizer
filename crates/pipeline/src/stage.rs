//! Stage trait + per-stage context.

use std::sync::Arc;

use async_trait::async_trait;

use ab_core::{BookId, Result};
use ab_db::{EphemeralDb, LibraryDb};

/// What every stage gets at run time. Shared, cheap to clone.
#[derive(Clone)]
pub struct StageContext {
    /// Persistent library DB.
    pub library: LibraryDb,
    /// Restartable state DB.
    pub ephemeral: EphemeralDb,
    /// Stop-token; stages check `is_cancelled()` periodically.
    pub cancel: tokio_util::sync::CancellationToken,
    /// Stage name (passed in by the executor).
    pub stage_name: &'static str,
}

/// Outcome of a single stage invocation. We don't return heavy data —
/// the stage has already persisted it to storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageOutcome {
    /// Work completed; mark stage as succeeded for this book.
    Done,
    /// Stage was skipped (e.g., already complete on a re-run). Same
    /// as `Done` from the scheduler's perspective, but logged
    /// separately.
    Skipped,
    /// Resumable work pending: the stage made progress but isn't done
    /// yet. Stage runner re-queues for next iteration. Used by
    /// chunked transcription.
    Continue,
}

/// A pipeline stage. Implementations live in feature crates and are
/// registered in the daemon's wiring.
#[async_trait]
pub trait Stage: Send + Sync + 'static {
    /// Unique stage name. Used as a key in `pipeline_progress`.
    fn name(&self) -> &'static str;

    /// Names of stages whose completion this one depends on. Empty
    /// vector means no dependencies (root stage).
    fn requires(&self) -> &'static [&'static str];

    /// Run the stage for one book.
    ///
    /// Heavy I/O is local to this method. The stage MUST persist all
    /// outputs to `ctx.library` / `ctx.ephemeral` / filesystem before
    /// returning. Anything held only in memory is lost on the next
    /// daemon restart.
    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome>;
}

/// Type-erased stage registration record.
pub(crate) struct StageEntry {
    pub(crate) stage: Arc<dyn Stage>,
}
