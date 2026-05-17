//! Operation journal — undo + crash recovery + dry-run diff (ADR-0039).
//!
//! Owns the `operation_journal` table, the write/done/fail
//! lifecycle helpers, the diff renderer, and the (placeholder)
//! crash-recovery entry point. Per-operation wiring (tag-write-
//! final, batch-edit, audiologo-apply, etc.) lands in the slice
//! that owns each mutating surface; this crate stays the
//! foundation everyone hooks into.

#![forbid(unsafe_code)]
#![allow(missing_docs)] // scaffold; tightened in follow-up slices

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::SqlitePool;

/// Progress states the schema's CHECK constraint enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Progress {
    Pending,
    Done,
    Failed,
    Reversed,
}

impl Progress {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Reversed => "reversed",
        }
    }
}

/// What kind of object the journal entry mutates. Free-form
/// string — adding a new target (e.g. `'cover'`) requires no
/// schema change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub kind: String,
    pub id: i64,
}

/// New-entry payload. The caller has already gathered the
/// pre-state and chosen an op kind; we accept JSON values so the
/// crate stays generic over what gets journaled.
#[derive(Debug, Clone)]
pub struct NewEntry<'a> {
    pub op_kind: &'a str,
    pub target: Target,
    pub pre_state: Value,
    pub reversible: bool,
    pub batch_id: Option<String>,
}

/// One row of the `operation_journal` table.
#[derive(Debug, Clone, Serialize)]
pub struct JournalEntry {
    pub op_id: i64,
    pub op_kind: String,
    pub target: Target,
    pub pre_state: Value,
    pub post_state: Option<Value>,
    pub created_at: i64,
    pub reversible: bool,
    pub batch_id: Option<String>,
    pub progress: Progress,
    pub failed_reason: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("journal row {0} not found")]
    NotFound(i64),
    #[error("invalid progress: {0}")]
    InvalidProgress(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
}

fn progress_from(s: &str) -> Result<Progress, JournalError> {
    Ok(match s {
        "pending" => Progress::Pending,
        "done" => Progress::Done,
        "failed" => Progress::Failed,
        "reversed" => Progress::Reversed,
        other => return Err(JournalError::InvalidProgress(other.to_owned())),
    })
}

/// Record a new pending journal entry. Returns the inserted `op_id`.
///
/// Call this BEFORE the mutation; if the mutation succeeds the
/// caller should call [`mark_done`], otherwise [`mark_failed`].
pub async fn record(pool: &SqlitePool, entry: &NewEntry<'_>) -> Result<i64, JournalError> {
    let pre_json = serde_json::to_string(&entry.pre_state)?;
    let reversible_i64 = i64::from(entry.reversible);
    let id = sqlx::query!(
        "INSERT INTO operation_journal
            (op_kind, target_kind, target_id, pre_state_json,
             reversible, batch_id, progress)
         VALUES (?, ?, ?, ?, ?, ?, 'pending')",
        entry.op_kind,
        entry.target.kind,
        entry.target.id,
        pre_json,
        reversible_i64,
        entry.batch_id,
    )
    .execute(pool)
    .await?
    .last_insert_rowid();
    Ok(id)
}

/// Mark a pending entry done. Writes `post_state_json` for the
/// diff renderer + undo path.
pub async fn mark_done(
    pool: &SqlitePool,
    op_id: i64,
    post_state: &Value,
) -> Result<(), JournalError> {
    let post_json = serde_json::to_string(post_state)?;
    let affected = sqlx::query!(
        "UPDATE operation_journal
            SET progress = 'done',
                post_state_json = ?
            WHERE op_id = ? AND progress = 'pending'",
        post_json,
        op_id,
    )
    .execute(pool)
    .await?
    .rows_affected();
    if affected == 0 {
        return Err(JournalError::NotFound(op_id));
    }
    Ok(())
}

/// Mark a pending entry failed. `reason` lands on `failed_reason`
/// so the recovery + audit surfaces can show it.
pub async fn mark_failed(pool: &SqlitePool, op_id: i64, reason: &str) -> Result<(), JournalError> {
    let affected = sqlx::query!(
        "UPDATE operation_journal
            SET progress = 'failed',
                failed_reason = ?
            WHERE op_id = ? AND progress = 'pending'",
        reason,
        op_id,
    )
    .execute(pool)
    .await?
    .rows_affected();
    if affected == 0 {
        return Err(JournalError::NotFound(op_id));
    }
    Ok(())
}

