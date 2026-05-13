//! Shared API state passed to every handler.

use std::sync::Arc;

use ab_db::{EphemeralDb, LibraryDb};
use ab_pipeline::cleanup::CleanupRegistry;
use ab_pipeline::{Dag, Scheduler};
use tokio_util::sync::CancellationToken;

/// Application state injected into every handler via axum's `State<>`.
#[derive(Clone)]
pub struct ApiState {
    /// Inner state (Arc-wrapped so the whole thing is cheap to clone).
    pub inner: Arc<ApiStateInner>,
}

/// Fields available to every API handler.
pub struct ApiStateInner {
    /// Library DB pool.
    pub library: LibraryDb,
    /// Ephemeral DB pool.
    pub ephemeral: EphemeralDb,
    /// Pipeline scheduler — handlers submit `BookId`s here to drive
    /// downstream stages.
    pub scheduler: Arc<Scheduler>,
    /// Pipeline DAG — handlers consult this to resolve user-supplied
    /// stage names (e.g. the retry endpoint, ADR-0023) into the typed
    /// [`ab_pipeline::StageId`] the scheduler requires.
    pub dag: Arc<Dag>,
    /// Registered cleanup targets (slice H.2.3, ADR-0025). The
    /// periodic loop owns its own clone of this; the API surface
    /// here drives the on-demand `aborg clean ...` flow.
    pub cleanup: CleanupRegistry,
    /// Daemon-wide cancellation token. Cloned (not constructed
    /// fresh) into every `StageContext` produced by an HTTP
    /// handler so retry-triggered stage work participates in
    /// graceful shutdown — per `ARCHITECTURE.md` § Signals,
    /// SIGTERM cancels the token and every long-running task
    /// races to clean shutdown.
    pub cancel: CancellationToken,
    /// Daemon start time (for `/health` uptime).
    pub started_at: std::time::Instant,
}

impl ApiState {
    /// Construct shared state.
    ///
    /// `cancel` must be the daemon's root cancellation token (not
    /// a fresh one). Handlers that spawn pipeline work clone this
    /// into their `StageContext` so SIGTERM-driven shutdown
    /// propagates into long-running retry / cleanup flows.
    #[allow(
        clippy::too_many_arguments,
        reason = "ApiState wires together six unavoidable runtime singletons; a builder would only relocate the count"
    )]
    pub fn new(
        library: LibraryDb,
        ephemeral: EphemeralDb,
        scheduler: Arc<Scheduler>,
        dag: Arc<Dag>,
        cleanup: CleanupRegistry,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            inner: Arc::new(ApiStateInner {
                library,
                ephemeral,
                scheduler,
                dag,
                cleanup,
                cancel,
                started_at: std::time::Instant::now(),
            }),
        }
    }
}
