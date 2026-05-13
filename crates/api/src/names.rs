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

// ── Pending disambiguation (slice H.3.6, ADR-0026) ────────────────

/// One unresolved disambiguation row + its candidate scores. The
/// GUI / CLI surface uses this to render "Book X — observed alias
/// Y — candidates A (score), B (score)" and let the operator pick.
#[derive(Serialize)]
pub struct PendingRow {
    pub pending_id: i64,
    pub kind: &'static str,
    pub book_id: i64,
    pub observed_alias: String,
    pub created_at: i64,
    pub candidates: Vec<PendingCandidate>,
}

#[derive(Serialize)]
pub struct PendingCandidate {
    pub id: i64,
    /// Display name (prime alias OR canonical) for this candidate.
    /// Operator sees the spellings + `audible_id` to disambiguate.
    pub display: Option<String>,
    pub audible_id: Option<String>,
    pub score: f64,
}

/// `GET /api/v1/names/pending` — list every unresolved
/// disambiguation row across all three identity kinds.
pub async fn names_pending_list(
    State(state): State<ApiState>,
) -> Result<Json<Vec<PendingRow>>, ApiError> {
    let mut out: Vec<PendingRow> = Vec::new();
    for kind in [
        IdentityKind::Author,
        IdentityKind::Narrator,
        IdentityKind::Series,
    ] {
        let rows = list_pending_for_kind(&state, kind).await?;
        out.extend(rows);
    }
    Ok(Json(out))
}

async fn list_pending_for_kind(
    state: &ApiState,
    kind: IdentityKind,
) -> Result<Vec<PendingRow>, ApiError> {
    let parent_table = kind.parent_table();
    let parent_pk = kind.parent_pk();
    let junction = kind.junction_table();
    let junction_fk = kind.fk_column();
    let (pending_table, candidate_table, _resolved_col, candidate_fk) = pending_tables_for(kind);

    let pending_sql = format!(
        "SELECT pending_id, book_id, observed_alias, created_at \
         FROM {pending_table} WHERE resolved_at IS NULL \
         ORDER BY created_at"
    );
    let pending_rows: Vec<(i64, i64, String, i64)> = sqlx::query_as(&pending_sql)
        .fetch_all(state.inner.library.pool())
        .await
        .map_err(|e| ab_core::Error::Database(format!("names pending list: {e}")))?;

    let candidate_sql = format!(
        "SELECT c.{candidate_fk}, c.score, \
                COALESCE( \
                    (SELECT alias FROM {junction} ja \
                       WHERE ja.{junction_fk} = c.{candidate_fk} AND ja.is_prime = 1 LIMIT 1), \
                    (SELECT name FROM {parent_table} p \
                       WHERE p.{parent_pk} = c.{candidate_fk})), \
                (SELECT audible_id FROM {parent_table} p \
                   WHERE p.{parent_pk} = c.{candidate_fk}) \
         FROM {candidate_table} c \
         WHERE c.pending_id = ? \
         ORDER BY c.score DESC, c.{candidate_fk}"
    );

    let mut out: Vec<PendingRow> = Vec::with_capacity(pending_rows.len());
    for (pending_id, book_id, observed_alias, created_at) in pending_rows {
        let cand_rows: Vec<(i64, f64, Option<String>, Option<String>)> =
            sqlx::query_as(&candidate_sql)
                .bind(pending_id)
                .fetch_all(state.inner.library.pool())
                .await
                .map_err(|e| ab_core::Error::Database(format!("names pending candidates: {e}")))?;
        let candidates: Vec<PendingCandidate> = cand_rows
            .into_iter()
            .map(|(id, score, display, audible_id)| PendingCandidate {
                id,
                display,
                audible_id,
                score,
            })
            .collect();
        out.push(PendingRow {
            pending_id,
            kind: parent_table,
            book_id,
            observed_alias,
            created_at,
            candidates,
        });
    }
    Ok(out)
}

/// Helper wrapper around the closed-allowlist dispatch used by the
/// corroboration code in `ab-catalog`. Re-implemented here so the
/// api crate doesn't have to take a dependency on `ab-catalog`'s
/// internals.
const fn pending_tables_for(
    kind: IdentityKind,
) -> (&'static str, &'static str, &'static str, &'static str) {
    match kind {
        IdentityKind::Author => (
            "author_disambiguation_pending",
            "author_disambiguation_candidate",
            "resolved_author_id",
            "author_id",
        ),
        IdentityKind::Narrator => (
            "narrator_disambiguation_pending",
            "narrator_disambiguation_candidate",
            "resolved_narrator_id",
            "narrator_id",
        ),
        IdentityKind::Series => (
            "series_disambiguation_pending",
            "series_disambiguation_candidate",
            "resolved_series_id",
            "series_id",
        ),
    }
}

