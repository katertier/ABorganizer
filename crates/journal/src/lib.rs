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
}
