//! Pairing-code flow (backlog item 4b).
//!
//! Four endpoints — three authenticated, one anonymous:
//!
//! - `POST   /api/v1/pairing/codes`        (authenticated)
//! - `GET    /api/v1/pairing/codes`        (authenticated)
//! - `DELETE /api/v1/pairing/codes/{code_id}` (authenticated)
//! - `POST   /api/v1/pairing/consume`      (anonymous — see below)
//!
//! ## Flow
//!
//! 1. Operator on the host: `POST /pairing/codes { device_label,
//!    scopes, expires_in_minutes? }` → daemon mints a fresh
//!    `XXXX-XXXX` code, **argon2id-hashes** it, stores the hash
//!    in `ephemeral.db::pairing_codes`, returns the raw code
//!    once.
//! 2. Operator types the code into the device's pairing screen.
//! 3. Device: `POST /pairing/consume { code, device_label }` →
//!    daemon iterates active rows, [`ab_core::auth::verify_password`]
//!    against each, on match issues a token (via the same shape
//!    as `POST /tokens`) and marks the code consumed.
//! 4. Device receives the raw bearer token once and stores it.
//!
//! ## Why `/pairing/consume` is anonymous
//!
//! It's how a new device gets its FIRST bearer token. Putting it
//! behind auth would defeat the whole point. Defense-in-depth:
//!
//! - **Rate-limit** ([`crate::rate_limit::RateLimiter`]) — 30
//!   failed attempts per 60-second rolling window before the
//!   handler returns 429 `Too Many Requests` with
//!   `Retry-After` set. Check fires BEFORE argon2id verify so a
//!   flood can't soak daemon CPU.
//! - **argon2id verify** — ~50ms per attempt, so even within
//!   the rate budget, brute-forcing an 8-char alphanumeric code
//!   takes ~15 GPU-years against the 36-bit code space.
//! - **Single-use** — `consumed_token_id` flips on success, so
//!   even if the same code somehow leaked the second consume
//!   call returns 404.
//! - **Time-bound** — `expires_at` filter in the SELECT.
//!
//! ## Code format
//!
//! `XXXX-XXXX` where each `X` is from a 22-letter alphabet
//! (uppercase `A-Z`; exclude visually-confusable `I`, `O`,
//! `S`, `Z`, to keep the human-readable surface clean). That's
//! 22 distinct chars × 8 positions = 22^8 ≈ 5.4 × 10^10 ≈ 36
//! bits of entropy. Combined with argon2id's slow verify, the
//! brute-force budget over a code's 10-min lifetime is one
//! attempt per 50ms ≈ 12000 attempts, leaving ~32 bits to chew
//! through — comfortably infeasible.

use argon2::password_hash::rand_core::RngCore;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

use ab_core::Error;
use ab_core::auth::{
    PasswordError, hash_api_token, hash_password, mint_api_token, verify_password,
};

use crate::error::ApiError;
use crate::state::ApiState;
use crate::tokens::TokenRow;

/// Path under `/api/v1/...` that the auth middleware lets
/// through without a bearer token. The consume endpoint mounts
/// here; everything else under `/pairing/*` is authenticated.
pub const CONSUME_PUBLIC_PATH: &str = "/pairing/consume";

/// 22-letter alphabet — `A`-`Z` minus visually-confusable
/// `I`, `O`, `S`, `Z`. Used for the human-readable pairing
/// code surface.
const CODE_ALPHABET: &[u8; 22] = b"ABCDEFGHJKLMNPQRTUVWXY";

/// Default lifetime when `expires_in_minutes` is unset on the
/// issue request. 10 minutes is a reasonable cap for a human
/// pairing flow.
const DEFAULT_LIFETIME_MINUTES: u32 = 10;

/// Maximum lifetime an operator can ask for at issue time.
/// Defends against a typo / accidental `expires_in_minutes:
/// 100000` never-expires-in-practice config.
const MAX_LIFETIME_MINUTES: u32 = 24 * 60;

