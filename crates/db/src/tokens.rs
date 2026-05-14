//! Pool-level token-lookup helpers shared by the api + shelf
//! auth middleware.
//!
//! Backlog item 4a wired the initial implementation in
//! `ab-api`; slice C1b hoisted the helpers down here so both
//! router surfaces (api + shelf) can call them without
//! crossing crate boundaries.
//!
//! The original implementation lived in `ab-api::tokens` and
//! took `&ApiState`. Shelf can't take `ApiState` without
//! pulling the whole api compile graph in, which we explicitly
//! decline (see `shelf/src/state.rs` docstring). Lowering the
//! helpers to operate on `&SqlitePool` directly lets every
//! surface that holds a [`LibraryDb`] use them.
//!
//! ## What this module owns
//!
//! - [`AuthenticatedToken`]: the result type stashed into
//!   request extensions on a successful lookup.
//! - [`lookup_by_raw_token`]: the lookup itself — hash the
//!   presented bearer, filter revoked / expired, update
//!   `last_used_at` (best-effort), return the typed handle.
//! - [`tokens_table_is_empty`]: small predicate used by the
//!   api crate's `admin_token` compat fallback.
//!
//! ## What it deliberately does not own
//!
//! - The axum middleware itself. Each router has its own
//!   `auth.rs` because the State type differs (`ApiState`
//!   vs `ShelfState`) and the orchestration around the
//!   helpers (allow-list, `admin_token` fallback, etc.) is
//!   small per-surface decisions.

use std::sync::Arc;

use ab_core::auth::hash_api_token;
use ab_core::{Error, Result};
use sqlx::SqlitePool;

/// Result of a successful token lookup. Auth middleware stashes
/// one of these in request extensions; downstream handlers
/// read `user_id` / `scopes` without re-hashing the bearer.
///
/// Cheap to clone — `scopes` is an `Arc`.
#[derive(Debug, Clone)]
pub struct AuthenticatedToken {
    /// `tokens.token_id`. Stable across the token's lifetime
    /// (rotations get a fresh row + a fresh id).
    pub token_id: i64,
    /// `tokens.user_id`. Joins to `users.user_id` for any
    /// downstream per-user scoping.
    pub user_id: i64,
    /// Authorization scopes, parsed from the row's JSON. Empty
    /// vec means "no scopes claimed" — downstream
    /// scope-checked routes reject. Future scope middleware
    /// reads this without holding the pool.
    pub scopes: Arc<Vec<String>>,
}

/// Look up a token by its raw bearer string.
///
/// Returns `Some(AuthenticatedToken)` only when the token row
/// exists, has not been revoked, and has not expired. Updates
/// `last_used_at` on a hit (best-effort — a failure to update
/// doesn't reject the auth).
///
/// `now_secs` is the Unix-epoch second used both for the
/// expiry filter and the `last_used_at` update. Passing it in
/// rather than computing it inside the fn keeps the helper
/// testable without time travel.
///
/// # Errors
///
/// Bubbles DB errors as [`Error::Database`]. Callers should
/// treat `Err` as a 5xx (database unavailable), not a rejected
/// token — the auth middleware does exactly that.
pub async fn lookup_by_raw_token(
    pool: &SqlitePool,
    raw_token: &str,
    now_secs: i64,
) -> Result<Option<AuthenticatedToken>> {
    let hash = hash_api_token(raw_token);
    let row = sqlx::query!(
        r#"SELECT token_id   AS "token_id!: i64",
                  user_id    AS "user_id!: i64",
                  scopes     AS "scopes!: String",
                  expires_at AS "expires_at?: i64",
                  revoked_at AS "revoked_at?: i64"
             FROM tokens
            WHERE token_hash = ?"#,
        hash,
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| Error::Database(format!("tokens lookup: {e}")))?;
    let Some(row) = row else {
        return Ok(None);
    };
    if row.revoked_at.is_some() {
        return Ok(None);
    }
    if row.expires_at.is_some_and(|exp| exp <= now_secs) {
        return Ok(None);
    }

    // Best-effort last_used_at update, fired into the
    // background. Failure logs but doesn't reject the auth.
    let pool_for_update = pool.clone();
    let token_id = row.token_id;
    tokio::spawn(async move {
        let r = sqlx::query!(
            "UPDATE tokens SET last_used_at = ? WHERE token_id = ?",
            now_secs,
            token_id,
        )
        .execute(&pool_for_update)
        .await;
        if let Err(e) = r {
            tracing::warn!(
                token_id,
                error = %e,
                "db.tokens.last_used_update_failed"
            );
        }
    });

    Ok(Some(AuthenticatedToken {
        token_id: row.token_id,
        user_id: row.user_id,
        scopes: Arc::new(serde_json::from_str(&row.scopes).unwrap_or_default()),
    }))
}

/// True iff `tokens` has no rows.
///
/// Used by the api crate's `admin_token` compat fallback to
/// decide whether to engage the bootstrap path (which only
/// fires until the operator has rotated to their first
/// per-user token). The shelf doesn't use this — it requires
/// real per-user tokens. The helper lives here anyway so a
/// future shelf-side admin fallback can drop in without a
/// re-export move.
///
/// # Errors
///
/// Bubbles DB errors. Caller decides whether to fail closed
/// (refuse the fallback) or fail open (engage it); the api
/// crate fails closed.
pub async fn tokens_table_is_empty(pool: &SqlitePool) -> Result<bool> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tokens")
        .fetch_one(pool)
        .await
        .map_err(|e| Error::Database(format!("tokens count: {e}")))?;
    Ok(count == 0)
}

/// Current Unix-epoch seconds, clipped to `i64::MAX` on
/// 2038-style overflow.
#[must_use]
pub fn unix_now_secs() -> i64 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    i64::try_from(secs).unwrap_or(i64::MAX)
}
