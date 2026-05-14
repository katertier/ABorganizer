//! Background-task registry endpoints (ADR-0035, slice B.13).
//!
//! Two routes:
//!
//! * `GET  /background/tasks`              — registry view.
//! * `POST /background/tasks/{name}/run`   — manual trigger.

use ab_background::{TaskCtx, execute_task, read_state};
use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;

use crate::error::ApiError;
use crate::state::ApiState;

#[derive(Serialize)]
pub struct BackgroundTaskRow {
    pub name: &'static str,
    pub interval_secs: u64,
    pub last_run_at: Option<i64>,
    pub last_status: Option<String>,
    pub last_summary: Option<String>,
    pub consecutive_failures: u32,
}

#[derive(Serialize)]
pub struct BackgroundTasksResponse {
    pub tasks: Vec<BackgroundTaskRow>,
}

/// `GET /api/v1/background/tasks` — list every registered task
/// with its latest state row from `background_task_state`.
pub async fn background_tasks_list(
    State(state): State<ApiState>,
) -> Result<Json<BackgroundTasksResponse>, ApiError> {
    let pool = state.inner.ephemeral.pool();
    let mut rows = Vec::with_capacity(state.inner.background.len());
    for name in state.inner.background.names() {
        let Some(task) = state.inner.background.get(name) else {
            continue;
        };
        let s = read_state(pool, name).await.map_err(|e| {
            ApiError::Internal(ab_core::Error::Database(format!("background list: {e}")))
        })?;
        rows.push(BackgroundTaskRow {
            name,
            interval_secs: task.interval().as_secs(),
            last_run_at: s.last_run_at,
            last_status: s.last_status,
            last_summary: s.last_summary,
            consecutive_failures: s.consecutive_failures,
        });
    }
    Ok(Json(BackgroundTasksResponse { tasks: rows }))
}

#[derive(Serialize)]
pub struct RunAck {
    pub task: String,
    pub triggered: bool,
}

/// `POST /api/v1/background/tasks/{name}/run` — manual trigger.
/// Out-of-band; does NOT reset the natural cadence. Returns 404
/// when the task name doesn't match the registry.
pub async fn background_task_run(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> Result<Json<RunAck>, ApiError> {
    let task = state
        .inner
        .background
        .get(name.as_str())
        .ok_or_else(|| ApiError::NotFound(format!("background task {name}")))?;
    let ctx = TaskCtx {
        library: state.inner.library.clone(),
        ephemeral: state.inner.ephemeral.clone(),
        cancel: state.inner.cancel.clone(),
    };
    execute_task(&ctx, task).await;
    Ok(Json(RunAck {
        task: name,
        triggered: true,
    }))
}