/// One pairing-code row, as returned by `GET /pairing/codes`.
/// The raw code + the hash are kept off the wire — the raw is
/// shown exactly once on the issue response, the hash is the
/// server-side verify key.
#[derive(Debug, Clone, Serialize)]
pub struct PairingCodeRow {
    pub code_id: i64,
    pub device_label: String,
    pub scopes: Vec<String>,
    pub issued_at: i64,
    pub expires_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consumed_token_id: Option<i64>,
}

/// Body for `POST /api/v1/pairing/codes`.
#[derive(Debug, Deserialize)]
pub struct CreatePairingCodeRequest {
    /// Operator-friendly label captured at issue time and
    /// reused as the token's `nickname` post-consume.
    pub device_label: String,
    /// Scopes the eventual token will carry. Free-form today.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Lifetime in minutes. None / 0 → default 10 minutes.
    /// Capped at 24 hours.
    #[serde(default)]
    pub expires_in_minutes: Option<u32>,
}

/// Response for `POST /api/v1/pairing/codes`. The `code` field
/// is the raw bearer string the operator types into the device;
/// **shown exactly once**, never persisted in plaintext.
#[derive(Debug, Serialize)]
pub struct CreatePairingCodeResponse {
    pub code: String,
    pub row: PairingCodeRow,
}

/// Body for `POST /api/v1/pairing/consume`.
#[derive(Debug, Deserialize)]
pub struct ConsumePairingCodeRequest {
    /// The `XXXX-XXXX` code the operator read off the daemon's
    /// pairing screen. Case-insensitive — we uppercase before
    /// comparing.
    pub code: String,
    /// Operator-friendly label the consumer wants on its
    /// token row. Falls back to the pairing code's stored
    /// `device_label` when empty.
    #[serde(default)]
    pub device_label: Option<String>,
}

/// Response for `POST /api/v1/pairing/consume`. The `token`
/// field is the raw bearer the device must record now;
/// subsequent `GET /tokens` calls never return it again.
#[derive(Debug, Serialize)]
pub struct ConsumePairingCodeResponse {
    pub token: String,
    pub row: TokenRow,
}

/// `POST /api/v1/pairing/codes` — issue a new pairing code.
///
/// # Errors
///
/// [`ApiError::BadRequest`] when `device_label` is empty / too
/// long, scopes hold an empty string, or `expires_in_minutes` is
/// way over the cap.
pub async fn pairing_codes_create(
    State(state): State<ApiState>,
    Json(req): Json<CreatePairingCodeRequest>,
) -> Result<(StatusCode, Json<CreatePairingCodeResponse>), ApiError> {
    let device_label = req.device_label.trim();
    if device_label.is_empty() {
        return Err(ApiError::BadRequest(
            "device_label must not be empty".to_owned(),
        ));
    }
    if device_label.len() > 128 {
        return Err(ApiError::BadRequest(
            "device_label must be ≤ 128 chars".to_owned(),
        ));
    }
    if req.scopes.iter().any(String::is_empty) {
        return Err(ApiError::BadRequest(
            "scope strings must be non-empty".to_owned(),
        ));
    }
    let lifetime_minutes = req
        .expires_in_minutes
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_LIFETIME_MINUTES)
        .min(MAX_LIFETIME_MINUTES);

    let raw_code = mint_code();
    // Both PasswordError variants (HashFailed, InvalidEncoded)
    // map to the same DB-error surface — InvalidEncoded can't
    // actually fire on the hash path (only verify), but we
    // unify both for safety. `e.to_string()` carries the
    // variant context.
    let code_hash = hash_password(&raw_code)
        .map_err(|e: PasswordError| Error::Database(format!("pairing_codes argon2 hash: {e}")))?;
    let scopes_json = serde_json::to_string(&req.scopes)
        .map_err(|e| Error::Database(format!("pairing_codes scopes encode: {e}")))?;
    let now = unix_now_secs();
    let expires_at = now.saturating_add(i64::from(lifetime_minutes) * 60);
    let device_label_owned = device_label.to_owned();

    let inserted = sqlx::query!(
        r#"INSERT INTO pairing_codes
             (code_hash, device_label, scopes_json, issued_at, expires_at)
             VALUES (?, ?, ?, ?, ?)
             RETURNING code_id AS "code_id!: i64""#,
        code_hash,
        device_label_owned,
        scopes_json,
        now,
        expires_at,
    )
    .fetch_one(state.inner.ephemeral.pool())
    .await
    .map_err(|e| Error::Database(format!("pairing_codes insert: {e}")))?;

    tracing::info!(
        code_id = inserted.code_id,
        device_label = %device_label_owned,
        lifetime_minutes,
        "api.pairing.code_issued"
    );

    Ok((
        StatusCode::CREATED,
        Json(CreatePairingCodeResponse {
            code: raw_code,
            row: PairingCodeRow {
                code_id: inserted.code_id,
                device_label: device_label_owned,
                scopes: req.scopes,
                issued_at: now,
                expires_at,
                consumed_token_id: None,
            },
        }),
    ))
}