/// Mark an entry reversed. Called by `aborg undo --commit` after
/// the inverse mutation lands.
pub async fn mark_reversed(pool: &SqlitePool, op_id: i64) -> Result<(), JournalError> {
    let affected = sqlx::query!(
        "UPDATE operation_journal
            SET progress = 'reversed'
            WHERE op_id = ? AND progress = 'done'",
        op_id,
    )
    .execute(pool)
    .await?
    .rows_affected();
    if affected == 0 {
        return Err(JournalError::NotFound(op_id));
    }
    Ok(())
}

/// Read one entry by id.
pub async fn get(pool: &SqlitePool, op_id: i64) -> Result<JournalEntry, JournalError> {
    let row = sqlx::query!(
        r#"SELECT
            op_kind         AS "op_kind!: String",
            target_kind     AS "target_kind!: String",
            target_id       AS "target_id!: i64",
            pre_state_json  AS "pre_state_json!: String",
            post_state_json AS "post_state_json: String",
            created_at      AS "created_at!: i64",
            reversible      AS "reversible!: i64",
            batch_id        AS "batch_id: String",
            progress        AS "progress!: String",
            failed_reason   AS "failed_reason: String"
         FROM operation_journal WHERE op_id = ?"#,
        op_id,
    )
    .fetch_optional(pool)
    .await?
    .ok_or(JournalError::NotFound(op_id))?;

    let pre_state: Value = serde_json::from_str(&row.pre_state_json)?;
    let post_state = row
        .post_state_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?;

    Ok(JournalEntry {
        op_id,
        op_kind: row.op_kind,
        target: Target {
            kind: row.target_kind,
            id: row.target_id,
        },
        pre_state,
        post_state,
        created_at: row.created_at,
        reversible: row.reversible != 0,
        batch_id: row.batch_id,
        progress: progress_from(&row.progress)?,
        failed_reason: row.failed_reason,
    })
}

/// List pending op-ids per `batch_id`. Used by the recovery pass
/// at daemon startup.
pub async fn pending_batches(
    pool: &SqlitePool,
) -> Result<Vec<(Option<String>, Vec<i64>)>, JournalError> {
    let rows = sqlx::query!(
        r#"SELECT
            op_id    AS "op_id!: i64",
            batch_id AS "batch_id: String"
         FROM operation_journal
         WHERE progress = 'pending'
         ORDER BY batch_id, op_id"#,
    )
    .fetch_all(pool)
    .await?;
    let mut grouped: Vec<(Option<String>, Vec<i64>)> = Vec::new();
    for r in rows {
        if let Some((last_batch, ids)) = grouped.last_mut() {
            if last_batch == &r.batch_id {
                ids.push(r.op_id);
                continue;
            }
        }
        grouped.push((r.batch_id, vec![r.op_id]));
    }
    Ok(grouped)
}

/// Mint a fresh batch id. ULID-shaped uuid (v7 includes a
/// timestamp prefix; both sort lexicographically by creation
/// time which is what the recovery pass wants).
#[must_use]
pub fn new_batch_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// Report from a single [`recover_pending`] (or
/// [`recover_pending_with`]) pass.
///
/// `failed_count` is the number of `pending` rows the recovery pass
/// flipped to `failed`; `retried_count` is the number flipped to
/// `done` by a [`Replayer`] in the registry. Without a
/// registry every row falls into `failed_count`. `batches` lists
/// per-batch detail so the daemon's startup log can show "found N
/// pending operations across M batches; R retried, F failed."
#[derive(Debug, Clone, Serialize)]
pub struct RecoveryReport {
    pub failed_count: usize,
    /// New in PR #194 — defaults to 0 when no replay handlers
    /// are wired up.
    #[serde(default)]
    pub retried_count: usize,
    pub batches: Vec<RecoveredBatch>,
    /// New in PR #195 — per-`op_kind` retried/failed split. Empty
    /// when the journal had no pending rows. `BTreeMap` for stable
    /// iteration order in startup logs + diff-friendly JSON.
    #[serde(default)]
    pub by_op_kind: BTreeMap<String, OpKindCounts>,
}

/// Per-`op_kind` outcome counts inside a [`RecoveryReport`].
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct OpKindCounts {
    pub retried: usize,
    pub failed: usize,
}

impl OpKindCounts {
    /// Total of retried + failed; convenience for log formatting.
    #[must_use]
    pub const fn total(&self) -> usize {
        self.retried + self.failed
    }
}

