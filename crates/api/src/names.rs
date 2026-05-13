//! Manual identity-alias mutation endpoints (slice H.3.4,
//! ADR-0026).
//!
//! Two operations the operator (CLI or future GUI) needs:
//!
//! - **Alias** — add a spelling to the junction so future
//!   tag-only matches resolve to the right parent.
//!   `POST /api/v1/names/{kind}/{id}/alias`.
//! - **Exalt** — flip which alias displays. Demotes the current
//!   prime, promotes the target. Atomic via a transaction so the
//!   partial unique index never sees two primes mid-flight.
//!   `POST /api/v1/names/{kind}/{id}/exalt`.
//!
//! Both go through the typed [`IdentityKind`] enum so the router
//! refuses unknown kinds with a structured 400 rather than tripping
//! a stray `format!` SQL build at runtime.

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::state::ApiState;

/// Identity-kind discriminant from the URL path. Closed
/// vocabulary; unknown values produce a 400 with the valid set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityKind {
    Author,
    Narrator,
    Series,
}

impl IdentityKind {
    /// Parse the lowercase wire form. None on miss; caller surfaces
    /// 400 with the valid set.
    fn parse(s: &str) -> Option<Self> {
        match s {
            "author" => Some(Self::Author),
            "narrator" => Some(Self::Narrator),
            "series" => Some(Self::Series),
            _ => None,
        }
    }

    /// Parent table name in SQL.
    const fn parent_table(self) -> &'static str {
        match self {
            Self::Author => "authors",
            Self::Narrator => "narrators",
            Self::Series => "series",
        }
    }

    /// Junction table name in SQL.
    const fn junction_table(self) -> &'static str {
        match self {
            Self::Author => "author_aliases",
            Self::Narrator => "narrator_aliases",
            Self::Series => "series_aliases",
        }
    }

    /// FK column inside the junction.
    const fn fk_column(self) -> &'static str {
        match self {
            Self::Author => "author_id",
            Self::Narrator => "narrator_id",
            Self::Series => "series_id",
        }
    }

    /// PK column on the parent.
    const fn parent_pk(self) -> &'static str {
        match self {
            Self::Author => "author_id",
            Self::Narrator => "narrator_id",
            Self::Series => "series_id",
        }
    }
}

fn parse_kind(s: &str) -> Result<IdentityKind, ApiError> {
    IdentityKind::parse(s).ok_or_else(|| {
        ApiError::BadRequest(format!(
            "unknown identity kind `{s}` (valid: author, narrator, series)"
        ))
    })
}

/// Body of `POST /names/{kind}/{id}/alias`.
#[derive(Deserialize)]
pub struct AliasAddRequest {
    /// The new spelling to record. Trimmed before insertion; the
    /// junction's `UNIQUE (parent_id, alias)` makes the call
    /// idempotent on repeats.
    pub alias: String,
}

/// Response shape shared by both endpoints.
#[derive(Serialize)]
pub struct NamesActionResponse {
    pub kind: &'static str,
    pub id: i64,
    /// `true` if a new alias row was inserted; `false` if the
    /// `(parent_id, alias)` pair already existed (idempotent
    /// no-op).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inserted: Option<bool>,
    /// `true` if the prime flag was moved; `false` if the
    /// target alias was already prime.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exalted: Option<bool>,
}

/// `POST /api/v1/names/{kind}/{id}/alias` — record a new spelling
/// for an existing identity row.
///
/// Returns 404 when the parent row doesn't exist; otherwise the
/// operation is idempotent (repeat with the same alias is a no-op
/// that returns `inserted: false`).
pub async fn names_alias(
    State(state): State<ApiState>,
    Path((kind_str, id)): Path<(String, i64)>,
    Json(req): Json<AliasAddRequest>,
) -> Result<Json<NamesActionResponse>, ApiError> {
    let kind = parse_kind(&kind_str)?;
    let alias = req.alias.trim();
    if alias.is_empty() {
        return Err(ApiError::BadRequest("alias must be non-empty".into()));
    }
    ensure_parent_exists(&state, kind, id).await?;
    let inserted = insert_alias(&state, kind, id, alias).await?;
    tracing::info!(
        kind = kind.parent_table(),
        id,
        alias,
        inserted,
        "api.names.alias"
    );
    Ok(Json(NamesActionResponse {
        kind: kind.parent_table(),
        id,
        inserted: Some(inserted),
        exalted: None,
    }))
}

