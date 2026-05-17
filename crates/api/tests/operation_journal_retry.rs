// Integration test target — same lint relaxation as other
// ab-api integration tests.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::doc_markdown
)]

//! Integration tests for `POST /api/v1/operation_journal/{op_id}/retry`.
//!
//! Exercises the four branches the handler distinguishes:
//! - 200 Retried (a Replayer returns `ReplayDecision::Retried`)
//! - 200 Skipped (a Replayer returns `ReplayDecision::Skipped(reason)`)
//! - 404 unknown op_id
//! - 404 row exists but no Replayer registered for its op_kind
//! - 409 row exists in a terminal progress state

use std::sync::Arc;

use ab_api::ApiState;
use ab_api::build_router;
use ab_core::auth::{hash_api_token, mint_api_token};
use ab_core::tunables::{DbTunables, SchedulerTunables, SecurityTunables};
use ab_db::{EphemeralDb, LibraryDb};
use ab_journal::{JournalEntry, JournalError, ReplayDecision, ReplayRegistry, Replayer};
use ab_pipeline::cleanup::CleanupRegistry;
use ab_pipeline::{Dag, Scheduler, StageContext};
use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use sqlx::SqlitePool;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;

struct RetriedReplayer;
#[async_trait]
impl Replayer for RetriedReplayer {
    fn op_kind(&self) -> &'static str {
        "tag-write-final"
    }
    async fn try_replay(
        &self,
        _pool: &SqlitePool,
        entry: &JournalEntry,
    ) -> Result<ReplayDecision, JournalError> {
        Ok(ReplayDecision::Retried(entry.pre_state.clone()))
    }
}

struct SkippedReplayer;
#[async_trait]
impl Replayer for SkippedReplayer {
    fn op_kind(&self) -> &'static str {
        "audiologo-cut"
    }
    async fn try_replay(
        &self,
        _pool: &SqlitePool,
        _entry: &JournalEntry,
    ) -> Result<ReplayDecision, JournalError> {
        Ok(ReplayDecision::Skipped("pre-state drifted".into()))
    }
}

async fn fresh_setup(registry: ReplayRegistry) -> (axum::Router, ApiState, TempDir) {
    let tmp = TempDir::new().expect("tempdir");
    let library = LibraryDb::open(&tmp.path().join("library.db"), &DbTunables::default())
        .await
        .expect("open library");
    let ephemeral = EphemeralDb::open(&tmp.path().join("ephemeral.db"), &DbTunables::default())
        .await
        .expect("open ephemeral");

    let dag = Arc::new(Dag::build(Vec::new()).expect("empty dag"));
    let cancel = CancellationToken::new();
    let ctx = StageContext {
        library: library.clone(),
        ephemeral: ephemeral.clone(),
        cancel: cancel.clone(),
        stage_name: "journal-retry-test",
    };
    let scheduler = Arc::new(Scheduler::spawn(
        Arc::clone(&dag),
        ctx,
        &SchedulerTunables::default(),
    ));

    let state = ApiState::with_replay_registry(
        library,
        ephemeral,
        scheduler,
        dag,
        CleanupRegistry::new(Vec::new()),
        cancel,
        SecurityTunables::default(),
        globset::GlobSet::empty(),
        ab_background::BackgroundRegistry::new(vec![]),
        ab_api::doctor::DoctorRegistry::new(vec![]),
        registry,
    );
    let router = build_router(state.clone());
    (router, state, tmp)
}

async fn mint_token(state: &ApiState) -> String {
    let raw = mint_api_token();
    let hash = hash_api_token(&raw);
    sqlx::query(
        "INSERT INTO tokens (user_id, token_hash, nickname, scopes) VALUES (1, ?, ?, '[]')",
    )
    .bind(&hash)
    .bind("test-token")
    .execute(state.inner.library.pool())
    .await
    .expect("insert token");
    raw
}

async fn insert_pending(lib: &LibraryDb, op_kind: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO operation_journal \
            (op_kind, target_kind, target_id, pre_state_json, progress) \
         VALUES (?, 'book', 1, '{}', 'pending') RETURNING op_id",
    )
    .bind(op_kind)
    .fetch_one(lib.pool())
    .await
    .expect("insert pending")
}

async fn read_progress(lib: &LibraryDb, op_id: i64) -> String {
    sqlx::query_scalar::<_, String>("SELECT progress FROM operation_journal WHERE op_id = ?")
        .bind(op_id)
        .fetch_one(lib.pool())
        .await
        .expect("read progress")
}

async fn retry(router: &axum::Router, token: &str, op_id: i64) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(format!("/operation_journal/{op_id}/retry"))
        .header("Authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .expect("build req");
    let res = router.clone().oneshot(req).await.expect("oneshot");
    let status = res.status();
    let body_bytes = axum::body::to_bytes(res.into_body(), 1 << 16)
        .await
        .expect("body");
    let body: Value = serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);
    (status, body)
}

#[tokio::test]
async fn retry_returns_retried_when_replayer_succeeds() {
    let registry = ReplayRegistry::new(vec![Arc::new(RetriedReplayer)]);
    let (router, state, _tmp) = fresh_setup(registry).await;
    let token = mint_token(&state).await;
    let op_id = insert_pending(&state.inner.library, "tag-write-final").await;

    let (status, body) = retry(&router, &token, op_id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["outcome"], "retried");
    assert_eq!(body["op_id"], op_id);
    assert_eq!(body["op_kind"], "tag-write-final");
    assert_eq!(read_progress(&state.inner.library, op_id).await, "done");
}

#[tokio::test]
async fn retry_returns_skipped_when_replayer_declines() {
    let registry = ReplayRegistry::new(vec![Arc::new(SkippedReplayer)]);
    let (router, state, _tmp) = fresh_setup(registry).await;
    let token = mint_token(&state).await;
    let op_id = insert_pending(&state.inner.library, "audiologo-cut").await;

    let (status, body) = retry(&router, &token, op_id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["outcome"], "skipped");
    assert_eq!(body["reason"], "pre-state drifted");
    assert_eq!(read_progress(&state.inner.library, op_id).await, "failed");
}

#[tokio::test]
async fn retry_404_when_op_id_does_not_exist() {
    let (router, state, _tmp) = fresh_setup(ReplayRegistry::default()).await;
    let token = mint_token(&state).await;
    let (status, _body) = retry(&router, &token, 99_999).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn retry_404_when_no_replayer_registered_for_op_kind() {
    let (router, state, _tmp) = fresh_setup(ReplayRegistry::default()).await;
    let token = mint_token(&state).await;
    let op_id = insert_pending(&state.inner.library, "unhandled-kind").await;

    let (status, _body) = retry(&router, &token, op_id).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Untouched — must still be pending.
    assert_eq!(read_progress(&state.inner.library, op_id).await, "pending");
}

#[tokio::test]
async fn retry_409_when_row_is_already_terminal() {
    let registry = ReplayRegistry::new(vec![Arc::new(RetriedReplayer)]);
    let (router, state, _tmp) = fresh_setup(registry).await;
    let token = mint_token(&state).await;
    let op_id = insert_pending(&state.inner.library, "tag-write-final").await;

    // First retry succeeds → row goes to 'done'.
    let (status, _body) = retry(&router, &token, op_id).await;
    assert_eq!(status, StatusCode::OK);

    // Second retry sees a non-pending row → 409.
    let (status, _body) = retry(&router, &token, op_id).await;
    assert_eq!(status, StatusCode::CONFLICT);
}
