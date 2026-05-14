//! Shared state passed to every shelf handler.
//!
//! Mirrors `ab_api::ApiState`'s shape on a much smaller surface
//! — the shelf bridge only needs read-only access to `library`
//! today. We deliberately don't share `ab_api::ApiState`
//! directly: that would drag `ab-api`'s transitive deps
//! (lofty, mp4ameta, etc.) into the shelf compile graph, which
//! adds ~20 crates for zero functional benefit. The trade-off
//! is that the auth middleware needs its own implementation
//! when it lands — tracked as C1b.

use std::sync::Arc;

use ab_db::LibraryDb;

/// State threaded through the shelf router.
///
/// Cheap to clone via `Arc`.
#[derive(Clone)]
pub struct ShelfState {
    inner: Arc<ShelfStateInner>,
}

struct ShelfStateInner {
    /// Persistent library DB. Read-only from shelf — the bridge
    /// never mutates `books` / `book_files`; mutations flow through
    /// the native API on the main port.
    library: LibraryDb,
}

impl ShelfState {
    /// Construct.
    #[must_use]
    pub fn new(library: LibraryDb) -> Self {
        Self {
            inner: Arc::new(ShelfStateInner { library }),
        }
    }

    /// Read-only handle on the library DB.
    #[must_use]
    pub fn library(&self) -> &LibraryDb {
        &self.inner.library
    }
}

impl std::fmt::Debug for ShelfState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShelfState").finish_non_exhaustive()
    }
}
