//! Player + library browser web UI.
//!
//! Built separately from the Rust binary: the `frontend/` directory
//! holds a Svelte 5 + Bun project that produces static assets in
//! `static/`. This Rust crate just serves those static files.
//!
//! See `frontend/README.md` for `bun install` / `bun run build`.

use axum::Router;

/// Mount the player UI under `/player`. Serves the prebuilt static
/// bundle in `static/`. Returns a stub router until the build is
/// wired into the build pipeline.
pub fn build_router() -> Router {
    Router::new()
}
