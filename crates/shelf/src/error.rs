//! HTTP-error mapping for the shelf bridge.
//!
//! Smaller than `ab_api::ApiError` — we don't surface 401/403
//! yet (auth lands in C1b), and the bridge consumers expect
//! ABS-shaped error JSON, which is simpler than RFC 7807. ABS
//! tends to return either a plain text error body or `{ "error":
//! "<message>" }`; we go with the JSON shape.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// Typed error variants the shelf handlers can return.
#[derive(Debug, thiserror::Error)]
pub enum ShelfError {
    /// Database lookup failed (5xx).
    #[error("database error: {0}")]
    Database(String),
    /// Requested item / library / file doesn't exist (404).
    #[error("{0}")]
    NotFound(String),
    /// Operator bug — bad path / query / body (400).
    #[error("{0}")]
    BadRequest(String),
    /// Filesystem read failed on file-streaming (5xx, log
    /// path on the way out).
    #[error("filesystem error: {0}")]
    FileSystem(String),
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

impl IntoResponse for ShelfError {
    fn into_response(self) -> Response {
        let (status, log_only) = match &self {
            Self::NotFound(_) => (StatusCode::NOT_FOUND, false),
            Self::BadRequest(_) => (StatusCode::BAD_REQUEST, false),
            Self::Database(_) | Self::FileSystem(_) => (StatusCode::INTERNAL_SERVER_ERROR, true),
        };
        let msg = self.to_string();
        if log_only {
            tracing::error!(error = %msg, "shelf.error");
        } else {
            tracing::info!(error = %msg, "shelf.reject");
        }
        let body = ErrorBody { error: msg };
        (status, Json(body)).into_response()
    }
}