/// One batch row in a [`RecoveryReport`].
#[derive(Debug, Clone, Serialize)]
pub struct RecoveredBatch {
    /// Batch id, or `None` for single ops that ran without a batch.
    pub batch_id: Option<String>,
    pub op_ids: Vec<i64>,
}

/// Constant reason logged on every recovery-pass failure. Operators
/// see this in `operation_journal.failed_reason` when they audit
/// what crashed; the consistent prefix makes the column easy to
/// filter on.
pub const RECOVERY_FAILED_REASON: &str =
    "daemon crash detected — operation was in flight at restart";

/// Crash-recovery pass: scan `operation_journal` for `progress =
/// 'pending'` rows and mark them all `failed` with a constant
/// reason.
///
/// **Safe by default.** This pass does NOT re-execute the
/// operation — different op kinds need different idempotency
/// reasoning (file-system writes, ASIN promotions, etc.), and
/// silently retrying could double-apply a mutation that mostly
/// succeeded before the crash. Marking pending → failed keeps
/// the operator in the loop: they see exactly what didn't
/// complete and can re-run intentionally.
///
/// Per-op-kind replay handlers can attach later via separate
/// slices. Each handler reads the `pre_state_json` for "is the
/// target still in the expected state?", then either retries
/// (idempotent) or leaves the row at `failed` (non-idempotent
/// or pre-state drift).
///
/// Idempotent: running this twice in succession only flips
/// rows the first call missed; the second call's
/// `pending_batches()` returns an empty list.
///
/// # Errors
///
/// Returns [`JournalError::Sql`] if any SQL step fails. The
/// recovery pass is best-effort — daemon startup should log the
/// error and continue (failing the whole startup on a journal
/// crash sweep would be worse than running un-recovered).
pub async fn recover_pending(pool: &SqlitePool) -> Result<RecoveryReport, JournalError> {
    recover_pending_with(pool, &ReplayRegistry::default()).await
}

/// Same as [`recover_pending`] but consults `registry` first.
///
/// For each pending row:
/// - If `registry` has a [`Replayer`] matching `op_kind`, call
///   it. [`ReplayDecision::Retried`] flips the row to `done` with
///   the handler-supplied post-state; [`ReplayDecision::Skipped`]
///   flips it to `failed` with the handler-supplied reason.
/// - A handler that returns `Err` is treated like no handler — the
///   row falls through to the canonical [`RECOVERY_FAILED_REASON`]
///   so a buggy handler can't break the safe-by-default contract.
/// - No handler → mark `failed` with [`RECOVERY_FAILED_REASON`]
///   (the original [`recover_pending`] behaviour).
///
/// # Errors
///
/// Returns [`JournalError::Sql`] / [`JournalError::Serde`] if any
/// SQL or JSON step fails. Handler errors are absorbed and reported
/// as `failed_count` increments — they don't fail the whole pass,
/// because daemon startup can't usefully react to a per-handler
/// crash beyond logging.
pub async fn recover_pending_with(
    pool: &SqlitePool,
    registry: &ReplayRegistry,
) -> Result<RecoveryReport, JournalError> {
    let batches = pending_batches(pool).await?;
    let mut failed_count = 0usize;
    let mut retried_count = 0usize;
    let mut recovered: Vec<RecoveredBatch> = Vec::with_capacity(batches.len());
    let mut by_op_kind: BTreeMap<String, OpKindCounts> = BTreeMap::new();

    for (batch_id, op_ids) in batches {
        for op_id in &op_ids {
            let entry = get(pool, *op_id).await?;
            let outcome = match registry.get(&entry.op_kind) {
                Some(handler) => handler.try_replay(pool, &entry).await,
                None => Err(JournalError::NotFound(*op_id)),
            };
            let counts = by_op_kind.entry(entry.op_kind.clone()).or_default();
            match outcome {
                Ok(ReplayDecision::Retried(post_state)) => {
                    mark_done(pool, *op_id, &post_state).await?;
                    retried_count += 1;
                    counts.retried += 1;
                }
                Ok(ReplayDecision::Skipped(reason)) => {
                    mark_failed(pool, *op_id, &reason).await?;
                    failed_count += 1;
                    counts.failed += 1;
                }
                Err(_) => {
                    mark_failed(pool, *op_id, RECOVERY_FAILED_REASON).await?;
                    failed_count += 1;
                    counts.failed += 1;
                }
            }
        }
        recovered.push(RecoveredBatch { batch_id, op_ids });
    }

    Ok(RecoveryReport {
        failed_count,
        retried_count,
        batches: recovered,
        by_op_kind,
    })
}

// ── Replay handler registry ───────────────────────────────────────

