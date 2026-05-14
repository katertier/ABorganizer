//! Bearer-token authentication middleware.
//!
//! Backlog item 4a wires this up to the `tokens` table (blake3-
//! hashed, per-user, revocable via `DELETE /api/v1/tokens/{id}`).
//! Every request except the public allow-list must carry
//! `Authorization: Bearer <token>` matching an active row.
//!
//! ## Resolution order
//!
//! 1. Hash the presented bearer with [`ab_core::auth::hash_api_token`]
//!    and look it up via [`crate::tokens::lookup_by_raw_token`].
//!    On hit, the request is authenticated and an
//!    [`crate::tokens::AuthenticatedToken`] is stashed in the
//!    request extensions (downstream handlers can read
//!    `user_id` / `scopes` without re-hashing).
//! 2. **Compat fallback**: if step 1 didn't match AND the
//!    `tokens` table has no rows yet AND
//!    `tunables.security.admin_token` is set AND the presented
//!    bearer matches it constant-time → accept. This is the
//!    one-cycle bridge that lets operators bootstrap their
//!    first per-user token without needing to construct the
//!    initial row via raw SQL.
//!
//! Once `tokens` has at least one row, the compat fallback
//! stops engaging — the operator has rotated.
//!
//! ## Allow-list
//!
//! Two paths bypass auth, per `API.md` and `ARCHITECTURE.md`
//! § Health-checks:
//!
//! - `GET /health` — liveness probe (uptime / version readout).
//! - `GET /version` — version sniff (CLI uses this to detect
//!   protocol drift before pairing).
//!
//! Both are read-only and reveal only generic version metadata.
//!
//! ## Default-deny on missing config
//!
//! When the tokens table is empty AND `admin_token` is unset,
//! every protected request returns 401. The daemon's startup
//! logs a `warn` line when both are missing so operators see
//! the gap. The previous behaviour (no auth middleware at all)
//! made every endpoint open; the new default fails closed.
//!
//! ## Constant-time comparison
//!
//! - Step 1 (token-table path): blake3 of the presented bearer
//!   is constant-time-compared inside `verify_api_token`. Length
//!   leak isn't a concern — the hash is always 64 hex chars.
//! - Step 2 (compat fallback): byte-by-byte XOR-then-OR fold so
//!   the timing of a wrong `admin_token` doesn't leak the matching
//!   prefix length. Length-mismatch fast-path leaks length only,
//!   and the `admin_token` is fixed-length per operator config.

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
/// require_token)` in `build_router`. See module docs for the
/// two-step resolution order (tokens table → `admin_token` compat
/// fallback when tokens is empty).
pub async fn require_token(
    State(state): State<ApiState>,
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
            "api.auth.reject_missing_bearer"
        );
        return unauthorized_response();
    };

    // Step 1: token-table lookup. The lookup hashes the bearer
    // with blake3 and filters revoked / expired in SQL.
    match crate::tokens::lookup_by_raw_token(&state, presented).await {
        Ok(Some(auth)) => {
            // Stash for downstream handlers (scope-checked
            // middleware lands with the player surface).
            request.extensions_mut().insert(auth);
            return next.run(request).await;
        }
        Ok(None) => {
            // Fall through to compat fallback below.
        }
        Err(e) => {
            tracing::error!(
                path = %path,
                error = %e,
                "api.auth.lookup_failed"
            );
            return unauthorized_response();
        }
    }

    // Step 2: compat fallback. Only fires when (a) `admin_token` is
    // set in tunables AND (b) the tokens table is empty (i.e.
    // operator hasn't rotated yet). Once they POST /tokens once,
    // this branch stops engaging.
    if let Some(expected) = state.inner.security.admin_token.as_deref()
        && tokens_table_is_empty(&state).await
        && constant_time_eq(presented.as_bytes(), expected.as_bytes())
    {
        tracing::debug!(
            path = %path,
            method = %request.method(),
            "api.auth.admin_token_compat_accepted"
        );
        return next.run(request).await;
    }

    tracing::info!(
        path = %path,
        method = %request.method(),
        "api.auth.reject_bad_token"
    );
    unauthorized_response()
}

/// True iff `tokens` has no rows. Read fresh per request; the
/// extra SELECT only fires on the compat-fallback branch (every
/// rejected token-table lookup). Counts are cheap on an empty /
/// near-empty table.
async fn tokens_table_is_empty(state: &ApiState) -> bool {
    let r: Result<i64, _> = sqlx::query_scalar("SELECT COUNT(*) FROM tokens")
        .fetch_one(state.inner.library.pool())
        .await;
    match r {
        Ok(0) => true,
        Ok(_) => false,
        Err(e) => {
            tracing::warn!(error = %e, "api.auth.tokens_count_failed");
            // Fail closed: if we can't count, don't engage the
            // fallback (forces a real token-table lookup, which
            // already failed).
            false
        }
    }
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