/// Body of `POST /api/v1/names/pending/{pending_id}/resolve`.
///
/// Operator picks one of the existing candidates by ID, or
/// supplies `create_new` to insert a fresh identity row when none
/// of the candidates is right.
#[derive(Deserialize)]
pub struct PendingResolveRequest {
    /// Which identity kind owns this pending row (`author` /
    /// `narrator` / `series`). Required so the dispatch can write
    /// to the right table without re-querying.
    pub kind: String,
    /// Existing parent ID to attach. Mutually exclusive with
    /// `create_new`.
    #[serde(default)]
    pub pick: Option<i64>,
    /// Escape hatch when none of the corroboration candidates is
    /// the right answer. Inserts a new parent row + canonical
    /// alias, then attaches.
    #[serde(default)]
    pub create_new: Option<CreateNew>,
}

#[derive(Deserialize)]
pub struct CreateNew {
    /// Canonical name for the new identity row.
    pub name: String,
    /// Optional `audible_id` to seed.
    #[serde(default)]
    pub audible_id: Option<String>,
}

#[derive(Serialize)]
pub struct PendingResolveResponse {
    pub pending_id: i64,
    pub kind: &'static str,
    pub book_id: i64,
    pub resolved_id: i64,
}

/// `POST /api/v1/names/pending/{pending_id}/resolve`.
///
/// Resolves the pending row: attaches the chosen identity to the
/// book (via the kind-appropriate FK or junction), registers the
/// observed alias on that identity if not already there, and
/// stamps `resolved_at` so the row drops out of the open list.
pub async fn names_pending_resolve(
    State(state): State<ApiState>,
    Path(pending_id): Path<i64>,
    Json(req): Json<PendingResolveRequest>,
) -> Result<Json<PendingResolveResponse>, ApiError> {
    let kind = parse_kind(&req.kind)?;
    if req.pick.is_some() && req.create_new.is_some() {
        return Err(ApiError::BadRequest(
            "pick and create_new are mutually exclusive".into(),
        ));
    }
    if req.pick.is_none() && req.create_new.is_none() {
        return Err(ApiError::BadRequest(
            "must specify exactly one of pick or create_new".into(),
        ));
    }
    let (pending_table, _candidate_table, resolved_col, _candidate_fk) = pending_tables_for(kind);

    let mut tx = state
        .inner
        .library
        .pool()
        .begin()
        .await
        .map_err(|e| ab_core::Error::Database(format!("resolve tx begin: {e}")))?;

    // Load the pending row to get book_id + observed_alias.
    let load_sql = format!(
        "SELECT book_id, observed_alias FROM {pending_table} \
         WHERE pending_id = ? AND resolved_at IS NULL LIMIT 1"
    );
    let pending: Option<(i64, String)> = sqlx::query_as(&load_sql)
        .bind(pending_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| ab_core::Error::Database(format!("resolve load pending: {e}")))?;
    let Some((book_id, observed_alias)) = pending else {
        return Err(ApiError::NotFound(format!(
            "pending {pending_id} not found or already resolved"
        )));
    };

    let resolved_id = if let Some(pick) = req.pick {
        verify_pick(&mut tx, kind, pending_id, pick).await?;
        pick
    } else if let Some(new) = req.create_new {
        create_new_identity(&mut tx, kind, &new).await?
    } else {
        // Validated above; unreachable.
        return Err(ApiError::BadRequest(
            "must specify exactly one of pick or create_new".into(),
        ));
    };

    // Register the observed alias on the resolved identity (if not
    // already there) so future tag-only matches resolve straight
    // here.
    let junction = kind.junction_table();
    let junction_fk = kind.fk_column();
    let alias_register_sql = format!(
        "INSERT OR IGNORE INTO {junction} \
         ({junction_fk}, alias, source, is_prime) VALUES (?, ?, 'manual', 0)"
    );
    sqlx::query(&alias_register_sql)
        .bind(resolved_id)
        .bind(&observed_alias)
        .execute(&mut *tx)
        .await
        .map_err(|e| ab_core::Error::Database(format!("resolve register alias: {e}")))?;

    // Attach the FK on the book based on kind. Authors map to a
    // direct FK (`books.author_id`); narrators / series go
    // through their junctions.
    attach_resolved_to_book(&mut tx, kind, book_id, resolved_id).await?;

    // Stamp the pending row resolved.
    let stamp_sql = format!(
        "UPDATE {pending_table} SET resolved_at = strftime('%s','now'), \
                {resolved_col} = ? WHERE pending_id = ?"
    );
    sqlx::query(&stamp_sql)
        .bind(resolved_id)
        .bind(pending_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| ab_core::Error::Database(format!("resolve stamp pending: {e}")))?;

    tx.commit()
        .await
        .map_err(|e| ab_core::Error::Database(format!("resolve commit: {e}")))?;
    tracing::info!(
        kind = kind.parent_table(),
        pending_id,
        book_id,
        resolved_id,
        "api.names.resolved"
    );
    Ok(Json(PendingResolveResponse {
        pending_id,
        kind: kind.parent_table(),
        book_id,
        resolved_id,
    }))
}