/// Outcome of a single [`Replayer::try_replay`] call.
#[derive(Debug, Clone, Serialize)]
pub enum ReplayDecision {
    /// Handler successfully re-executed the operation. The inner
    /// JSON becomes the row's `post_state_json` and progress flips
    /// to `done`.
    Retried(Value),
    /// Handler decided the operation should NOT be retried (e.g.
    /// pre-state has drifted, target row was deleted, non-
    /// idempotent invariant violated). Row flips to `failed` with
    /// the supplied reason — preferred over a generic
    /// [`RECOVERY_FAILED_REASON`] because it tells the operator
    /// *why* the handler declined.
    Skipped(String),
}

/// Per-`op_kind` replay strategy.
///
/// A daemon registers one handler per mutating `op_kind` it knows
/// how to idempotently re-execute (or knowingly refuse to). Op
/// kinds with no handler fall through to the safe-by-default
/// failed-reason flush — that's the [`recover_pending`] behaviour.
#[async_trait]
pub trait Replayer: Send + Sync {
    /// Exact `op_kind` this handler claims. Match is exact — no
    /// globs or prefix matching, because two `op_kind`s with a
    /// shared prefix may need entirely different replay logic.
    fn op_kind(&self) -> &'static str;

    /// Re-execute or refuse. `entry` is the full journal row
    /// including `pre_state`; the handler reads it to check
    /// whether the world still matches what the operation expected.
    async fn try_replay(
        &self,
        pool: &SqlitePool,
        entry: &JournalEntry,
    ) -> Result<ReplayDecision, JournalError>;
}

/// Cheap-to-clone dispatch table for [`Replayer`]s.
#[derive(Clone, Default)]
pub struct ReplayRegistry {
    handlers: Arc<HashMap<&'static str, Arc<dyn Replayer>>>,
}

impl ReplayRegistry {
    /// Build a registry from a flat list. Later entries with the
    /// same `op_kind` overwrite earlier ones — the daemon's
    /// registration order decides which wins.
    #[must_use]
    pub fn new(handlers: Vec<Arc<dyn Replayer>>) -> Self {
        let mut m: HashMap<&'static str, Arc<dyn Replayer>> = HashMap::new();
        for h in handlers {
            m.insert(h.op_kind(), h);
        }
        Self {
            handlers: Arc::new(m),
        }
    }

    /// List the `op_kind`s registered. Stable order is not
    /// guaranteed; doctor / debug surfaces should sort.
    #[must_use]
    pub fn op_kinds(&self) -> Vec<&'static str> {
        self.handlers.keys().copied().collect()
    }

    /// Lookup handler by `op_kind`.
    #[must_use]
    pub fn get(&self, op_kind: &str) -> Option<Arc<dyn Replayer>> {
        self.handlers.get(op_kind).cloned()
    }

    /// True when no handlers are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }
}

impl std::fmt::Debug for ReplayRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut kinds = self.op_kinds();
        kinds.sort_unstable();
        f.debug_struct("ReplayRegistry")
            .field("op_kinds", &kinds)
            .finish()
    }
}