/// `POST /api/v1/names/{kind}/{id}/exalt` — move the prime-alias
/// flag to the given spelling.
///
/// 404 when the parent row doesn't exist; 404 when the alias
/// doesn't already exist on that parent (operator should
/// `POST .../alias` first if they meant to add it).
pub async fn names_exalt(
    State(state): State<ApiState>,
    Path((kind_str, id)): Path<(String, i64)>,
    Json(req): Json<AliasAddRequest>,
) -> Result<Json<NamesActionResponse>, ApiError> {
    let kind = parse_kind(&kind_str)?;
    let alias = req.alias.trim();
    if alias.is_empty() {
        return Err(ApiError::BadRequest("alias must be non-empty".into()));
    }
    ensure_parent_exists(&state, kind, id).await?;
    let exalted = move_prime(&state, kind, id, alias).await?;
    tracing::info!(
        kind = kind.parent_table(),
        id,
        alias,
        exalted,
        "api.names.exalt"
    );
    Ok(Json(NamesActionResponse {
        kind: kind.parent_table(),
        id,
        inserted: None,
        exalted: Some(exalted),
    }))
}

async fn ensure_parent_exists(
    state: &ApiState,
    kind: IdentityKind,
    id: i64,
) -> Result<(), ApiError> {
    let table = kind.parent_table();
    let pk = kind.parent_pk();
    let sql = format!("SELECT 1 FROM {table} WHERE {pk} = ? LIMIT 1");
    let found: Option<i64> = sqlx::query_scalar(&sql)
        .bind(id)
        .fetch_optional(state.inner.library.pool())
        .await
        .map_err(|e| ab_core::Error::Database(format!("names parent lookup: {e}")))?;
    if found.is_none() {
        return Err(ApiError::NotFound(format!("{table} row {id} not found")));
    }
    Ok(())
}

async fn insert_alias(
    state: &ApiState,
    kind: IdentityKind,
    id: i64,
    alias: &str,
) -> Result<bool, ApiError> {
    let junction = kind.junction_table();
    let fk = kind.fk_column();
    let sql = format!(
        "INSERT OR IGNORE INTO {junction} \
         ({fk}, alias, source, is_prime) VALUES (?, ?, 'manual', 0)"
    );
    let res = sqlx::query(&sql)
        .bind(id)
        .bind(alias)
        .execute(state.inner.library.pool())
        .await
        .map_err(|e| ab_core::Error::Database(format!("names alias insert: {e}")))?;
    Ok(res.rows_affected() > 0)
}

async fn move_prime(
    state: &ApiState,
    kind: IdentityKind,
    id: i64,
    alias: &str,
) -> Result<bool, ApiError> {
    let junction = kind.junction_table();
    let fk = kind.fk_column();
    let mut tx = state
        .inner
        .library
        .pool()
        .begin()
        .await
        .map_err(|e| ab_core::Error::Database(format!("names exalt tx begin: {e}")))?;

    // Verify the target alias exists on this parent. Reject 404 if
    // not — operator must `POST .../alias` first.
    let target_sql = format!(
        "SELECT is_prime FROM {junction} \
         WHERE {fk} = ? AND alias = ? COLLATE NOCASE \
         LIMIT 1"
    );
    let current: Option<i64> = sqlx::query_scalar(&target_sql)
        .bind(id)
        .bind(alias)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| ab_core::Error::Database(format!("names exalt target lookup: {e}")))?;
    let Some(was_prime) = current else {
        return Err(ApiError::NotFound(format!(
            "alias `{alias}` not found on {} {id} — add it first via POST .../alias",
            kind.parent_table()
        )));
    };
    if was_prime != 0 {
        // Already prime → no-op success.
        tx.rollback()
            .await
            .map_err(|e| ab_core::Error::Database(format!("names exalt rollback: {e}")))?;
        return Ok(false);
    }

    // Demote any existing prime. The partial unique index would
    // reject a second row at `is_prime = 1` so the
    // demote-then-promote order matters.
    let demote_sql = format!("UPDATE {junction} SET is_prime = 0 WHERE {fk} = ? AND is_prime = 1");
    sqlx::query(&demote_sql)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|e| ab_core::Error::Database(format!("names exalt demote: {e}")))?;

    // Promote the target. `COLLATE NOCASE` so "j.c. williams"
    // exalts "J.C. Williams" without an exact-match dance.
    let promote_sql = format!(
        "UPDATE {junction} SET is_prime = 1 \
         WHERE {fk} = ? AND alias = ? COLLATE NOCASE"
    );
    sqlx::query(&promote_sql)
        .bind(id)
        .bind(alias)
        .execute(&mut *tx)
        .await
        .map_err(|e| ab_core::Error::Database(format!("names exalt promote: {e}")))?;

    tx.commit()
        .await
        .map_err(|e| ab_core::Error::Database(format!("names exalt commit: {e}")))?;
    Ok(true)
}
