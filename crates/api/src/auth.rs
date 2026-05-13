//! Bearer-token authentication middleware.
//!
//! Slice-zero of the security story (REVIEW.md § 3.2 +
//! MYREVIEW.md § 3.1). Until the per-user token table + pairing
//! flow ship, the daemon supports a single `admin_token`
//! configured via `tunables.security.admin_token`. Every API
//! request except the explicit allow-list must carry
//! `Authorization: Bearer <token>` matching that value.
//!
//! ## Allow-list
//!
//! Two paths are intentionally unauthenticated, per `API.md`
//! and the `ARCHITECTURE.md` § Health-checks note:
//!
//! - `GET /health` — liveness probe (uptime / version readout).
//! - `GET /version` — version sniff (used by the CLI to detect
//!   protocol drift before pairing).
//!
//! Both are read-only and reveal only generic version metadata.
//! When the bind address is non-loopback, operators can layer
//! a reverse-proxy rate limit on these two endpoints; the
//! daemon itself doesn't (the rest of the surface is
//! authenticated, so a `DoS` via auth'd endpoints needs the
//! token anyway).
//!
//! ## Default-deny on missing config
//!
//! If `tunables.security.admin_token` is `None`, the middleware
//! rejects every request to a protected path with 401. The
//! daemon's startup logs a `warn` line when this happens so
//! operators see the gap. The previous behaviour (no auth
//! middleware at all) made every endpoint open; the new default
//! is to fail closed.
//!
//! ## Constant-time comparison
//!
//! Token comparison uses a byte-by-byte XOR-then-OR fold so
//! the timing of a wrong token doesn't leak the matching
//! prefix length. A length-mismatch fast-path before the loop
//! still leaks length, but that's a single bit per probe and
//! the admin token is fixed-length per operator config — the
//! attack budget is one prefix-length probe per token rotation.

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

use crate::state::ApiState;

/// Paths that bypass auth. Order-independent; exact match only
/// (no prefix tricks — every protected handler that nests under
/// these would also be exempt, which we don't want).
const PUBLIC_PATHS: &[&str] = &["/health", "/version"];

/// True iff `request_path` is on the allow-list.
fn is_public(request_path: &str) -> bool {
    PUBLIC_PATHS.contains(&request_path)
}

/// Constant-time byte compare. Returns `true` iff `a` and `b`
/// have the same length and every byte matches.
///
/// `subtle` crate would be the textbook answer; rolling a tiny
/// fold here avoids a new dep for one comparison. Length leak
/// is acknowledged in the module docs.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Extract the bearer token from `Authorization: Bearer <token>`.
/// Returns `None` if the header is missing, the scheme isn't
/// `Bearer`, or the value is empty.
fn extract_bearer(headers: &axum::http::HeaderMap) -> Option<&str> {
    let value = headers.get(axum::http::header::AUTHORIZATION)?;
    let s = value.to_str().ok()?;
    let token = s.strip_prefix("Bearer ")?.trim();
    if token.is_empty() { None } else { Some(token) }
}

/// Auth middleware applied to every protected route.
///
/// Wired via `axum::middleware::from_fn_with_state(state.clone(),
/// require_admin_token)` in `build_router`. The state extractor
/// gives us access to the configured token without leaking it
/// into request extensions.
pub async fn require_admin_token(
    State(state): State<ApiState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let path = request.uri().path();
    if is_public(path) {
        return next.run(request).await;
    }

    let Some(expected) = state.inner.security.admin_token.as_deref() else {
        // Default-deny: no token configured ⇒ no requests.
        tracing::warn!(
            path = %path,
            method = %request.method(),
            "api.auth.reject_no_token_configured"
        );
        return unauthorized_response();
    };

    let Some(presented) = extract_bearer(request.headers()) else {
        tracing::info!(
            path = %path,
            method = %request.method(),
            "api.auth.reject_missing_bearer"
        );
        return unauthorized_response();
    };

    if !constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
        tracing::info!(
            path = %path,
            method = %request.method(),
            "api.auth.reject_bad_token"
        );
        return unauthorized_response();
    }

    next.run(request).await
}

fn unauthorized_response() -> Response {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(axum::http::header::WWW_AUTHENTICATE, "Bearer")
        .body(Body::from("Unauthorized"))
        .unwrap_or_else(|_| {
            // Builder can't fail with these inputs, but the
            // type system makes us handle it. Fall back to the
            // bare-bytes path.
            Response::new(Body::from("Unauthorized"))
        })
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn public_paths_are_allow_listed() {
        assert!(is_public("/health"));
        assert!(is_public("/version"));
        assert!(!is_public("/library/scan"));
        // No prefix trickery — exact match.
        assert!(!is_public("/health/extra"));
        assert!(!is_public("/version/something"));
    }

    #[test]
    fn constant_time_eq_basics() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn extract_bearer_handles_common_shapes() {
        use axum::http::HeaderMap;
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer token123".parse().expect("parse"),
        );
        assert_eq!(extract_bearer(&h), Some("token123"));

        let mut h2 = HeaderMap::new();
        h2.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer ".parse().expect("parse"),
        );
        assert_eq!(extract_bearer(&h2), None, "empty token after Bearer");

        let h3 = HeaderMap::new();
        assert_eq!(extract_bearer(&h3), None, "missing header");

        let mut h4 = HeaderMap::new();
        h4.insert(
            axum::http::header::AUTHORIZATION,
            "Basic abc".parse().expect("parse"),
        );
        assert_eq!(extract_bearer(&h4), None, "wrong scheme");
    }
}
