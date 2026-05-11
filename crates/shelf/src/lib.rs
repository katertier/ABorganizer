//! Audiobookshelf-compatible API translation.
//!
//! See `docs/CLIENT-MATRIX.md` for the supported endpoint subset and
//! tested clients. Endpoints not in the subset return HTTP 501 with
//! a stable error code so clients can degrade.
//!
//! Pinned to **ABS API v2.x** ABI shape (their server source serves as
//! the reference; we mirror enough of `/api/items`, `/api/libraries`,
//! `/api/session`, `/api/me/progress`, `/api/playlists` for the
//! tested clients to function).

#![allow(missing_docs)] // scaffold

use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

/// Pinned ABS API major version we mirror.
pub const ABS_API_VERSION: &str = "2";

/// Build the ABS-compat router. Mount at root (ABS clients expect
/// `/api/items/...` at the host root, not under a prefix).
pub fn build_router() -> Router {
    Router::new()
        .route("/api/info", get(info))
        .route("/healthcheck", get(healthcheck))
}

#[derive(Serialize)]
struct AbsInfo {
    server_version: &'static str,
    api_version: &'static str,
    app: &'static str,
    note: &'static str,
}

async fn info() -> Json<AbsInfo> {
    Json(AbsInfo {
        server_version: ab_core::build_info::VERSION,
        api_version: ABS_API_VERSION,
        app: ab_core::build_info::APP_NAME,
        note: "Audiobookshelf-compatible subset; see docs/CLIENT-MATRIX.md",
    })
}

async fn healthcheck() -> &'static str {
    "OK"
}
