//! Shared API state passed to every handler.

use std::sync::Arc;

use ab_db::{EphemeralDb, LibraryDb};

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
    /// Daemon start time (for `/health` uptime).
    pub started_at: std::time::Instant,
}

impl ApiState {
    /// Construct shared state.
    pub fn new(library: LibraryDb, ephemeral: EphemeralDb) -> Self {
        Self {
            inner: Arc::new(ApiStateInner {
                library,
                ephemeral,
                started_at: std::time::Instant::now(),
            }),
        }
    }
}
