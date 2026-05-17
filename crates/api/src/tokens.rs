//! Per-user API token CRUD (backlog item 4a).
//!
//! Three endpoints:
//!
//! - `GET    /api/v1/tokens` — list tokens for the authenticated
//!   user (or every user when the caller is `is_admin`).
//! - `POST   /api/v1/tokens` — issue a new token. Returns the
//!   raw token **exactly once**; persisted as a blake3 hash.
//! - `DELETE /api/v1/tokens/{token_id}` — revoke (set
//!   `revoked_at = now()`).
//!
//! # Auth required
//!
//! Every endpoint here goes through `crate::auth::require_token`
//! like the rest of `/api/v1/*`. There's no chicken-and-egg —
//! the daemon's `admin_token` tunable still works as a one-cycle
//! compat fallback when the `tokens` table is empty, so the
//! operator can use it to bootstrap the first per-user token.
//!
//! # What's persisted
//!
//! - `tokens.token_hash` — lower-case hex of the blake3 digest of
//!   the raw token. We never persist the raw bytes.
//! - `tokens.nickname` — operator-supplied label ("iPad",
//!   "Plappa-iPhone"). Optional but useful in the revocation UI.
//! - `tokens.scopes` — JSON array of scope strings (free-form
//!   today; scope-checked middleware lands with the player surface).
//! - `tokens.expires_at` — derived from `expires_in_days` in the
//!   request (or NULL = no expiry).
//! - `tokens.revoked_at` — NULL until `DELETE` flips it. Migration
//!   022 adds the column.
//!
//! # Why no pairing flow yet
//!
//! Backlog item 4 was originally "per-user tokens + pairing
//! flow." We split the pairing-code flow into its own slice (4b)
//! to keep PRs small. The pairing-code path is the bigger half
//! — it touches anonymous endpoints, code-display UX, argon2id
//! for the low-entropy codes, plus a CLI surface. Per-user
//! tokens land first; pairing layers on top.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use serde_json::json;

use ab_core::Error;
use ab_core::auth::{hash_api_token, mint_api_token};

use crate::error::ApiError;
use crate::state::ApiState;

/// `op_kind` recorded in `operation_journal` for
/// `DELETE /api/v1/tokens/{id}` revocation.
///
/// `reversible = false` — a revoked token cannot be un-revoked
/// because the server only persists the blake3 hash, not the raw
/// bytes. The journal row exists for audit ("who revoked what,
/// when") not for replay. `pre_state` carries the row's
/// non-secret metadata (NO `token_hash`).
pub const OP_KIND_TOKEN_REVOKE: &str = "token-revoke";

/// One token row, as returned by `GET /api/v1/tokens`.
///
/// The raw token is **never** returned here — only on the
/// initial POST response (see [`CreateTokenResponse`]). The
/// hash itself is also kept off the wire because it's the
/// lookup key on the server side; leaking it would defeat the
/// "DB-leak doesn't directly leak the token" defense.
#[derive(Debug, Clone, Serialize)]
pub struct TokenRow {
    pub token_id: i64,
    pub user_id: i64,
    pub nickname: Option<String>,
    pub scopes: Vec<String>,
    pub issued_at: i64,
    pub last_used_at: Option<i64>,
    pub expires_at: Option<i64>,
    pub revoked_at: Option<i64>,
}

/// Body for `POST /api/v1/tokens`.
#[derive(Debug, Deserialize)]
pub struct CreateTokenRequest {
    /// Operator-friendly label. Stored verbatim. Trimmed; empty
    /// → stored as NULL.
    #[serde(default)]
    pub nickname: Option<String>,
    /// Scopes granted to the token. Free-form for now (see
    /// module docs). Empty Vec = no scopes (admin-only).
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Lifetime in days. `None` (or 0) = no expiry. Capped at
    /// 3650 (10 years) to avoid pathological values.
    #[serde(default)]
    pub expires_in_days: Option<u32>,
}

/// Response for the **issue** endpoint.
///
/// The `token` field is the raw bearer string the operator
/// must record now; subsequent `GET /tokens` calls never
/// return it again. Treat this struct like a one-time secret
/// display surface.
#[derive(Debug, Serialize)]
pub struct CreateTokenResponse {
    /// The raw bearer token — show once, store nowhere on the
    /// server side beyond `token_hash`.
    pub token: String,
    /// Metadata for the newly-created row (no hash field).
    pub row: TokenRow,
}

