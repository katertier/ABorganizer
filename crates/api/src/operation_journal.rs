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
use axum::extract::{Path, Query, State};
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

/// Response shape for `POST /operation_journal/{op_id}/retry`.
///
/// `outcome` is one of:
/// - `"retried"` — Replayer succeeded; row flipped to `done` with
///   the handler-supplied post-state.
/// - `"skipped"` — Replayer declined (pre-state drifted, target
///   gone, non-idempotent invariant); row flipped to `failed` with
///   `reason` lifted onto `failed_reason`.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OperationJournalRetryResponse {
    pub op_id: i64,
    pub op_kind: String,
    pub outcome: String,
    /// For `outcome = "skipped"`: the handler-supplied reason.
    /// For `outcome = "retried"`: `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Response shape for `GET /operation_journal/replayers`.
///
/// Lists the `op_kind`s for which a concrete [`ab_journal::Replayer`]
/// has been registered at daemon startup (ADR-0039, PR #194). An empty
/// list means `recover_pending` will only mark stragglers as `failed`
/// — no per-`op_kind` retry has been wired yet.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OperationJournalReplayersResponse {
    /// Sorted ascending for stable diffs.
    pub op_kinds: Vec<String>,
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

/// `GET /api/v1/operation_journal/{op_id}` — fetch a single entry by id.
///
/// Same shape as one row of `operation_journal_list`. Useful for an
/// operator-facing detail view that surfaces full `pre_state_json` /
/// `post_state_json` (the list view typically truncates for table
/// rendering).
///
/// # Errors
///
/// - `404` if `op_id` doesn't exist.
/// - `500` for DB failures.
pub async fn operation_journal_get(
    State(state): State<ApiState>,
    Path(op_id): Path<i64>,
) -> Result<Json<OperationJournalRow>, ApiError> {
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
            WHERE op_id = ?"#,
        op_id,
    )
    .fetch_optional(state.inner.library.pool())
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "operation_journal get: {e}"
        )))
    })?;

    let r = raw.ok_or_else(|| ApiError::NotFound(format!("operation_journal op_id={op_id}")))?;

    Ok(Json(OperationJournalRow {
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
    }))
}

/// `POST /api/v1/operation_journal/{op_id}/retry` — re-dispatch a single pending entry.
///
/// Looks up the row, finds the registered [`ab_journal::Replayer`] for its
/// `op_kind`, and calls `try_replay`. Mirrors `recover_pending_with`'s
/// per-row logic but for an operator-triggered retry. Idempotent —
/// once the row leaves `pending` no further retry call succeeds.
///
/// # Errors
///
/// - `404` if `op_id` doesn't exist or no `Replayer` is registered for
///   the row's `op_kind`.
/// - `409` if the row is not in `pending` state (already `done`,
///   `failed`, or `reversed`).
/// - `500` for DB / Replayer errors.
pub async fn operation_journal_retry_post(
    State(state): State<ApiState>,
    Path(op_id): Path<i64>,
) -> Result<Json<OperationJournalRetryResponse>, ApiError> {
    let entry = match ab_journal::get(state.inner.library.pool(), op_id).await {
        Ok(e) => e,
        Err(ab_journal::JournalError::NotFound(_)) => {
            return Err(ApiError::NotFound(format!(
                "operation_journal op_id={op_id}"
            )));
        }
        Err(e) => {
            return Err(ApiError::Internal(ab_core::Error::Database(format!(
                "operation_journal get: {e}"
            ))));
        }
    };

    if entry.progress != ab_journal::Progress::Pending {
        return Err(ApiError::Conflict(format!(
            "op_id={op_id} is in progress={} — only pending rows can be retried",
            entry.progress.as_str(),
        )));
    }

    let Some(handler) = state.inner.replay_registry.get(&entry.op_kind) else {
        return Err(ApiError::NotFound(format!(
            "no Replayer registered for op_kind={kind}",
            kind = entry.op_kind,
        )));
    };

    let decision = handler
        .try_replay(state.inner.library.pool(), &entry)
        .await
        .map_err(|e| {
            ApiError::Internal(ab_core::Error::Database(format!(
                "Replayer({kind})::try_replay: {e}",
                kind = entry.op_kind,
            )))
        })?;

    let response = match decision {
        ab_journal::ReplayDecision::Retried(post_state) => {
            ab_journal::mark_done(state.inner.library.pool(), op_id, &post_state)
                .await
                .map_err(|e| {
                    ApiError::Internal(ab_core::Error::Database(format!(
                        "operation_journal mark_done: {e}"
                    )))
                })?;
            OperationJournalRetryResponse {
                op_id,
                op_kind: entry.op_kind,
                outcome: "retried".to_owned(),
                reason: None,
            }
        }
        ab_journal::ReplayDecision::Skipped(reason) => {
            ab_journal::mark_failed(state.inner.library.pool(), op_id, &reason)
                .await
                .map_err(|e| {
                    ApiError::Internal(ab_core::Error::Database(format!(
                        "operation_journal mark_failed: {e}"
                    )))
                })?;
            OperationJournalRetryResponse {
                op_id,
                op_kind: entry.op_kind,
                outcome: "skipped".to_owned(),
                reason: Some(reason),
            }
        }
    };
    Ok(Json(response))
}

