//! Background-task registry (ADR-0035).
//!
//! Long-lived non-stage tasks (library rescan, refresh-stale-audnexus,
//! cover refresh, VACUUM, update check, companion-hint cleanup) share
//! the same shape: tunable interval, idle priority, per-task last-run
//! timestamp persisted in `ephemeral.db`, optional manual trigger via
//! API.
//!
//! ## Slice scope
//!
//! Foundation only — the trait, registry, scheduling loop, ephemeral
//! state table, and an `update-check` placeholder task that serves as
//! smoke proof. The full task lineup (library-rescan #125,
//! refresh-stale-audnexus, cover-refresh, VACUUM, etc.) lands in
//! follow-up slices that each implement the [`BackgroundTask`] trait.

#![forbid(unsafe_code)]
#![allow(missing_docs)] // scaffold; tightened in follow-up slices

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use ab_db::{EphemeralDb, LibraryDb};
use async_trait::async_trait;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

/// Counts + a one-liner suitable for the `/background/tasks`
/// endpoint.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TaskReport {
    pub processed: u64,
    pub skipped: u64,
    pub errors: u64,
    pub summary: String,
}

impl TaskReport {
    pub fn ok(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            ..Self::default()
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    #[error(transparent)]
    Core(#[from] ab_core::Error),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error("{0}")]
    Other(String),
}

/// Context handed to every task invocation.
#[derive(Clone)]
pub struct TaskCtx {
    pub library: LibraryDb,
    pub ephemeral: EphemeralDb,
    pub cancel: CancellationToken,
}

/// A long-lived periodic task running at `Priority::Idle`.
///
/// Implementations are dyn-compatible; the registry owns them as
/// `Arc<dyn BackgroundTask>`.
#[async_trait]
pub trait BackgroundTask: Send + Sync {
    /// Stable identifier — surfaces in `/background/tasks` rows and
    /// `aborg background run <name>` invocations.
    fn name(&self) -> &'static str;

    /// How often this task should fire when nothing intervenes.
    fn interval(&self) -> Duration;

    /// Do the work. Returns a report on success or `TaskError` on
    /// failure; failures increment the consecutive-failure counter
    /// and feed exponential backoff (handled by the loop).
    async fn run(&self, ctx: &TaskCtx) -> Result<TaskReport, TaskError>;
}

/// Cheap-to-clone registry; loops + handlers share one instance.
#[derive(Clone)]
pub struct BackgroundRegistry {
    tasks: Arc<BTreeMap<&'static str, Arc<dyn BackgroundTask>>>,
}

impl BackgroundRegistry {
    pub fn new(tasks: Vec<Arc<dyn BackgroundTask>>) -> Self {
        let mut map: BTreeMap<&'static str, Arc<dyn BackgroundTask>> = BTreeMap::new();
        for t in tasks {
            map.insert(t.name(), t);
        }
        Self {
            tasks: Arc::new(map),
        }
    }

    /// Stable, alphabetically ordered list of registered task names.
    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.tasks.keys().copied().collect()
    }

    /// Look up a registered task by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn BackgroundTask>> {
        self.tasks.get(name).cloned()
    }

    /// Number of registered tasks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }
}

impl std::fmt::Debug for BackgroundRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackgroundRegistry")
            .field("tasks", &self.tasks.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Persisted state for one task.
#[derive(Debug, Clone, Serialize)]
pub struct TaskState {
    pub task_name: String,
    pub last_run_at: Option<i64>,
    pub last_status: Option<String>,
    pub last_summary: Option<String>,
    pub consecutive_failures: u32,
}

/// Read the persisted state row (or default if never run).
pub async fn read_state(pool: &sqlx::SqlitePool, task_name: &str) -> Result<TaskState, TaskError> {
    let row = sqlx::query!(
        r#"SELECT
            last_run_at          AS "last_run_at: i64",
            last_status          AS "last_status: String",
            last_summary         AS "last_summary: String",
            consecutive_failures AS "consecutive_failures!: i64"
         FROM background_task_state WHERE task_name = ?"#,
        task_name,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map_or_else(
        || TaskState {
            task_name: task_name.to_owned(),
            last_run_at: None,
            last_status: None,
            last_summary: None,
            consecutive_failures: 0,
        },
        |r| TaskState {
            task_name: task_name.to_owned(),
            last_run_at: r.last_run_at,
            last_status: r.last_status,
            last_summary: r.last_summary,
            consecutive_failures: u32::try_from(r.consecutive_failures).unwrap_or(0),
        },
    ))
}

async fn record_run(
    pool: &sqlx::SqlitePool,
    name: &str,
    success: bool,
    summary: &str,
) -> Result<(), TaskError> {
    let now = now_unix_seconds();
    let status = if success { "ok" } else { "error" };
    let delta: i64 = i64::from(!success);
    let reset: i64 = i64::from(!success);
    sqlx::query!(
        "INSERT INTO background_task_state (
             task_name, last_run_at, last_status, last_summary,
             consecutive_failures
         ) VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(task_name) DO UPDATE SET
             last_run_at  = excluded.last_run_at,
             last_status  = excluded.last_status,
             last_summary = excluded.last_summary,
             consecutive_failures = CASE WHEN ? = 1
                 THEN background_task_state.consecutive_failures + ?
                 ELSE 0
             END",
        name,
        now,
        status,
        summary,
        delta,
        reset,
        delta,
    )
    .execute(pool)
    .await?;
    Ok(())
}