// ── Diff renderer ─────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum DiffKind {
    Added,
    Removed,
    Changed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FieldDiff {
    pub path: String,
    pub kind: DiffKind,
    pub before: Option<Value>,
    pub after: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DiffRender {
    pub fields: Vec<FieldDiff>,
}

/// Render the structural diff between two JSON values.
///
/// Both ends are walked as if they're maps at the top level; for
/// non-map inputs a single "" path entry surfaces the scalar diff.
/// Slice B.14 keeps this intentionally shallow (no per-array-index
/// detailed diffs) — array changes record the whole list as
/// `Changed`. The web UI's columnar view + the CLI table consume
/// the same `DiffRender`.
#[must_use]
pub fn render_diff(pre: &Value, post: &Value) -> DiffRender {
    let mut fields = Vec::new();
    walk_diff("", pre, post, &mut fields);
    DiffRender { fields }
}

fn walk_diff(prefix: &str, pre: &Value, post: &Value, out: &mut Vec<FieldDiff>) {
    match (pre, post) {
        (Value::Object(a), Value::Object(b)) => walk_objects(prefix, a, b, out),
        (a, b) if a == b => {}
        (Value::Null, b) => out.push(FieldDiff {
            path: prefix.to_owned(),
            kind: DiffKind::Added,
            before: None,
            after: Some(b.clone()),
        }),
        (a, Value::Null) => out.push(FieldDiff {
            path: prefix.to_owned(),
            kind: DiffKind::Removed,
            before: Some(a.clone()),
            after: None,
        }),
        (a, b) => out.push(FieldDiff {
            path: prefix.to_owned(),
            kind: DiffKind::Changed,
            before: Some(a.clone()),
            after: Some(b.clone()),
        }),
    }
}

fn walk_objects(
    prefix: &str,
    pre: &Map<String, Value>,
    post: &Map<String, Value>,
    out: &mut Vec<FieldDiff>,
) {
    let mut keys: Vec<&String> = pre.keys().chain(post.keys()).collect();
    keys.sort();
    keys.dedup();
    for key in keys {
        let child = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match (pre.get(key), post.get(key)) {
            (Some(a), Some(b)) => walk_diff(&child, a, b, out),
            (None, Some(b)) => out.push(FieldDiff {
                path: child,
                kind: DiffKind::Added,
                before: None,
                after: Some(b.clone()),
            }),
            (Some(a), None) => out.push(FieldDiff {
                path: child,
                kind: DiffKind::Removed,
                before: Some(a.clone()),
                after: None,
            }),
            (None, None) => {}
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use ab_core::tunables::DbTunables;
    use ab_db::LibraryDb;
    use serde_json::json;
    use tempfile::TempDir;

    async fn db() -> (TempDir, LibraryDb) {
        let dir = TempDir::new().expect("tempdir");
        let lib = LibraryDb::open(&dir.path().join("library.db"), &DbTunables::default())
            .await
            .expect("open");
        (dir, lib)
    }

    async fn add_book(db: &LibraryDb, title: &str) -> i64 {
        sqlx::query!("INSERT INTO books (title) VALUES (?)", title)
            .execute(db.pool())
            .await
            .expect("insert")
            .last_insert_rowid()
    }

    #[tokio::test]
    async fn record_then_done_round_trip() {
        let (_d, db) = db().await;
        let bid = add_book(&db, "A").await;
        let id = record(
            db.pool(),
            &NewEntry {
                op_kind: "write-tags-final",
                target: Target {
                    kind: "book".into(),
                    id: bid,
                },
                pre_state: json!({ "title": "A" }),
                reversible: true,
                batch_id: None,
            },
        )
        .await
        .expect("record");
        mark_done(db.pool(), id, &json!({ "title": "Alpha" }))
            .await
            .expect("done");
        let row = get(db.pool(), id).await.expect("get");
        assert_eq!(row.progress, Progress::Done);
        assert_eq!(row.post_state, Some(json!({ "title": "Alpha" })));
    }

    #[tokio::test]
    async fn mark_failed_records_reason() {
        let (_d, db) = db().await;
        let bid = add_book(&db, "A").await;
        let id = record(
            db.pool(),
            &NewEntry {
                op_kind: "write-tags-final",
                target: Target {
                    kind: "book".into(),
                    id: bid,
                },
                pre_state: json!({}),
                reversible: true,
                batch_id: None,
            },
        )
        .await
        .expect("record");
        mark_failed(db.pool(), id, "disk full").await.expect("fail");
        let row = get(db.pool(), id).await.expect("get");
        assert_eq!(row.progress, Progress::Failed);
        assert_eq!(row.failed_reason.as_deref(), Some("disk full"));
    }

    #[tokio::test]
    async fn pending_batches_groups_by_batch_id() {
        let (_d, db) = db().await;
        let bid = add_book(&db, "A").await;
        let batch_a = "batch-a".to_owned();
        for _ in 0..2 {
            let _ = record(
                db.pool(),
                &NewEntry {
                    op_kind: "batch-edit",
                    target: Target {
                        kind: "book".into(),
                        id: bid,
                    },
                    pre_state: json!({}),
                    reversible: true,
                    batch_id: Some(batch_a.clone()),
                },
            )
            .await
            .expect("record");
        }
        let lone = record(
            db.pool(),
            &NewEntry {
                op_kind: "write-tags-final",
                target: Target {
                    kind: "book".into(),
                    id: bid,
                },
                pre_state: json!({}),
                reversible: true,
                batch_id: None,
            },
        )
        .await
        .expect("record");
        mark_done(db.pool(), lone, &json!({})).await.expect("done");

        let groups = pending_batches(db.pool()).await.expect("pending");
        // The "done" entry shouldn't appear; we expect one batch
        // group with two pending ops.
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0.as_deref(), Some("batch-a"));
        assert_eq!(groups[0].1.len(), 2);
    }

    #[test]
    fn diff_detects_added_removed_changed() {
        let pre = json!({ "title": "A", "tags": ["x", "y"], "rating": 3 });
        let post = json!({ "title": "B", "tags": ["x", "y", "z"], "narrator": "Carl" });
        let d = render_diff(&pre, &post);
        // Sort the rendered fields by path so the assertion is
        // independent of map iteration order.
        let mut paths: Vec<&str> = d.fields.iter().map(|f| f.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(paths, vec!["narrator", "rating", "tags", "title"]);
    }

    #[test]
    fn batch_id_round_trips() {
        let a = new_batch_id();
        let b = new_batch_id();
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn recover_pending_marks_pending_rows_failed() {
        let (_d, db) = db().await;
        let bid = add_book(&db, "Crashed").await;
        // Two pending rows from a hypothetical mid-batch crash.
        let batch = new_batch_id();
        let op1 = record(
            db.pool(),
            &NewEntry {
                op_kind: "tag-write-final",
                target: Target {
                    kind: "book".into(),
                    id: bid,
                },
                pre_state: json!({ "title": "Crashed" }),
                reversible: true,
                batch_id: Some(batch.clone()),
            },
        )
        .await
        .expect("record op1");
        let op2 = record(
            db.pool(),
            &NewEntry {
                op_kind: "tag-write-final",
                target: Target {
                    kind: "book".into(),
                    id: bid,
                },
                pre_state: json!({ "subtitle": null }),
                reversible: true,
                batch_id: Some(batch.clone()),
            },
        )
        .await
        .expect("record op2");

        let report = recover_pending(db.pool()).await.expect("recover");
        assert_eq!(report.failed_count, 2);
        assert_eq!(report.batches.len(), 1);
        assert_eq!(report.batches[0].batch_id.as_deref(), Some(batch.as_str()));
        assert_eq!(report.batches[0].op_ids, vec![op1, op2]);

        // Both rows now `failed` with the canonical reason.
        let row1 = get(db.pool(), op1).await.expect("get op1");
        let row2 = get(db.pool(), op2).await.expect("get op2");
        assert_eq!(row1.progress, Progress::Failed);
        assert_eq!(row1.failed_reason.as_deref(), Some(RECOVERY_FAILED_REASON));
        assert_eq!(row2.progress, Progress::Failed);
    }

    #[tokio::test]
    async fn recover_pending_skips_already_done_and_failed_rows() {
        let (_d, db) = db().await;
        let bid = add_book(&db, "Mixed").await;
        // A pending row.
        let pending_op = record(
            db.pool(),
            &NewEntry {
                op_kind: "tag-write-final",
                target: Target {
                    kind: "book".into(),
                    id: bid,
                },
                pre_state: json!({ "title": "Mixed" }),
                reversible: true,
                batch_id: None,
            },
        )
        .await
        .expect("record pending");
        // A done row.
        let done_op = record(
            db.pool(),
            &NewEntry {
                op_kind: "tag-write-final",
                target: Target {
                    kind: "book".into(),
                    id: bid,
                },
                pre_state: json!({ "title": "Mixed" }),
                reversible: true,
                batch_id: None,
            },
        )
        .await
        .expect("record done");
        mark_done(db.pool(), done_op, &json!({ "title": "MIXED" }))
            .await
            .expect("mark done");
        // A failed row (from a prior recovery, say).
        let prior_failed = record(
            db.pool(),
            &NewEntry {
                op_kind: "tag-write-final",
                target: Target {
                    kind: "book".into(),
                    id: bid,
                },
                pre_state: json!({ "title": "Mixed" }),
                reversible: true,
                batch_id: None,
            },
        )
        .await
        .expect("record prior-failed");
        mark_failed(db.pool(), prior_failed, "earlier crash")
            .await
            .expect("mark failed");

        let report = recover_pending(db.pool()).await.expect("recover");
        assert_eq!(
            report.failed_count, 1,
            "only the one pending row should be touched"
        );
        let post_done = get(db.pool(), done_op).await.expect("get done");
        let post_prior = get(db.pool(), prior_failed).await.expect("get prior");
        assert_eq!(post_done.progress, Progress::Done, "done row stays done");
        assert_eq!(
            post_prior.failed_reason.as_deref(),
            Some("earlier crash"),
            "prior-failed reason preserved"
        );
        let post_pending = get(db.pool(), pending_op).await.expect("get pending");
        assert_eq!(post_pending.progress, Progress::Failed);
    }

    #[tokio::test]
    async fn recover_pending_is_idempotent() {
        let (_d, db) = db().await;
        let bid = add_book(&db, "Idem").await;
        record(
            db.pool(),
            &NewEntry {
                op_kind: "tag-write-final",
                target: Target {
                    kind: "book".into(),
                    id: bid,
                },
                pre_state: json!({}),
                reversible: true,
                batch_id: None,
            },
        )
        .await
        .expect("record");

        let r1 = recover_pending(db.pool()).await.expect("recover 1");
        let r2 = recover_pending(db.pool()).await.expect("recover 2");
        assert_eq!(r1.failed_count, 1);
        assert_eq!(
            r2.failed_count, 0,
            "second pass finds no pending rows to flip"
        );
        assert!(r2.batches.is_empty());
    }

    #[tokio::test]
    async fn recover_pending_handles_empty_journal() {
        let (_d, db) = db().await;
        let report = recover_pending(db.pool()).await.expect("recover empty");
        assert_eq!(report.failed_count, 0);
        assert_eq!(report.retried_count, 0);
        assert!(report.batches.is_empty());
        assert!(report.by_op_kind.is_empty());
    }

    // ── ReplayRegistry tests ─────────────────────────────────────

    struct EchoReplayer {
        op_kind: &'static str,
    }

    #[async_trait]
    impl Replayer for EchoReplayer {
        fn op_kind(&self) -> &'static str {
            self.op_kind
        }
        async fn try_replay(
            &self,
            _pool: &SqlitePool,
            entry: &JournalEntry,
        ) -> Result<ReplayDecision, JournalError> {
            // Echo the pre-state as post-state so callers can assert.
            Ok(ReplayDecision::Retried(entry.pre_state.clone()))
        }
    }

    struct SkipReplayer;

    #[async_trait]
    impl Replayer for SkipReplayer {
        fn op_kind(&self) -> &'static str {
            "skip-me"
        }
        async fn try_replay(
            &self,
            _pool: &SqlitePool,
            _entry: &JournalEntry,
        ) -> Result<ReplayDecision, JournalError> {
            Ok(ReplayDecision::Skipped("post-state drifted".into()))
        }
    }

    struct ErrorReplayer;

    #[async_trait]
    impl Replayer for ErrorReplayer {
        fn op_kind(&self) -> &'static str {
            "boom"
        }
        async fn try_replay(
            &self,
            _pool: &SqlitePool,
            entry: &JournalEntry,
        ) -> Result<ReplayDecision, JournalError> {
            Err(JournalError::NotFound(entry.op_id))
        }
    }

    #[tokio::test]
    async fn replay_registry_routes_handler_match_to_done() {
        let (_d, db) = db().await;
        let op_id = record(
            db.pool(),
            &NewEntry {
                op_kind: "retry-me",
                target: Target {
                    kind: "book".into(),
                    id: 1,
                },
                pre_state: json!({ "n": 1 }),
                reversible: true,
                batch_id: None,
            },
        )
        .await
        .expect("record");
        let registry = ReplayRegistry::new(vec![Arc::new(EchoReplayer {
            op_kind: "retry-me",
        })]);
        let report = recover_pending_with(db.pool(), &registry)
            .await
            .expect("recover with");
        assert_eq!(report.retried_count, 1);
        assert_eq!(report.failed_count, 0);
        let entry = get(db.pool(), op_id).await.expect("get");
        assert_eq!(entry.progress, Progress::Done);
        assert_eq!(entry.post_state, Some(json!({ "n": 1 })));
    }

    #[tokio::test]
    async fn replay_registry_skipped_marks_failed_with_reason() {
        let (_d, db) = db().await;
        let op_id = record(
            db.pool(),
            &NewEntry {
                op_kind: "skip-me",
                target: Target {
                    kind: "book".into(),
                    id: 1,
                },
                pre_state: json!({}),
                reversible: true,
                batch_id: None,
            },
        )
        .await
        .expect("record");
        let registry = ReplayRegistry::new(vec![Arc::new(SkipReplayer)]);
        let report = recover_pending_with(db.pool(), &registry)
            .await
            .expect("recover with");
        assert_eq!(report.retried_count, 0);
        assert_eq!(report.failed_count, 1);
        let entry = get(db.pool(), op_id).await.expect("get");
        assert_eq!(entry.progress, Progress::Failed);
        assert_eq!(entry.failed_reason.as_deref(), Some("post-state drifted"));
    }

    #[tokio::test]
    async fn replay_registry_handler_error_falls_through_to_canonical_reason() {
        let (_d, db) = db().await;
        let op_id = record(
            db.pool(),
            &NewEntry {
                op_kind: "boom",
                target: Target {
                    kind: "book".into(),
                    id: 1,
                },
                pre_state: json!({}),
                reversible: true,
                batch_id: None,
            },
        )
        .await
        .expect("record");
        let registry = ReplayRegistry::new(vec![Arc::new(ErrorReplayer)]);
        let report = recover_pending_with(db.pool(), &registry)
            .await
            .expect("recover with");
        assert_eq!(report.failed_count, 1);
        let entry = get(db.pool(), op_id).await.expect("get");
        assert_eq!(entry.progress, Progress::Failed);
        assert_eq!(entry.failed_reason.as_deref(), Some(RECOVERY_FAILED_REASON));
    }

    #[tokio::test]
    async fn replay_registry_op_kind_with_no_handler_uses_canonical_reason() {
        let (_d, db) = db().await;
        let op_id = record(
            db.pool(),
            &NewEntry {
                op_kind: "unhandled",
                target: Target {
                    kind: "book".into(),
                    id: 1,
                },
                pre_state: json!({}),
                reversible: true,
                batch_id: None,
            },
        )
        .await
        .expect("record");
        // Empty registry → behaves exactly like recover_pending.
        let report = recover_pending_with(db.pool(), &ReplayRegistry::default())
            .await
            .expect("recover with empty");
        assert_eq!(report.failed_count, 1);
        let entry = get(db.pool(), op_id).await.expect("get");
        assert_eq!(entry.progress, Progress::Failed);
        assert_eq!(entry.failed_reason.as_deref(), Some(RECOVERY_FAILED_REASON));
    }

    #[test]
    fn replay_registry_op_kinds_lists_registered_kinds() {
        let registry = ReplayRegistry::new(vec![
            Arc::new(EchoReplayer { op_kind: "a" }),
            Arc::new(EchoReplayer { op_kind: "b" }),
        ]);
        let mut kinds = registry.op_kinds();
        kinds.sort_unstable();
        assert_eq!(kinds, vec!["a", "b"]);
        assert!(!registry.is_empty());
    }

    #[tokio::test]
    async fn recover_pending_with_splits_counts_per_op_kind() {
        let (_d, db) = db().await;
        // Two retried (tag-write-final) + one failed (skip-me) + one
        // unhandled (no replayer, falls through to canonical reason).
        for _ in 0..2 {
            record(
                db.pool(),
                &NewEntry {
                    op_kind: "tag-write-final",
                    target: Target {
                        kind: "book".into(),
                        id: 1,
                    },
                    pre_state: json!({ "ok": true }),
                    reversible: true,
                    batch_id: None,
                },
            )
            .await
            .expect("record tag-write");
        }
        record(
            db.pool(),
            &NewEntry {
                op_kind: "skip-me",
                target: Target {
                    kind: "book".into(),
                    id: 1,
                },
                pre_state: json!({}),
                reversible: true,
                batch_id: None,
            },
        )
        .await
        .expect("record skip");
        record(
            db.pool(),
            &NewEntry {
                op_kind: "unhandled",
                target: Target {
                    kind: "book".into(),
                    id: 1,
                },
                pre_state: json!({}),
                reversible: true,
                batch_id: None,
            },
        )
        .await
        .expect("record unhandled");

        let registry = ReplayRegistry::new(vec![
            Arc::new(EchoReplayer {
                op_kind: "tag-write-final",
            }),
            Arc::new(SkipReplayer),
        ]);
        let report = recover_pending_with(db.pool(), &registry)
            .await
            .expect("recover with");

        assert_eq!(report.retried_count, 2);
        assert_eq!(report.failed_count, 2);
        let tag = report
            .by_op_kind
            .get("tag-write-final")
            .expect("tag bucket");
        assert_eq!(tag.retried, 2);
        assert_eq!(tag.failed, 0);
        assert_eq!(tag.total(), 2);
        let skip = report.by_op_kind.get("skip-me").expect("skip bucket");
        assert_eq!(skip.retried, 0);
        assert_eq!(skip.failed, 1);
        let unhandled = report
            .by_op_kind
            .get("unhandled")
            .expect("unhandled bucket");
        assert_eq!(unhandled.retried, 0);
        assert_eq!(unhandled.failed, 1);
    }

    #[test]
    fn replay_registry_default_is_empty() {
        let registry = ReplayRegistry::default();
        assert!(registry.is_empty());
        assert!(registry.op_kinds().is_empty());
        assert!(registry.get("anything").is_none());
    }
}
