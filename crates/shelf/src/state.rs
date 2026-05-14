//! Shared state passed to every shelf handler.
//!
//! Mirrors `ab_api::ApiState`'s shape on a much smaller surface
//! — the shelf bridge only needs read-only access to `library`
//! today. We deliberately don't share `ab_api::ApiState`
//! directly: that would drag `ab-api`'s transitive deps
//! (lofty, mp4ameta, etc.) into the shelf compile graph, which
//! adds ~20 crates for zero functional benefit. The trade-off
//! taken in slice C1b: the auth middleware re-implements
//! orchestration here, but the token-lookup helpers themselves
//! live in [`ab_db::tokens`] so the two surfaces share the
//! actual DB code.

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