fn now_unix_seconds() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    i64::try_from(secs).unwrap_or(i64::MAX)
}

/// Maximum back-off cap when a task fails repeatedly.
pub const MAX_BACKOFF: Duration = Duration::from_secs(24 * 60 * 60);

/// Compute the next `next_run = last_run + interval * (2 ^ failures)`
/// with the failure exponent clamped to keep the multiplier under
/// the day cap.
fn next_run_at(state: &TaskState, base_interval: Duration) -> i64 {
    let last = state.last_run_at.unwrap_or(0);
    let base = base_interval.as_secs();
    let mult: u64 = if state.consecutive_failures >= 3 {
        let exp = (state.consecutive_failures - 2).min(20);
        1u64 << exp
    } else {
        1
    };
    let scaled = base.saturating_mul(mult).min(MAX_BACKOFF.as_secs());
    last.saturating_add(i64::try_from(scaled).unwrap_or(i64::MAX))
}

/// Inputs for the loop. Mirrors the `CleanupLoopCtx` shape so the
/// daemon-main wiring reads symmetrically.
#[derive(Clone)]
pub struct BackgroundLoopCtx {
    pub ctx: TaskCtx,
    pub registry: BackgroundRegistry,
    pub tick_interval: Duration,
}

/// Run the scheduling loop until `cancel` fires.
///
/// Each tick:
/// 1. Sleep `tick_interval` (default 60s; configurable via tunables).
/// 2. For each registered task, fetch state + compute `next_run`.
/// 3. If `now >= next_run`, invoke `run(ctx)`; persist the result.
pub async fn run_background_loop(loop_ctx: BackgroundLoopCtx, cancel: CancellationToken) {
    let BackgroundLoopCtx {
        ctx,
        registry,
        tick_interval,
    } = loop_ctx;
    if tick_interval.is_zero() {
        tracing::info!("background.loop.disabled");
        return;
    }
    tracing::info!(
        tick_secs = tick_interval.as_secs(),
        tasks = registry.len(),
        "background.loop.start",
    );
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                tracing::info!("background.loop.stop");
                return;
            }
            () = tokio::time::sleep(tick_interval) => {
                tick(&ctx, &registry).await;
            }
        }
    }
}

async fn tick(ctx: &TaskCtx, registry: &BackgroundRegistry) {
    let now = now_unix_seconds();
    for name in registry.names() {
        let Some(task) = registry.get(name) else {
            continue;
        };
        match read_state(ctx.ephemeral.pool(), name).await {
            Ok(state) => {
                let scheduled = next_run_at(&state, task.interval());
                if now < scheduled {
                    continue;
                }
                execute_task(ctx, task).await;
            }
            Err(err) => {
                tracing::warn!(task = name, error = %err, "background.state.read_failed");
            }
        }
    }
}

/// Execute one task and persist the resulting state row.
pub async fn execute_task(ctx: &TaskCtx, task: Arc<dyn BackgroundTask>) {
    let name = task.name();
    tracing::info!(task = name, "background.task.run.start");
    match task.run(ctx).await {
        Ok(report) => {
            tracing::info!(
                task = name,
                processed = report.processed,
                skipped = report.skipped,
                errors = report.errors,
                "background.task.run.ok",
            );
            if let Err(e) = record_run(ctx.ephemeral.pool(), name, true, &report.summary).await {
                tracing::warn!(task = name, error = %e, "background.state.write_failed");
            }
        }
        Err(err) => {
            let msg = err.to_string();
            tracing::warn!(task = name, error = %msg, "background.task.run.err");
            if let Err(e) = record_run(ctx.ephemeral.pool(), name, false, &msg).await {
                tracing::warn!(task = name, error = %e, "background.state.write_failed");
            }
        }
    }
}

// ── A minimal task that proves the loop works ──────────────────────

/// Placeholder task — performs no real work but exercises the
/// registry / loop / persistence end-to-end. Replaced or removed
/// once the real tasks (library-rescan etc.) ship.
pub struct HeartbeatTask {
    pub interval: Duration,
}