/// `GET /api/v1/pairing/codes` — list every pairing code
/// (active + consumed + expired). The hash + raw code stay off
/// the wire; only metadata.
///
/// # Errors
///
/// Bubbles DB failures as 500-class `ApiError`.
pub async fn pairing_codes_list(
    State(state): State<ApiState>,
) -> Result<Json<Vec<PairingCodeRow>>, ApiError> {
    let rows = sqlx::query!(
        r#"SELECT code_id            AS "code_id!: i64",
                  device_label       AS "device_label!: String",
                  scopes_json        AS "scopes_json!: String",
                  issued_at          AS "issued_at!: i64",
                  expires_at         AS "expires_at!: i64",
                  consumed_token_id  AS "consumed_token_id?: i64"
             FROM pairing_codes
            ORDER BY issued_at DESC, code_id DESC"#,
    )
    .fetch_all(state.inner.ephemeral.pool())
    .await
    .map_err(|e| Error::Database(format!("pairing_codes list: {e}")))?;
    let out: Vec<PairingCodeRow> = rows
        .into_iter()
        .map(|r| PairingCodeRow {
            code_id: r.code_id,
            device_label: r.device_label,
            scopes: serde_json::from_str(&r.scopes_json).unwrap_or_default(),
            issued_at: r.issued_at,
            expires_at: r.expires_at,
            consumed_token_id: r.consumed_token_id,
        })
        .collect();
    Ok(Json(out))
}

