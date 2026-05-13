//! Typed API errors that render to RFC 7807 Problem Details responses.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// Wire format for API errors. Matches `application/problem+json`.
#[derive(Debug, Serialize)]
pub struct Problem {
    /// URI reference identifying the problem type.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Short, human-readable summary.
    pub title: &'static str,
    /// HTTP status code.
    pub status: u16,
    /// Detailed explanation specific to this occurrence.
    pub detail: String,
}

/// Top-level API error.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ApiError {
    /// Caller is unauthenticated.
    #[error("unauthorized")]
    Unauthorized,
    /// Caller authenticated but lacks the required scope.
    #[error("forbidden")]
    Forbidden,
    /// Requested resource was not found.
    #[error("not found: {0}")]
    NotFound(String),
    /// Request body was malformed.
    #[error("bad request: {0}")]
    BadRequest(String),
    /// Request conflicts with current resource state — e.g. the
    /// audiologo endpoint refusing to apply a second cut to a
    /// `(file_id, kind)` pair that already has one applied. The
    /// caller resolves by issuing a reject / re-detect first.
    #[error("conflict: {0}")]
    Conflict(String),
    /// Underlying core error.
    #[error("internal: {0}")]
    Internal(#[from] ab_core::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, problem) = match &self {
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                Problem {
                    kind: "about:blank#unauthorized",
                    title: "Unauthorized",
                    status: 401,
                    detail: self.to_string(),
                },
            ),
            Self::Forbidden => (
                StatusCode::FORBIDDEN,
                Problem {
                    kind: "about:blank#forbidden",
                    title: "Forbidden",
                    status: 403,
                    detail: self.to_string(),
                },
            ),
            Self::NotFound(_) => (
                StatusCode::NOT_FOUND,
                Problem {
                    kind: "about:blank#not-found",
                    title: "Not Found",
                    status: 404,
                    detail: self.to_string(),
                },
            ),
            Self::BadRequest(_) => (
                StatusCode::BAD_REQUEST,
                Problem {
                    kind: "about:blank#bad-request",
                    title: "Bad Request",
                    status: 400,
                    detail: self.to_string(),
                },
            ),
            Self::Conflict(_) => (
                StatusCode::CONFLICT,
                Problem {
                    kind: "about:blank#conflict",
                    title: "Conflict",
                    status: 409,
                    detail: self.to_string(),
                },
            ),
            Self::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Problem {
                    kind: "about:blank#internal",
                    title: "Internal Server Error",
                    status: 500,
                    detail: self.to_string(),
                },
            ),
        };
        (status, Json(problem)).into_response()
    }
}
