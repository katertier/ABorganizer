//! Shared API state passed to every handler.

use std::sync::Arc;

use ab_db::{EphemeralDb, LibraryDb};
use ab_pipeline::Scheduler;

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
    /// Daemon start time (for `/health` uptime).
    pub started_at: std::time::Instant,
}

impl ApiState {
    /// Construct shared state.
    pub fn new(library: LibraryDb, ephemeral: EphemeralDb, scheduler: Arc<Scheduler>) -> Self {
        Self {
            inner: Arc::new(ApiStateInner {
                library,
                ephemeral,
                scheduler,
                started_at: std::time::Instant::now(),
            }),
        }
    }
}