/// `GET /api/v1/tokens` — list every token in the table.
///
/// For now this returns every token regardless of caller —
/// scope-checked filtering (`is_admin` vs `user_id == self`)
/// lands when the player surface introduces per-user
/// segmentation. The middleware already gates access on a valid
/// bearer, so non-admins reaching this endpoint at all already
/// requires the operator to have issued them a token.
///
/// # Errors
///
/// Bubbles DB failures as 500-class `ApiError`.
pub async fn tokens_list(State(state): State<ApiState>) -> Result<Json<Vec<TokenRow>>, ApiError> {
    let rows = sqlx::query!(
        r#"SELECT token_id      AS "token_id!: i64",
                  user_id       AS "user_id!: i64",
                  nickname      AS "nickname?: String",
                  scopes        AS "scopes!: String",
                  issued_at     AS "issued_at!: i64",
                  last_used_at  AS "last_used_at?: i64",
                  expires_at    AS "expires_at?: i64",
                  revoked_at    AS "revoked_at?: i64"
             FROM tokens
            ORDER BY issued_at DESC, token_id DESC"#,
    )
    .fetch_all(state.inner.library.pool())
    .await
    .map_err(|e| Error::Database(format!("tokens list: {e}")))?;
    let out: Vec<TokenRow> = rows
        .into_iter()
        .map(|r| TokenRow {
            token_id: r.token_id,
            user_id: r.user_id,
            nickname: r.nickname,
            scopes: serde_json::from_str(&r.scopes).unwrap_or_default(),
            issued_at: r.issued_at,
            last_used_at: r.last_used_at,
            expires_at: r.expires_at,
            revoked_at: r.revoked_at,
        })
        .collect();
    Ok(Json(out))
}

/// `POST /api/v1/tokens` — issue a new token under `user_id=1`
/// (the default user). The body's nickname / scopes /
/// `expires_in_days` are stored verbatim.
///
/// **The raw token is returned exactly once** in the response.
/// The server stores only the blake3 hash; we can never reissue
/// the raw bearer afterward.
///
/// Multi-user support (binding tokens to specific user rows) is
/// deferred to the slice that introduces user CRUD.
///
/// # Errors
///
/// - [`ApiError::BadRequest`] — nickname too long, scopes
///   non-string, `expires_in_days` out of bounds.
pub async fn tokens_create(
    State(state): State<ApiState>,
    Json(req): Json<CreateTokenRequest>,
) -> Result<(StatusCode, Json<CreateTokenResponse>), ApiError> {
    let nickname = req
        .nickname
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    if nickname.as_deref().is_some_and(|s| s.len() > 128) {
        return Err(ApiError::BadRequest(
            "nickname must be ≤ 128 chars".to_owned(),
        ));
    }
    if req.scopes.iter().any(String::is_empty) {
        return Err(ApiError::BadRequest(
            "scope strings must be non-empty".to_owned(),
        ));
    }
    let expires_in_days = req.expires_in_days.unwrap_or(0).min(3650);

    let raw_token = mint_api_token();
    let hash = hash_api_token(&raw_token);
    let scopes_json = serde_json::to_string(&req.scopes)
        .map_err(|e| Error::Database(format!("scopes JSON encode: {e}")))?;
    let now = ab_db::unix_now_secs();
    let expires_at: Option<i64> = if expires_in_days == 0 {
        None
    } else {
        Some(now.saturating_add(i64::from(expires_in_days) * 86_400))
    };

    // Hard-code user_id = 1 (the default user seeded by migration
    // 001). Multi-user binding lands with user CRUD.
    let user_id: i64 = 1;

    let inserted = sqlx::query!(
        r#"INSERT INTO tokens
             (user_id, token_hash, nickname, scopes, issued_at, expires_at)
             VALUES (?, ?, ?, ?, ?, ?)
             RETURNING token_id AS "token_id!: i64""#,
        user_id,
        hash,
        nickname,
        scopes_json,
        now,
        expires_at,
    )
    .fetch_one(state.inner.library.pool())
    .await
    .map_err(|e| Error::Database(format!("tokens insert: {e}")))?;

    tracing::info!(
        token_id = inserted.token_id,
        user_id,
        nickname = ?nickname,
        scope_count = req.scopes.len(),
        "api.tokens.issued"
    );

    Ok((
        StatusCode::CREATED,
        Json(CreateTokenResponse {
            token: raw_token,
            row: TokenRow {
                token_id: inserted.token_id,
                user_id,
                nickname,
                scopes: req.scopes,
                issued_at: now,
                last_used_at: None,
                expires_at,
                revoked_at: None,
            },
        }),
    ))
}

