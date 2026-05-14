//! Typed API errors that render to RFC 7807 Problem Details responses.

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
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
    /// Caller exceeded the rate budget for an endpoint. Carries
    /// the `Retry-After` value the response should advertise
    /// (always ≥ 1 second). Today only emitted by
    /// `POST /pairing/consume` via
    /// [`crate::rate_limit::RateLimiter`].
    #[error("too many requests: retry after {retry_after_secs}s")]
    RateLimited {
        /// Seconds the client should wait before retrying.
        /// Lands as the `Retry-After` HTTP header.
        retry_after_secs: u64,
    },
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
            Self::RateLimited { retry_after_secs } => {
                // 429 with Retry-After header is the wire-format
                // standard. Branch separately because we need to
                // mount the header on the response; every other
                // variant only sets a status + body.
                let problem = Problem {
                    kind: "about:blank#rate-limited",
                    title: "Too Many Requests",
                    status: 429,
                    detail: self.to_string(),
                };
                let mut resp = (StatusCode::TOO_MANY_REQUESTS, Json(problem)).into_response();
                // `Retry-After: <seconds>` — RFC 9110 § 10.2.3.
                // HeaderValue::from is infallible for u64 via the
                // i64 / u64 numeric conversion impls; *retry_after_secs
                // is bounded ≤ window length so it fits in any reasonable HeaderValue.
                resp.headers_mut()
                    .insert(header::RETRY_AFTER, HeaderValue::from(*retry_after_secs));
                return resp;
            }
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
