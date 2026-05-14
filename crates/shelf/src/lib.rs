//! Audiobookshelf-compatible API translation.
//!
//! See `docs/CLIENT-MATRIX.md` for the supported endpoint subset
//! and tested clients. Endpoints not in the subset return HTTP
//! 501 with a stable error code so clients can degrade.
//!
//! Pinned to **ABS API v2.x** ABI shape (their server source
//! serves as the reference; we mirror enough of `/api/items`,
//! `/api/libraries`, `/api/session`, `/api/me/progress`,
//! `/api/playlists` for the tested clients to function).
//!
//! ## C1 MVP scope (this slice)
//!
//! Three read-only endpoints sit on top of the existing
//! `/api/info` + `/healthcheck` scaffold:
//!
//! - `GET /api/libraries` — single fixed library entry.
//! - `GET /api/items/{id}` — book detail in ABS JSON shape.
//! - `GET /api/items/{id}/file/{ino}` — stream the file (whole
//!   file; Range support deferred to C1b).
//!
//! Auth is **deferred to C1b**. The daemon's default bind is
//! `127.0.0.1` (loopback), so the MVP is safe-by-default on
//! the dev box. Operators who flip `server.bind` to `0.0.0.0`
//! before C1b lands need a layer-7 reverse proxy with bearer
//! enforcement, or to keep `server.abs_enabled = false`.

#![allow(missing_docs)] // scaffold

use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

pub mod error;
pub mod files;
#[cfg(test)]
mod integration_tests;
pub mod items;
pub mod libraries;
pub mod state;

pub use state::ShelfState;

/// Pinned ABS API major version we mirror.
pub const ABS_API_VERSION: &str = "2";

/// Build the ABS-compat router. Mount at root (ABS clients
/// expect `/api/items/...` at the host root, not under a
/// prefix).
///
/// `state` carries the library DB handle threaded into every
/// data-reading handler. Future C1b slice adds an auth layer
/// on top; the state shape stays.
pub fn build_router(state: ShelfState) -> Router {
    Router::new()
        .route("/api/info", get(info))
        .route("/healthcheck", get(healthcheck))
        .route("/api/libraries", get(libraries::list_libraries))
        // axum 0.7 (matchit 0.7) uses `:param` syntax for
        // captures. `{id}` would match the literal string,
        // silently 404-ing every real request. Caught by the
        // shelf integration tests; pre-existing api-crate
        // routes carried the same bug — fixed in this slice.
        .route("/api/items/:id", get(items::get_item))
        .route("/api/items/:id/file/:ino", get(files::stream_file))
        .with_state(state)
}

#[derive(Serialize)]
struct AbsInfo {
    server_version: &'static str,
    api_version: &'static str,
    app: &'static str,
    note: &'static str,
}

#[allow(clippy::unused_async, reason = "axum handler signature parity")]
async fn info() -> Json<AbsInfo> {
    Json(AbsInfo {
        server_version: ab_core::build_info::VERSION,
        api_version: ABS_API_VERSION,
        app: ab_core::build_info::APP_NAME,
        note: "Audiobookshelf-compatible subset; see docs/CLIENT-MATRIX.md",
    })
}

#[allow(clippy::unused_async, reason = "axum handler signature parity")]
async fn healthcheck() -> &'static str {
    "OK"
}