/// `DELETE /api/v1/tokens/{token_id}` — revoke a token (set
/// `revoked_at = now()`).
///
/// Idempotent: a second DELETE on an already-revoked row is a
/// no-op `204` and records NO new journal row (the revocation
/// already happened — the journal carries the row from the
/// first call).
///
/// Records an `operation_journal` row with
/// `op_kind = "token-revoke"` and `reversible = false`. The
/// row's `pre_state` carries non-secret metadata
/// (`token_id`, `nickname`, `scopes`, `issued_at`,
/// `last_used_at`, `expires_at`) so the audit surface can show
/// "operator revoked X at time Y"; `token_hash` is NEVER copied
/// into the journal.
///
/// # Errors
///
/// [`ApiError::NotFound`] when no row matches `token_id`.
pub async fn tokens_revoke(
    State(state): State<ApiState>,
    Path(token_id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let pool = state.inner.library.pool();

    // Snapshot the row's non-secret metadata for pre_state +
    // detect "already revoked" / "doesn't exist" before doing
    // any mutation. Single SELECT covers both.
    let row = sqlx::query!(
        r#"SELECT token_id    AS "token_id!: i64",
                  user_id     AS "user_id!: i64",
                  nickname    AS "nickname?: String",
                  scopes      AS "scopes!: String",
                  issued_at   AS "issued_at!: i64",
                  last_used_at AS "last_used_at?: i64",
                  expires_at  AS "expires_at?: i64",
                  revoked_at  AS "revoked_at?: i64"
             FROM tokens WHERE token_id = ?"#,
        token_id,
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| Error::Database(format!("tokens revoke pre-read: {e}")))?;
    let Some(row) = row else {
        return Err(ApiError::NotFound(format!("token {token_id}")));
    };

    // Idempotent: already-revoked row → 204 with no journal write.
    if row.revoked_at.is_some() {
        tracing::info!(token_id, "api.tokens.revoke.idempotent_no_op");
        return Ok(StatusCode::NO_CONTENT);
    }

    let pre_state = json!({
        "token_id": row.token_id,
        "user_id": row.user_id,
        "nickname": row.nickname,
        "scopes": row.scopes,
        "issued_at": row.issued_at,
        "last_used_at": row.last_used_at,
        "expires_at": row.expires_at,
    });
    let entry = ab_journal::NewEntry {
        op_kind: OP_KIND_TOKEN_REVOKE,
        target: ab_journal::Target {
            kind: "token".to_owned(),
            id: token_id,
        },
        pre_state,
        reversible: false,
        batch_id: None,
    };
    let op_id = crate::journal_capture::record_pending(pool, &entry).await?;

    let now = ab_db::unix_now_secs();
    let result = sqlx::query!(
        "UPDATE tokens
            SET revoked_at = ?
          WHERE token_id = ? AND revoked_at IS NULL",
        now,
        token_id,
    )
    .execute(pool)
    .await;

    match result {
        Ok(out) => {
            crate::journal_capture::mark_done_or_log(
                pool,
                op_id,
                &json!({ "revoked_at": now }),
                "api.tokens_revoke",
            )
            .await;
            tracing::info!(
                token_id,
                rows_affected = out.rows_affected(),
                "api.tokens.revoked"
            );
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => {
            let reason = format!("tokens revoke: {e}");
            crate::journal_capture::mark_failed_or_log(pool, op_id, &reason, "api.tokens_revoke")
                .await;
            Err(ApiError::Internal(Error::Database(reason)))
        }
    }
}

/// Look up a token by its raw bearer string.
///
/// Thin wrapper around [`ab_db::lookup_by_raw_token`] — the
/// underlying helper was hoisted into `ab-db` in slice C1b so
/// the shelf auth middleware can call it without dragging the
/// whole `ab-api` compile graph in. This wrapper preserves
/// the `(state, raw)` shape every api-side caller already
/// uses.
///
/// # Errors
///
/// Bubbles DB errors. Caller treats `Err` as a 5xx, not a
/// rejected token.
pub async fn lookup_by_raw_token(
    state: &ApiState,
    raw_token: &str,
) -> Result<Option<AuthenticatedToken>, Error> {
    ab_db::lookup_by_raw_token(
        state.inner.library.pool(),
        raw_token,
        ab_db::unix_now_secs(),
    )
    .await
}

/// Re-export from `ab-db` — same struct that's stashed in
/// request extensions on a successful lookup. The previous
/// duplicate definition (this crate's own `AuthenticatedToken`)
/// was removed in C1b once the helper moved down.
pub use ab_db::AuthenticatedToken;

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use ab_core::tunables::{DbTunables, SchedulerTunables, SecurityTunables};
    use ab_db::{EphemeralDb, LibraryDb};
    use ab_pipeline::cleanup::CleanupRegistry;
    use ab_pipeline::{Dag, Scheduler, StageContext};
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    async fn fresh_state() -> (ApiState, TempDir) {
        let tmp = TempDir::new().expect("tmpdir");
        let library = LibraryDb::open(&tmp.path().join("library.db"), &DbTunables::default())
            .await
            .expect("open library");
        let ephemeral = EphemeralDb::open(&tmp.path().join("ephemeral.db"), &DbTunables::default())
            .await
            .expect("open ephemeral");
        let cancel = CancellationToken::new();
        let dag = Arc::new(Dag::build(Vec::new()).expect("empty dag"));
        let ctx = StageContext {
            library: library.clone(),
            ephemeral: ephemeral.clone(),
            cancel: cancel.clone(),
            stage_name: "test",
        };
        let scheduler = Arc::new(Scheduler::spawn(
            Arc::clone(&dag),
            ctx,
            &SchedulerTunables::default(),
        ));
        let state = ApiState::new(
            library,
            ephemeral,
            scheduler,
            dag,
            CleanupRegistry::new(Vec::new()),
            cancel,
            SecurityTunables::default(),
            globset::GlobSet::empty(),
            ab_background::BackgroundRegistry::new(vec![]),
            crate::doctor::DoctorRegistry::new(vec![]),
        );
        (state, tmp)
    }

    #[tokio::test]
    async fn list_empty_on_fresh_db() {
        let (state, _tmp) = fresh_state().await;
        let Json(rows) = tokens_list(State(state)).await.expect("list");
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn create_returns_raw_token_then_list_omits_it() {
        let (state, _tmp) = fresh_state().await;
        let (status, Json(created)) = tokens_create(
            State(state.clone()),
            Json(CreateTokenRequest {
                nickname: Some("iPad".to_owned()),
                scopes: vec!["read".to_owned(), "play".to_owned()],
                expires_in_days: Some(30),
            }),
        )
        .await
        .expect("create");
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(created.token.len(), 64, "raw token is 64-char hex");
        assert!(created.row.expires_at.is_some());
        assert_eq!(created.row.nickname.as_deref(), Some("iPad"));
        assert_eq!(created.row.scopes, vec!["read", "play"]);

        // Subsequent list returns the row but NOT the raw token —
        // the response shape doesn't even have that field.
        let Json(rows) = tokens_list(State(state)).await.expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].token_id, created.row.token_id);
        assert_eq!(rows[0].scopes, created.row.scopes);
    }

    #[tokio::test]
    async fn create_with_no_expiry_stores_null() {
        let (state, _tmp) = fresh_state().await;
        let (_, Json(created)) = tokens_create(
            State(state),
            Json(CreateTokenRequest {
                nickname: None,
                scopes: vec![],
                expires_in_days: None,
            }),
        )
        .await
        .expect("create");
        assert_eq!(created.row.expires_at, None);
        assert!(
            created.row.nickname.is_none(),
            "empty/none nickname stays None"
        );
    }

    #[tokio::test]
    async fn create_rejects_long_nickname() {
        let (state, _tmp) = fresh_state().await;
        let r = tokens_create(
            State(state),
            Json(CreateTokenRequest {
                nickname: Some("x".repeat(129)),
                scopes: vec![],
                expires_in_days: None,
            }),
        )
        .await;
        assert!(matches!(r, Err(ApiError::BadRequest(_))));
    }

    #[tokio::test]
    async fn create_rejects_empty_scope_string() {
        let (state, _tmp) = fresh_state().await;
        let r = tokens_create(
            State(state),
            Json(CreateTokenRequest {
                nickname: None,
                scopes: vec!["read".to_owned(), String::new()],
                expires_in_days: None,
            }),
        )
        .await;
        assert!(matches!(r, Err(ApiError::BadRequest(_))));
    }

    #[tokio::test]
    async fn revoke_then_lookup_returns_none() {
        let (state, _tmp) = fresh_state().await;
        let (_, Json(created)) = tokens_create(
            State(state.clone()),
            Json(CreateTokenRequest {
                nickname: None,
                scopes: vec![],
                expires_in_days: None,
            }),
        )
        .await
        .expect("create");

        // Lookup works before revocation.
        let pre = lookup_by_raw_token(&state, &created.token)
            .await
            .expect("pre");
        assert!(pre.is_some());

        let _ = tokens_revoke(State(state.clone()), Path(created.row.token_id))
            .await
            .expect("revoke");

        // Now the token doesn't authenticate.
        let post = lookup_by_raw_token(&state, &created.token)
            .await
            .expect("post");
        assert!(post.is_none(), "revoked token must not authenticate");
    }

    #[tokio::test]
    async fn revoke_unknown_id_is_not_found() {
        let (state, _tmp) = fresh_state().await;
        let r = tokens_revoke(State(state), Path(99_999)).await;
        assert!(matches!(r, Err(ApiError::NotFound(_))));
    }

    #[tokio::test]
    async fn revoke_already_revoked_is_idempotent_204() {
        let (state, _tmp) = fresh_state().await;
        let (_, Json(created)) = tokens_create(
            State(state.clone()),
            Json(CreateTokenRequest {
                nickname: None,
                scopes: vec![],
                expires_in_days: None,
            }),
        )
        .await
        .expect("create");
        let _ = tokens_revoke(State(state.clone()), Path(created.row.token_id))
            .await
            .expect("first revoke");
        let r = tokens_revoke(State(state), Path(created.row.token_id)).await;
        assert!(r.is_ok(), "second revoke must be a no-op success");
    }

    #[tokio::test]
    async fn lookup_returns_none_for_unknown_token() {
        let (state, _tmp) = fresh_state().await;
        let r = lookup_by_raw_token(&state, "not-a-real-token-just-some-string")
            .await
            .expect("lookup");
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn lookup_returns_none_for_expired_token() {
        let (state, _tmp) = fresh_state().await;
        // Issue with expires_in_days=0 first so we have a row,
        // then manually backdate its expires_at to be in the past.
        let (_, Json(created)) = tokens_create(
            State(state.clone()),
            Json(CreateTokenRequest {
                nickname: None,
                scopes: vec![],
                expires_in_days: Some(1),
            }),
        )
        .await
        .expect("create");
        let past = 1_i64;
        sqlx::query("UPDATE tokens SET expires_at = ? WHERE token_id = ?")
            .bind(past)
            .bind(created.row.token_id)
            .execute(state.inner.library.pool())
            .await
            .expect("backdate");
        let r = lookup_by_raw_token(&state, &created.token)
            .await
            .expect("lookup");
        assert!(r.is_none(), "expired token must not authenticate");
    }

    #[tokio::test]
    async fn lookup_returns_scopes_on_hit() {
        let (state, _tmp) = fresh_state().await;
        let (_, Json(created)) = tokens_create(
            State(state.clone()),
            Json(CreateTokenRequest {
                nickname: Some("test".to_owned()),
                scopes: vec!["scope.a".to_owned(), "scope.b".to_owned()],
                expires_in_days: None,
            }),
        )
        .await
        .expect("create");
        let auth = lookup_by_raw_token(&state, &created.token)
            .await
            .expect("lookup")
            .expect("hit");
        assert_eq!(auth.token_id, created.row.token_id);
        assert_eq!(auth.user_id, 1);
        assert_eq!(&*auth.scopes, &["scope.a", "scope.b"]);
    }
}
