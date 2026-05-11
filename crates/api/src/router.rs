//! Top-level axum Router builder.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::state::ApiState;

/// Build the native API router. Mount at `/api/v1`.
pub fn build_router(state: ApiState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .with_state(state)
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
    uptime_secs: u64,
    app: &'static str,
    version: &'static str,
}

async fn health(State(state): State<ApiState>) -> Json<Health> {
    let uptime = state.inner.started_at.elapsed().as_secs();
    Json(Health {
        status: "ok",
        uptime_secs: uptime,
        app: ab_core::build_info::APP_NAME,
        version: ab_core::build_info::VERSION,
    })
}

#[derive(Serialize)]
struct VersionInfo {
    name: &'static str,
    #[serde(rename = "version")]
    semver: &'static str,
    description: &'static str,
}

async fn version() -> Json<VersionInfo> {
    Json(VersionInfo {
        name: ab_core::build_info::APP_NAME,
        semver: ab_core::build_info::VERSION,
        description: ab_core::build_info::DESCRIPTION,
    })
}