/// `DELETE /api/v1/pairing/codes/{code_id}` — revoke an active
/// pairing code before it's consumed.
///
/// Hard-delete (not a soft-delete) because pairing codes carry
/// no useful audit value once revoked — the consumed token row
/// (if any) survives in `tokens` as the durable record.
///
/// # Errors
///
/// [`ApiError::NotFound`] when no row matches `code_id`.
pub async fn pairing_codes_revoke(
    State(state): State<ApiState>,
    Path(code_id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let affected = sqlx::query!(
        "DELETE FROM pairing_codes WHERE code_id = ? AND consumed_token_id IS NULL",
        code_id,
    )
    .execute(state.inner.ephemeral.pool())
    .await
    .map_err(|e| Error::Database(format!("pairing_codes delete: {e}")))?
    .rows_affected();
    if affected == 0 {
        let exists: Option<i64> = sqlx::query_scalar!(
            "SELECT code_id FROM pairing_codes WHERE code_id = ?",
            code_id,
        )
        .fetch_optional(state.inner.ephemeral.pool())
        .await
        .map_err(|e| Error::Database(format!("pairing_codes revoke check: {e}")))?;
        if exists.is_none() {
            return Err(ApiError::NotFound(format!("pairing_code {code_id}")));
        }
        // Code exists but is already consumed → 409 Conflict
        // (you can't revoke a consumed pairing code; revoke the
        // token via `DELETE /tokens/{id}` instead).
        return Err(ApiError::Conflict(format!(
            "pairing_code {code_id} is already consumed; revoke the token instead"
        )));
    }
    tracing::info!(code_id, "api.pairing.code_revoked");
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/pairing/consume` — anonymous (in
/// [`crate::auth::PUBLIC_PATHS`]). Verify the presented code
/// against every active pairing row; on match issue a fresh
/// token and mark the row consumed.
///
/// Returns the raw bearer **exactly once**.
///
/// # Rate limiting
///
/// Gated by [`crate::rate_limit::RateLimiter`] (default 30
/// failed attempts per 60s rolling window). When the budget is
/// exhausted the handler returns [`ApiError::RateLimited`] with
/// a `Retry-After` header — argon2id verify is NOT attempted,
/// so the daemon's CPU stays free even under a flood.
///
/// Only FAILED consume attempts count toward the budget;
/// successful pairings don't increment. The check happens
/// before any DB query so a 429 response is cheap.
///
/// # Errors
///
/// - [`ApiError::BadRequest`] — code is empty / malformed.
/// - [`ApiError::RateLimited`] — too many recent failures from
///   anywhere on the network. Same response for all sources
///   (single global counter, see `rate_limit.rs` module doc on
///   "Why global, not per-IP").
/// - [`ApiError::NotFound`] — no active pairing code matched
///   the presented value (wrong code, expired, already
///   consumed, or revoked). Same response shape for all four
///   so an attacker can't distinguish.
pub async fn pairing_consume(
    State(state): State<ApiState>,
    Json(req): Json<ConsumePairingCodeRequest>,
) -> Result<(StatusCode, Json<ConsumePairingCodeResponse>), ApiError> {
    // Rate-limit FIRST so a flood doesn't soak the daemon on
    // argon2id verifies. The check is cheap (mutex + VecDeque
    // pop_front of stale entries); recording happens only on
    // the failure path below.
    if let crate::rate_limit::CheckResult::RateLimited { retry_after_secs } =
        state.inner.pairing_consume_limiter.check()
    {
        tracing::warn!(retry_after_secs, "api.pairing.consume_rate_limited");
        return Err(ApiError::RateLimited { retry_after_secs });
    }

    let presented = req.code.trim().to_ascii_uppercase();
    if presented.is_empty() {
        return Err(ApiError::BadRequest("code must not be empty".to_owned()));
    }
    let now = unix_now_secs();

    // Pull every still-active row. The table is tiny in practice
    // — typical operator has <5 pending codes — so SELECT + N×
    // argon2id verify is fine. Order by `issued_at DESC` so the
    // most recently issued code is verified first; matches the
    // "I just printed it 30 seconds ago" UX.
    let candidates = sqlx::query!(
        r#"SELECT code_id      AS "code_id!: i64",
                  code_hash    AS "code_hash!: String",
                  device_label AS "device_label!: String",
                  scopes_json  AS "scopes_json!: String",
                  expires_at   AS "expires_at!: i64"
             FROM pairing_codes
            WHERE consumed_token_id IS NULL
              AND expires_at > ?
            ORDER BY issued_at DESC, code_id DESC"#,
        now,
    )
    .fetch_all(state.inner.ephemeral.pool())
    .await
    .map_err(|e| Error::Database(format!("pairing_codes consume select: {e}")))?;

    let mut matched: Option<MatchedPairing> = None;
    for row in candidates {
        match verify_password(&presented, &row.code_hash) {
            Ok(true) => {
                matched = Some(MatchedPairing {
                    code_id: row.code_id,
                    device_label: row.device_label,
                    scopes_json: row.scopes_json,
                    expires_at: row.expires_at,
                });
                break;
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(
                    code_id = row.code_id,
                    error = %e,
                    "api.pairing.verify_failed_row"
                );
            }
        }
    }
    let Some(hit) = matched else {
        // Failed attempt — record into the rate-limiter's
        // rolling-window bucket. The check at the top of this
        // handler will see it next time. Only NoMatch counts
        // (successful pairings explicitly don't increment).
        state.inner.pairing_consume_limiter.record_failure();
        // Same shape for every "no good match" sub-case so an
        // attacker can't distinguish "wrong code" from "expired"
        // from "already consumed."
        tracing::info!("api.pairing.consume_no_match");
        return Err(ApiError::NotFound(
            "pairing code not recognised, expired, or already consumed".to_owned(),
        ));
    };

    let token_nickname = pick_token_nickname(req.device_label.as_deref(), &hit.device_label);
    let (issued_token_id, raw_token, scopes_vec) =
        issue_paired_token(&state, &hit, &token_nickname, now).await?;

    // Mark consumed in ephemeral.db (separate DB from tokens).
    sqlx::query!(
        "UPDATE pairing_codes SET consumed_token_id = ? WHERE code_id = ?",
        issued_token_id,
        hit.code_id,
    )
    .execute(state.inner.ephemeral.pool())
    .await
    .map_err(|e| Error::Database(format!("pairing consume mark: {e}")))?;

    tracing::info!(
        code_id = hit.code_id,
        token_id = issued_token_id,
        device_label = %token_nickname,
        "api.pairing.consumed"
    );

    Ok((
        StatusCode::OK,
        Json(ConsumePairingCodeResponse {
            token: raw_token,
            row: TokenRow {
                token_id: issued_token_id,
                user_id: 1,
                nickname: Some(token_nickname),
                scopes: scopes_vec,
                issued_at: now,
                last_used_at: None,
                expires_at: Some(hit.expires_at),
                revoked_at: None,
            },
        }),
    ))
}

/// Pulled out of [`pairing_consume`] for clarity — carries the
/// row state through the after-verify path.
struct MatchedPairing {
    code_id: i64,
    device_label: String,
    scopes_json: String,
    expires_at: i64,
}

/// Resolve the token's `nickname` from the consume request
/// preference, falling back to the pairing row's label when
/// the request didn't supply (or supplied blank).
fn pick_token_nickname(request_label: Option<&str>, pairing_label: &str) -> String {
    request_label
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(|| pairing_label.to_owned(), str::to_owned)
}

/// INSERT the token row (library.db) for a successful consume.
/// Returns `(token_id, raw_token, scopes_vec)` — caller is
/// responsible for then marking the pairing row consumed
/// (ephemeral.db) and building the HTTP response.
///
/// Token inherits the pairing's `expires_at` (so revoking the
/// pairing bounds the token's life); operator can extend later
/// via `POST /tokens`.
async fn issue_paired_token(
    state: &ApiState,
    hit: &MatchedPairing,
    token_nickname: &str,
    now: i64,
) -> Result<(i64, String, Vec<String>), ApiError> {
    let raw_token = mint_api_token();
    let token_hash = hash_api_token(&raw_token);
    let scopes_vec: Vec<String> = serde_json::from_str(&hit.scopes_json).unwrap_or_default();
    let scopes_json = &hit.scopes_json;
    let expires_at = hit.expires_at;
    let issued = sqlx::query!(
        r#"INSERT INTO tokens
             (user_id, token_hash, nickname, scopes, issued_at, expires_at)
             VALUES (1, ?, ?, ?, ?, ?)
             RETURNING token_id AS "token_id!: i64""#,
        token_hash,
        token_nickname,
        scopes_json,
        now,
        expires_at,
    )
    .fetch_one(state.inner.library.pool())
    .await
    .map_err(|e| Error::Database(format!("pairing consume token insert: {e}")))?;
    Ok((issued.token_id, raw_token, scopes_vec))
}

/// Mint an `XXXX-XXXX` human-readable pairing code. 22-letter
/// alphabet × 8 positions = ~36 bits of entropy; argon2id-slow
/// verify on the consume path keeps brute-force out of
/// operational reach.
fn mint_code() -> String {
    let mut bytes = [0_u8; 8];
    argon2::password_hash::rand_core::OsRng.fill_bytes(&mut bytes);
    let mut out = String::with_capacity(9); // 8 chars + 1 dash
    for (i, b) in bytes.iter().enumerate() {
        if i == 4 {
            out.push('-');
        }
        let idx = (*b as usize) % CODE_ALPHABET.len();
        out.push(CODE_ALPHABET[idx] as char);
    }
    out
}

fn unix_now_secs() -> i64 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    i64::try_from(secs).unwrap_or(i64::MAX)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use ab_core::tunables::{DbTunables, SchedulerTunables, SecurityTunables};
    use ab_db::{EphemeralDb, LibraryDb};
    use ab_pipeline::cleanup::CleanupRegistry;
    use ab_pipeline::{Dag, Scheduler, StageContext};
    use std::sync::Arc;
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

    #[test]
    fn mint_code_shape() {
        let c = mint_code();
        assert_eq!(c.len(), 9, "8 chars + dash");
        assert_eq!(c.chars().nth(4), Some('-'));
        // Every non-dash char is in the alphabet.
        for (i, ch) in c.chars().enumerate() {
            if i == 4 {
                continue;
            }
            assert!(
                CODE_ALPHABET.contains(&(ch as u8)),
                "char {ch} at {i} not in alphabet"
            );
        }
    }

    #[test]
    fn mint_code_is_distinct_per_call() {
        let a = mint_code();
        let b = mint_code();
        assert_ne!(a, b, "rng collision smoke");
    }

    #[tokio::test]
    async fn create_returns_raw_code_then_list_omits_it() {
        let (state, _tmp) = fresh_state().await;
        let (status, Json(created)) = pairing_codes_create(
            State(state.clone()),
            Json(CreatePairingCodeRequest {
                device_label: "iPad".to_owned(),
                scopes: vec!["play".to_owned()],
                expires_in_minutes: Some(30),
            }),
        )
        .await
        .expect("create");
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(created.code.len(), 9, "raw code XXXX-XXXX");
        assert!(created.row.expires_at > created.row.issued_at);
        let Json(rows) = pairing_codes_list(State(state)).await.expect("list");
        assert_eq!(rows.len(), 1);
        // No `code` field on the listing row at all — serde
        // can't accidentally leak it.
        assert_eq!(rows[0].code_id, created.row.code_id);
        assert_eq!(rows[0].device_label, "iPad");
    }

    #[tokio::test]
    async fn create_rejects_empty_device_label() {
        let (state, _tmp) = fresh_state().await;
        let r = pairing_codes_create(
            State(state),
            Json(CreatePairingCodeRequest {
                device_label: "   ".to_owned(),
                scopes: vec![],
                expires_in_minutes: None,
            }),
        )
        .await;
        assert!(matches!(r, Err(ApiError::BadRequest(_))));
    }

    #[tokio::test]
    async fn create_defaults_and_caps_lifetime() {
        let (state, _tmp) = fresh_state().await;
        // None → 10 minutes
        let (_, Json(a)) = pairing_codes_create(
            State(state.clone()),
            Json(CreatePairingCodeRequest {
                device_label: "A".to_owned(),
                scopes: vec![],
                expires_in_minutes: None,
            }),
        )
        .await
        .expect("a");
        let life = a.row.expires_at - a.row.issued_at;
        assert!(
            (599..=601).contains(&life),
            "default lifetime ≈ 10min, got {life}s"
        );
        // 99999 → capped to MAX
        let (_, Json(b)) = pairing_codes_create(
            State(state),
            Json(CreatePairingCodeRequest {
                device_label: "B".to_owned(),
                scopes: vec![],
                expires_in_minutes: Some(99_999),
            }),
        )
        .await
        .expect("b");
        let life_b = b.row.expires_at - b.row.issued_at;
        let max_secs = i64::from(MAX_LIFETIME_MINUTES) * 60;
        assert!(
            (max_secs - 1..=max_secs + 1).contains(&life_b),
            "capped to {max_secs}s, got {life_b}s"
        );
    }

    #[tokio::test]
    async fn revoke_unknown_id_is_not_found() {
        let (state, _tmp) = fresh_state().await;
        let r = pairing_codes_revoke(State(state), Path(99_999)).await;
        assert!(matches!(r, Err(ApiError::NotFound(_))));
    }

    #[tokio::test]
    async fn revoke_pending_removes_row() {
        let (state, _tmp) = fresh_state().await;
        let (_, Json(created)) = pairing_codes_create(
            State(state.clone()),
            Json(CreatePairingCodeRequest {
                device_label: "X".to_owned(),
                scopes: vec![],
                expires_in_minutes: None,
            }),
        )
        .await
        .expect("create");
        let _ = pairing_codes_revoke(State(state.clone()), Path(created.row.code_id))
            .await
            .expect("revoke");
        let Json(rows) = pairing_codes_list(State(state)).await.expect("list");
        assert!(rows.is_empty(), "revoked pending row is hard-deleted");
    }

    #[tokio::test]
    async fn consume_round_trip() {
        let (state, _tmp) = fresh_state().await;
        let (_, Json(created)) = pairing_codes_create(
            State(state.clone()),
            Json(CreatePairingCodeRequest {
                device_label: "iPad-A".to_owned(),
                scopes: vec!["play".to_owned(), "read".to_owned()],
                expires_in_minutes: Some(15),
            }),
        )
        .await
        .expect("create");

        let (status, Json(consumed)) = pairing_consume(
            State(state.clone()),
            Json(ConsumePairingCodeRequest {
                code: created.code.clone(),
                device_label: Some("iPad-A-paired".to_owned()),
            }),
        )
        .await
        .expect("consume");
        assert_eq!(status, StatusCode::OK);
        assert_eq!(consumed.token.len(), 64, "raw bearer is 64 hex chars");
        assert_eq!(consumed.row.nickname.as_deref(), Some("iPad-A-paired"));
        assert_eq!(consumed.row.scopes, vec!["play", "read"]);
        assert_eq!(consumed.row.expires_at, Some(created.row.expires_at));

        // Listing now shows the consumed_token_id is set.
        let Json(rows) = pairing_codes_list(State(state.clone()))
            .await
            .expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].consumed_token_id, Some(consumed.row.token_id));

        // Second consume of the same code fails with NotFound
        // (no longer in active set).
        let r2 = pairing_consume(
            State(state),
            Json(ConsumePairingCodeRequest {
                code: created.code,
                device_label: None,
            }),
        )
        .await;
        assert!(matches!(r2, Err(ApiError::NotFound(_))));
    }

    #[tokio::test]
    async fn consume_rejects_wrong_code() {
        let (state, _tmp) = fresh_state().await;
        let (_, Json(_created)) = pairing_codes_create(
            State(state.clone()),
            Json(CreatePairingCodeRequest {
                device_label: "X".to_owned(),
                scopes: vec![],
                expires_in_minutes: None,
            }),
        )
        .await
        .expect("create");
        let r = pairing_consume(
            State(state),
            Json(ConsumePairingCodeRequest {
                code: "WRON-GONE".to_owned(),
                device_label: None,
            }),
        )
        .await;
        assert!(matches!(r, Err(ApiError::NotFound(_))));
    }

    #[tokio::test]
    async fn consume_is_case_insensitive() {
        let (state, _tmp) = fresh_state().await;
        let (_, Json(created)) = pairing_codes_create(
            State(state.clone()),
            Json(CreatePairingCodeRequest {
                device_label: "X".to_owned(),
                scopes: vec![],
                expires_in_minutes: None,
            }),
        )
        .await
        .expect("create");
        let lower = created.code.to_lowercase();
        let r = pairing_consume(
            State(state),
            Json(ConsumePairingCodeRequest {
                code: lower,
                device_label: None,
            }),
        )
        .await;
        assert!(r.is_ok(), "lower-case input should still verify");
    }

    #[tokio::test]
    async fn consume_rejects_expired_code() {
        let (state, _tmp) = fresh_state().await;
        let (_, Json(created)) = pairing_codes_create(
            State(state.clone()),
            Json(CreatePairingCodeRequest {
                device_label: "X".to_owned(),
                scopes: vec![],
                expires_in_minutes: Some(1),
            }),
        )
        .await
        .expect("create");
        // Backdate expires_at to be in the past.
        sqlx::query("UPDATE pairing_codes SET expires_at = 1 WHERE code_id = ?")
            .bind(created.row.code_id)
            .execute(state.inner.ephemeral.pool())
            .await
            .expect("backdate");
        let r = pairing_consume(
            State(state),
            Json(ConsumePairingCodeRequest {
                code: created.code,
                device_label: None,
            }),
        )
        .await;
        assert!(matches!(r, Err(ApiError::NotFound(_))));
    }

    #[tokio::test]
    async fn revoke_consumed_code_returns_conflict() {
        let (state, _tmp) = fresh_state().await;
        let (_, Json(created)) = pairing_codes_create(
            State(state.clone()),
            Json(CreatePairingCodeRequest {
                device_label: "X".to_owned(),
                scopes: vec![],
                expires_in_minutes: None,
            }),
        )
        .await
        .expect("create");
        let _ = pairing_consume(
            State(state.clone()),
            Json(ConsumePairingCodeRequest {
                code: created.code,
                device_label: None,
            }),
        )
        .await
        .expect("consume");
        let r = pairing_codes_revoke(State(state), Path(created.row.code_id)).await;
        assert!(
            matches!(r, Err(ApiError::Conflict(_))),
            "consumed code → 409 conflict; revoke the token instead"
        );
    }

    /// Repeatedly hammering the consume endpoint with wrong
    /// codes should eventually trip the rate-limiter and return
    /// `RateLimited` instead of `NotFound`. The default budget
    /// is 30 failures / 60s — we hammer 32 times and check the
    /// 31st response (1-indexed) flips to `RateLimited`.
    #[tokio::test]
    async fn consume_rate_limit_kicks_in_after_threshold() {
        let (state, _tmp) = fresh_state().await;

        // First 30 attempts: each should fail with NotFound (no
        // pairing codes exist), each records a failure.
        for i in 1..=30 {
            let r = pairing_consume(
                State(state.clone()),
                Json(ConsumePairingCodeRequest {
                    code: "WRON-GONE".to_owned(),
                    device_label: None,
                }),
            )
            .await;
            assert!(
                matches!(r, Err(ApiError::NotFound(_))),
                "attempt {i}: expected NotFound, got {r:?}"
            );
        }

        // 31st attempt: bucket is full → RateLimited.
        let r = pairing_consume(
            State(state.clone()),
            Json(ConsumePairingCodeRequest {
                code: "WRON-GONE".to_owned(),
                device_label: None,
            }),
        )
        .await;
        match r {
            Err(ApiError::RateLimited { retry_after_secs }) => {
                assert!(
                    retry_after_secs >= 1,
                    "Retry-After must be ≥ 1, got {retry_after_secs}"
                );
            }
            other => panic!("31st attempt should return RateLimited, got {other:?}"),
        }
    }

    /// The rate-limiter check fires BEFORE bad-request
    /// validation, so even an empty-code request gets 429 when
    /// the bucket is full. This matches the doc contract:
    /// "argon2id verify is NOT attempted" implies "no
    /// per-request work happens at all once locked out."
    #[tokio::test]
    async fn rate_limit_short_circuits_empty_code() {
        let (state, _tmp) = fresh_state().await;

        // Pre-fill the bucket to the limit via the limiter
        // directly — faster than 30 handler calls.
        for _ in 0..30 {
            state.inner.pairing_consume_limiter.record_failure();
        }

        // Even with an empty code (which would normally return
        // BadRequest), the rate-limiter fires first.
        let r = pairing_consume(
            State(state),
            Json(ConsumePairingCodeRequest {
                code: String::new(),
                device_label: None,
            }),
        )
        .await;
        assert!(
            matches!(r, Err(ApiError::RateLimited { .. })),
            "rate-limit must short-circuit before BadRequest validation, got {r:?}"
        );
    }
}
