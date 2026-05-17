//! Read-only API surface for `operation_journal` (ADR-0039).
//!
//! The journal is the operator's lens into every mutating
//! operation: tag writes, batch edits, audiologo cuts, etc.
//! Each row carries `op_kind` / `target_kind` / `target_id` plus
//! `progress` (`pending` / `done` / `failed` / `reversed`),
//! `pre_state_json` / `post_state_json`, `batch_id` for grouped
//! ops, and `failed_reason` when the stage gave up.
//!
//! This module exposes a paginated read with filters; it does NOT
//! expose any write (mutations route through their owning stages
//! / endpoints and write their own journal rows).
//!
//! Companion surfaces shipped previously:
//!
//! * `StaleOperationJournalTarget` (PR #181) — 90-day prune of
//!   terminal-state rows.
//! * `JournalCheck` doctor (PR #182) — warns when pending rows
//!   are non-zero.

use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::pagination::{clamp_limit, clamp_offset};
use crate::state::ApiState;

/// One row in the journal, as surfaced to API clients.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OperationJournalRow {
    pub op_id: i64,
    pub op_kind: String,
    pub target_kind: String,
    pub target_id: i64,
    pub progress: String,
    pub batch_id: Option<String>,
    pub created_at: i64,
    pub reversible: bool,
    pub failed_reason: Option<String>,
    /// Pre-mutation state as opaque JSON. Clients render as
    /// preformatted text or feed to a diff viewer.
    pub pre_state_json: String,
    /// Post-mutation state, NULL while pending or for dry-run.
    pub post_state_json: Option<String>,
}

/// Optional filters for `GET /operation_journal`.
#[derive(Debug, Deserialize, Default)]
pub struct OperationJournalQuery {
    /// Filter to one op kind (e.g. `tag-write-final`).
    pub op_kind: Option<String>,
    /// Filter to one progress state (`pending` / `done` /
    /// `failed` / `reversed`). Unknown values return empty.
    pub progress: Option<String>,
    /// Filter to one batch id.
    pub batch_id: Option<String>,
    /// 1..=200, defaults to 50; clamped per [`crate::pagination`].
    pub limit: Option<i64>,
    /// 0-based offset, clamped to >= 0.
    pub offset: Option<i64>,
}

/// Response shape for `GET /operation_journal`.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OperationJournalListResponse {
    pub rows: Vec<OperationJournalRow>,
    /// Total matching the filter set, NOT clamped by limit.
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// `GET /api/v1/operation_journal` — paginated read with optional
/// filters. Ordered `created_at DESC, op_id DESC` (newest first).
///
/// # Errors
///
/// Returns [`ApiError::Internal`] for DB failures.
pub async fn operation_journal_list(
    State(state): State<ApiState>,
    Query(query): Query<OperationJournalQuery>,
) -> Result<Json<OperationJournalListResponse>, ApiError> {
    let limit = clamp_limit(query.limit);
    let offset = clamp_offset(query.offset);

    let total: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "count!: i64" FROM operation_journal
            WHERE (?1 IS NULL OR op_kind   = ?1)
              AND (?2 IS NULL OR progress  = ?2)
              AND (?3 IS NULL OR batch_id  = ?3)"#,
        query.op_kind,
        query.progress,
        query.batch_id,
    )
    .fetch_one(state.inner.library.pool())
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "operation_journal count: {e}"
        )))
    })?;

    let raw = sqlx::query!(
        r#"SELECT op_id          AS "op_id!: i64",
                  op_kind         AS "op_kind!: String",
                  target_kind     AS "target_kind!: String",
                  target_id       AS "target_id!: i64",
                  progress        AS "progress!: String",
                  batch_id        AS "batch_id?: String",
                  created_at      AS "created_at!: i64",
                  reversible      AS "reversible!: i64",
                  failed_reason   AS "failed_reason?: String",
                  pre_state_json  AS "pre_state_json!: String",
                  post_state_json AS "post_state_json?: String"
             FROM operation_journal
            WHERE (?1 IS NULL OR op_kind   = ?1)
              AND (?2 IS NULL OR progress  = ?2)
              AND (?3 IS NULL OR batch_id  = ?3)
            ORDER BY created_at DESC, op_id DESC
            LIMIT ?4 OFFSET ?5"#,
        query.op_kind,
        query.progress,
        query.batch_id,
        limit,
        offset,
    )
    .fetch_all(state.inner.library.pool())
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "operation_journal list: {e}"
        )))
    })?;

    let rows = raw
        .into_iter()
        .map(|r| OperationJournalRow {
            op_id: r.op_id,
            op_kind: r.op_kind,
            target_kind: r.target_kind,
            target_id: r.target_id,
            progress: r.progress,
            batch_id: r.batch_id,
            created_at: r.created_at,
            reversible: r.reversible != 0,
            failed_reason: r.failed_reason,
            pre_state_json: r.pre_state_json,
            post_state_json: r.post_state_json,
        })
        .collect();

    Ok(Json(OperationJournalListResponse {
        rows,
        total,
        limit,
        offset,
    }))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn response_serializes_with_pagination_keys() {
        let r = OperationJournalListResponse {
            rows: vec![],
            total: 0,
            limit: 50,
            offset: 0,
        };
        let json = serde_json::to_string(&r).expect("serialize");
        assert!(json.contains("\"rows\""));
        assert!(json.contains("\"total\""));
        assert!(json.contains("\"limit\""));
        assert!(json.contains("\"offset\""));
    }

    #[test]
    fn row_serializes_reversible_as_bool() {
        let r = OperationJournalRow {
            op_id: 1,
            op_kind: "tag-write-final".into(),
            target_kind: "book".into(),
            target_id: 42,
            progress: "done".into(),
            batch_id: Some("01HXYZ".into()),
            created_at: 1_700_000_000,
            reversible: true,
            failed_reason: None,
            pre_state_json: "{}".into(),
            post_state_json: Some("{}".into()),
        };
        let json = serde_json::to_string(&r).expect("serialize");
        assert!(json.contains("\"reversible\":true"));
        assert!(json.contains("\"failed_reason\":null"));
    }
}