#[async_trait]
impl BackgroundTask for HeartbeatTask {
    fn name(&self) -> &'static str {
        "heartbeat"
    }
    fn interval(&self) -> Duration {
        self.interval
    }
    async fn run(&self, _ctx: &TaskCtx) -> Result<TaskReport, TaskError> {
        Ok(TaskReport::ok("heartbeat ok"))
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use ab_core::tunables::DbTunables;
    use tempfile::TempDir;

    async fn open_dbs() -> (TempDir, TaskCtx) {
        let dir = TempDir::new().expect("tempdir");
        let tun = DbTunables::default();
        let library = LibraryDb::open(&dir.path().join("library.db"), &tun)
            .await
            .expect("open library");
        let ephemeral = EphemeralDb::open(&dir.path().join("ephemeral.db"), &tun)
            .await
            .expect("open ephemeral");
        (
            dir,
            TaskCtx {
                library,
                ephemeral,
                cancel: CancellationToken::new(),
            },
        )
    }

    #[tokio::test]
    async fn registry_keys_sorted() {
        struct A;
        struct B;
        #[async_trait]
        impl BackgroundTask for A {
            fn name(&self) -> &'static str {
                "alpha"
            }
            fn interval(&self) -> Duration {
                Duration::from_secs(60)
            }
            async fn run(&self, _ctx: &TaskCtx) -> Result<TaskReport, TaskError> {
                Ok(TaskReport::ok("a"))
            }
        }
        #[async_trait]
        impl BackgroundTask for B {
            fn name(&self) -> &'static str {
                "beta"
            }
            fn interval(&self) -> Duration {
                Duration::from_secs(60)
            }
            async fn run(&self, _ctx: &TaskCtx) -> Result<TaskReport, TaskError> {
                Ok(TaskReport::ok("b"))
            }
        }
        let r = BackgroundRegistry::new(vec![Arc::new(B), Arc::new(A)]);
        assert_eq!(r.names(), vec!["alpha", "beta"]);
    }

    #[tokio::test]
    async fn execute_persists_state() {
        let (_d, ctx) = open_dbs().await;
        let task: Arc<dyn BackgroundTask> = Arc::new(HeartbeatTask {
            interval: Duration::from_secs(60),
        });
        execute_task(&ctx, Arc::clone(&task)).await;
        let s = read_state(ctx.ephemeral.pool(), "heartbeat")
            .await
            .expect("read");
        assert_eq!(s.last_status.as_deref(), Some("ok"));
        assert!(s.last_run_at.is_some());
        assert_eq!(s.consecutive_failures, 0);
    }

    #[tokio::test]
    async fn failure_increments_consecutive_count() {
        struct Bad;
        #[async_trait]
        impl BackgroundTask for Bad {
            fn name(&self) -> &'static str {
                "bad"
            }
            fn interval(&self) -> Duration {
                Duration::from_secs(60)
            }
            async fn run(&self, _ctx: &TaskCtx) -> Result<TaskReport, TaskError> {
                Err(TaskError::Other("boom".into()))
            }
        }
        let (_d, ctx) = open_dbs().await;
        let task: Arc<dyn BackgroundTask> = Arc::new(Bad);
        execute_task(&ctx, Arc::clone(&task)).await;
        execute_task(&ctx, Arc::clone(&task)).await;
        let s = read_state(ctx.ephemeral.pool(), "bad").await.expect("read");
        assert_eq!(s.consecutive_failures, 2);
        assert_eq!(s.last_status.as_deref(), Some("error"));
    }

    #[test]
    fn backoff_kicks_in_after_three_failures() {
        let base = Duration::from_secs(60);
        let s_two = TaskState {
            task_name: "t".into(),
            last_run_at: Some(0),
            last_status: Some("error".into()),
            last_summary: None,
            consecutive_failures: 2,
        };
        let s_three = TaskState {
            consecutive_failures: 3,
            ..s_two.clone()
        };
        let s_four = TaskState {
            consecutive_failures: 4,
            ..s_two.clone()
        };
        assert_eq!(next_run_at(&s_two, base), 60); // no backoff yet
        assert_eq!(next_run_at(&s_three, base), 120); // 2× interval
        assert_eq!(next_run_at(&s_four, base), 240); // 4× interval
    }

    #[test]
    fn backoff_capped_at_day() {
        let base = Duration::from_secs(60);
        let s = TaskState {
            task_name: "t".into(),
            last_run_at: Some(0),
            last_status: Some("error".into()),
            last_summary: None,
            consecutive_failures: 20,
        };
        let cap = i64::try_from(MAX_BACKOFF.as_secs()).expect("cap fits");
        assert_eq!(next_run_at(&s, base), cap);
    }
}