/// `GET /api/v1/operation_journal/replayers` — list of registered `op_kind`s.
///
/// Used by the operator-facing dashboard and `aborg doctor` to confirm
/// which mutating operations `recover_pending` can actually replay after
/// a daemon crash.
///
/// # Errors
///
/// Infallible today; signature returns `Result` to leave room
/// for future enrichments (per-`op_kind` health, last-run stats).
#[allow(clippy::unused_async, reason = "axum handler signature")]
pub async fn operation_journal_replayers_get(
    State(state): State<ApiState>,
) -> Result<Json<OperationJournalReplayersResponse>, ApiError> {
    let mut op_kinds: Vec<String> = state
        .inner
        .replay_registry
        .op_kinds()
        .into_iter()
        .map(str::to_owned)
        .collect();
    op_kinds.sort();
    Ok(Json(OperationJournalReplayersResponse { op_kinds }))
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
    fn replayers_response_serializes_op_kinds_array() {
        let r = OperationJournalReplayersResponse {
            op_kinds: vec!["tag-write-final".into(), "audiologo-cut".into()],
        };
        let json = serde_json::to_string(&r).expect("serialize");
        assert!(json.contains("\"op_kinds\""));
        assert!(json.contains("\"tag-write-final\""));
        assert!(json.contains("\"audiologo-cut\""));
    }

    #[test]
    fn replayers_response_empty_serializes_as_empty_array() {
        let r = OperationJournalReplayersResponse { op_kinds: vec![] };
        let json = serde_json::to_string(&r).expect("serialize");
        assert_eq!(json, r#"{"op_kinds":[]}"#);
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

    mod get_handler {
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
            let ephemeral =
                EphemeralDb::open(&tmp.path().join("ephemeral.db"), &DbTunables::default())
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

        async fn seed_done_row(state: &ApiState, op_kind: &str, target_id: i64) -> i64 {
            sqlx::query_scalar!(
                r#"INSERT INTO operation_journal
                       (op_kind, target_kind, target_id, pre_state_json,
                        post_state_json, progress, reversible)
                   VALUES (?, 'book', ?, '{"a":1}', '{"a":2}', 'done', 1)
                   RETURNING op_id AS "op_id!: i64""#,
                op_kind,
                target_id,
            )
            .fetch_one(state.inner.library.pool())
            .await
            .expect("seed journal row")
        }

        #[tokio::test]
        async fn returns_row_when_present() {
            let (state, _tmp) = fresh_state().await;
            let op_id = seed_done_row(&state, "tag-write-final", 42).await;
            let Json(row) = operation_journal_get(State(state), Path(op_id))
                .await
                .expect("handler ok");
            assert_eq!(row.op_id, op_id);
            assert_eq!(row.op_kind, "tag-write-final");
            assert_eq!(row.target_kind, "book");
            assert_eq!(row.target_id, 42);
            assert_eq!(row.progress, "done");
            assert!(row.reversible);
            assert_eq!(row.pre_state_json, r#"{"a":1}"#);
            assert_eq!(row.post_state_json.as_deref(), Some(r#"{"a":2}"#));
            assert!(row.failed_reason.is_none());
        }

        #[tokio::test]
        async fn returns_404_when_op_id_missing() {
            let (state, _tmp) = fresh_state().await;
            let err = operation_journal_get(State(state), Path(99_999))
                .await
                .expect_err("expected 404");
            assert!(
                matches!(err, ApiError::NotFound(ref msg) if msg.contains("99999")),
                "got: {err:?}",
            );
        }

        #[tokio::test]
        async fn picks_correct_row_among_many() {
            let (state, _tmp) = fresh_state().await;
            let _a = seed_done_row(&state, "kind-a", 1).await;
            let b = seed_done_row(&state, "kind-b", 2).await;
            let _c = seed_done_row(&state, "kind-c", 3).await;
            let Json(row) = operation_journal_get(State(state), Path(b))
                .await
                .expect("handler ok");
            assert_eq!(row.op_id, b);
            assert_eq!(row.op_kind, "kind-b");
            assert_eq!(row.target_id, 2);
        }
    }
}
