//! Stage trait + per-stage context.

use std::sync::Arc;

use async_trait::async_trait;

use ab_core::{BookId, Result};
use ab_db::{EphemeralDb, LibraryDb};

/// Typed stage identifier.
///
/// Every pipeline stage exposes a `pub const STAGE_ID: StageId`
/// constant. [`Stage::requires`] returns `&'static [StageId]`,
/// so cross-stage dependencies are stored as the typed
/// identifier rather than the loose `&'static str` the old API
/// used. Renaming a stage now means changing its `STAGE_ID`
/// once; dependents either compile against the new symbol or
/// fail at compile time. The previous "rename a string and
/// silently break the DAG" failure mode is gone.
///
/// The wrapped string is the canonical name written to
/// `pipeline_progress.stage` and surfaced in `tracing` fields.
/// Convert with [`StageId::as_str`] / `Display` / `AsRef<str>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StageId(&'static str);

impl StageId {
    /// Construct from a static string. `const`, so stages can
    /// `pub const STAGE_ID: StageId = StageId::new("…")`.
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        Self(name)
    }

    /// The wrapped name as it lives in `pipeline_progress` /
    /// tracing / job submission.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl std::fmt::Display for StageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl AsRef<str> for StageId {
    fn as_ref(&self) -> &str {
        self.0
    }
}

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
    /// Typically `Self::STAGE_ID.as_str()` — each stage exposes a
    /// `pub const STAGE_ID: StageId` constant.
    fn name(&self) -> &'static str;

    /// Stages whose completion this one depends on. Empty
    /// vector means no dependencies (root stage). Returning
    /// [`StageId`]s (not free strings) means renaming a stage
    /// in one place propagates as a compile-time check at every
    /// dependent.
    fn requires(&self) -> &'static [StageId];

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
