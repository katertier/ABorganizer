//! Bearer-token authentication for the shelf router (slice C1b).
//!
//! Same broad shape as `ab_api::auth` but slimmer — the shelf
//! is a read-only ABS-compat surface, so the orchestration is
//! just "look up token-table row, accept on hit, 401
//! otherwise."
//!
//! ## Allow-list
//!
//! Two paths bypass auth, matching the unauthenticated ABS
//! conventions:
//!
//! - `/healthcheck` — liveness probe (returns `OK` text).
//! - `/api/info` — version + capability sniff. ABS clients
//!   call this *before* pairing to verify they're talking to
//!   something they can speak to.
//!
//! Both are read-only and reveal only generic metadata.
//!
//! ## What this middleware does NOT do
//!
//! - **No `admin_token` compat fallback.** The api-side has it
//!   for one-cycle bootstrap; shelf assumes operators rotate
//!   to a real per-user token via the api before pointing an
//!   ABS client at the daemon. If a future operator surveys
//!   show this gap matters, the fallback is a ~20-line addition
//!   that mirrors `ab_api::auth::tokens_table_is_empty`.
//! - **No scope checks.** Every authenticated token can read
//!   every shelf endpoint. Scope-gated routes (player position
//!   write, playlist mutation) live on the api side and have
//!   their own per-scope middleware.

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

use crate::state::ShelfState;

/// Paths that bypass auth. Order-independent; exact match only.
const PUBLIC_PATHS: &[&str] = &["/healthcheck", "/api/info"];

/// True iff `request_path` is on the allow-list.
fn is_public(request_path: &str) -> bool {
    PUBLIC_PATHS.contains(&request_path)
}

/// Extract the bearer token from `Authorization: Bearer <token>`.
fn extract_bearer(headers: &axum::http::HeaderMap) -> Option<&str> {
    let value = headers.get(axum::http::header::AUTHORIZATION)?;
    let s = value.to_str().ok()?;
    let token = s.strip_prefix("Bearer ")?.trim();
    if token.is_empty() { None } else { Some(token) }
}

/// Auth middleware applied to every protected route.
///
/// Mirrors `ab_api::auth::require_token` but consults
/// [`ShelfState`] instead of `ApiState` and skips the
/// `admin_token` fallback (see module docs).
pub async fn require_token(
    State(state): State<ShelfState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let path = request.uri().path();
    if is_public(path) {
        return next.run(request).await;
    }

    let Some(presented) = extract_bearer(request.headers()) else {
        tracing::info!(
            path = %path,
            method = %request.method(),
            "shelf.auth.reject_missing_bearer"
        );
        return unauthorized_response();
    };

    match ab_db::lookup_by_raw_token(state.library().pool(), presented, ab_db::unix_now_secs())
        .await
    {
        Ok(Some(auth)) => {
            // Stash for downstream handlers — same pattern as
            // ab-api so shared scope-checked layers can live
            // anywhere.
            request.extensions_mut().insert(auth);
            next.run(request).await
        }
        Ok(None) => {
            tracing::info!(
                path = %path,
                method = %request.method(),
                "shelf.auth.reject_bad_token"
            );
            unauthorized_response()
        }
        Err(e) => {
            tracing::error!(
                path = %path,
                error = %e,
                "shelf.auth.lookup_failed"
            );
            unauthorized_response()
        }
    }
}

fn unauthorized_response() -> Response {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(axum::http::header::WWW_AUTHENTICATE, "Bearer")
        .body(Body::from("Unauthorized"))
        .unwrap_or_else(|_| Response::new(Body::from("Unauthorized")))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn public_paths_are_allow_listed() {
        assert!(is_public("/healthcheck"));
        assert!(is_public("/api/info"));
        assert!(!is_public("/api/items/1"));
        assert!(!is_public("/api/libraries"));
        // No prefix trickery.
        assert!(!is_public("/healthcheck/extra"));
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