async fn verify_pick(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    kind: IdentityKind,
    pending_id: i64,
    pick: i64,
) -> Result<(), ApiError> {
    let (_pending, candidate_table, _resolved, candidate_fk) = pending_tables_for(kind);
    let sql = format!(
        "SELECT 1 FROM {candidate_table} \
         WHERE pending_id = ? AND {candidate_fk} = ? LIMIT 1"
    );
    let found: Option<i64> = sqlx::query_scalar(&sql)
        .bind(pending_id)
        .bind(pick)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|e| ab_core::Error::Database(format!("resolve verify pick: {e}")))?;
    if found.is_none() {
        return Err(ApiError::BadRequest(format!(
            "pick {pick} is not a candidate for pending {pending_id}; \
             use create_new for a fresh row"
        )));
    }
    Ok(())
}

async fn create_new_identity(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    kind: IdentityKind,
    new: &CreateNew,
) -> Result<i64, ApiError> {
    let parent_table = kind.parent_table();
    let parent_pk = kind.parent_pk();
    let insert_sql = format!(
        "INSERT INTO {parent_table} (name, audible_id) \
         VALUES (?, ?) RETURNING {parent_pk}"
    );
    let new_id: i64 = sqlx::query_scalar(&insert_sql)
        .bind(&new.name)
        .bind(new.audible_id.as_deref())
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| ab_core::Error::Database(format!("resolve create_new insert: {e}")))?;
    let junction = kind.junction_table();
    let junction_fk = kind.fk_column();
    let alias_sql = format!(
        "INSERT INTO {junction} \
         ({junction_fk}, alias, source, is_prime) VALUES (?, ?, 'manual', 1)"
    );
    sqlx::query(&alias_sql)
        .bind(new_id)
        .bind(&new.name)
        .execute(&mut **tx)
        .await
        .map_err(|e| ab_core::Error::Database(format!("resolve create_new alias: {e}")))?;
    Ok(new_id)
}

async fn attach_resolved_to_book(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    kind: IdentityKind,
    book_id: i64,
    resolved_id: i64,
) -> Result<(), ApiError> {
    match kind {
        IdentityKind::Author => {
            sqlx::query!(
                "UPDATE books SET author_id = ? WHERE book_id = ?",
                resolved_id,
                book_id,
            )
            .execute(&mut **tx)
            .await
            .map_err(|e| ab_core::Error::Database(format!("resolve attach author: {e}")))?;
        }
        IdentityKind::Narrator => {
            sqlx::query!(
                "INSERT OR IGNORE INTO book_narrator (book_id, narrator_id) VALUES (?, ?)",
                book_id,
                resolved_id,
            )
            .execute(&mut **tx)
            .await
            .map_err(|e| ab_core::Error::Database(format!("resolve attach narrator: {e}")))?;
        }
        IdentityKind::Series => {
            sqlx::query!(
                "INSERT OR IGNORE INTO book_series (book_id, series_id, is_primary) \
                 VALUES (?, ?, 1)",
                book_id,
                resolved_id,
            )
            .execute(&mut **tx)
            .await
            .map_err(|e| ab_core::Error::Database(format!("resolve attach series: {e}")))?;
        }
    }
    Ok(())
}
