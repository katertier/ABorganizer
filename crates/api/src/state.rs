//! Shared API state passed to every handler.

use std::sync::Arc;

use ab_background::BackgroundRegistry;
use ab_core::tunables::SecurityTunables;
use ab_db::{EphemeralDb, LibraryDb};
use ab_pipeline::cleanup::CleanupRegistry;
use ab_pipeline::{Dag, Scheduler};
use globset::GlobSet;
use tokio_util::sync::CancellationToken;

use crate::doctor::DoctorRegistry;
use crate::rate_limit::RateLimiter;

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
    /// Security knobs (bearer-token + library-root allowlist).
    /// Consumed by the auth middleware in [`crate::auth`] and
    /// by the `library_scan` handler's path-validation guard.
    pub security: SecurityTunables,
    /// Daemon start time (for `/health` uptime).
    pub started_at: std::time::Instant,
    /// Rate limiter for the anonymous
    /// `POST /api/v1/pairing/consume` endpoint. Defaults to
    /// [`crate::rate_limit::DEFAULT_PAIRING_CONSUME_LIMIT`]
    /// failed attempts per
    /// [`crate::rate_limit::DEFAULT_PAIRING_CONSUME_WINDOW`].
    /// Initialised inline by [`ApiState::new`] so the existing
    /// constructor signature is unchanged.
    pub pairing_consume_limiter: Arc<RateLimiter>,
    /// Compiled watch-folder exclusion globs from
    /// `PipelineTunables.scan_excludes` (B.4, tracker #119). The
    /// `library_scan` handler passes this through to
    /// `ab_scan::scan_with_excludes`. Compiled once at boot;
    /// empty `GlobSet` disables exclusions (suitable for tests).
    pub scan_excludes: GlobSet,
    /// Background-task registry (ADR-0035). Shared with the
    /// daemon's scheduling loop so the API surface
    /// (`/background/tasks` + manual triggers) and the
    /// autonomous tick agree on the registered set.
    pub background: BackgroundRegistry,
    /// Registered doctor checks (ADR-0037, B.9). Read-only by
    /// trait contract; consumed by `/doctor`, `/doctor/all`,
    /// `/doctor/{name}` handlers.
    pub doctor: DoctorRegistry,
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
        security: SecurityTunables,
        scan_excludes: GlobSet,
        background: BackgroundRegistry,
        doctor: DoctorRegistry,
    ) -> Self {
        Self {
            inner: Arc::new(ApiStateInner {
                library,
                ephemeral,
                scheduler,
                dag,
                cleanup,
                cancel,
                security,
                started_at: std::time::Instant::now(),
                pairing_consume_limiter: Arc::new(RateLimiter::default()),
                scan_excludes,
                background,
                doctor,
            }),
        }
    }
}
