// Integration test target — same lint relaxation as other
// ab-api integration tests.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::doc_markdown
)]

//! Integration tests for the `operation_journal` capture half of
//! `PATCH /books/{id}/status` (ADR-0039 step 1).
//!
//! Asserts:
//! - On success: a `book-status-set` row is inserted with
//!   `progress = 'done'`, `pre_state` carries `{current, intent}`,
//!   and `post_state` carries `{reading_status}`.
//! - On non-existent book: the handler returns 404 and no
//!   journal row is created (we don't want pending stragglers
//!   for operations that never started).
//! - The `reversible` column lands as `1` (boolean true).
//! - `op_kind` matches the published `OP_KIND_BOOK_STATUS_SET`
//!   constant so the future StatusReplayer slice can claim it.

use std::sync::Arc;

use ab_api::ApiState;
use ab_api::build_router;
use ab_api::progress::OP_KIND_BOOK_STATUS_SET;
use ab_core::auth::{hash_api_token, mint_api_token};
use ab_core::tunables::{DbTunables, SchedulerTunables, SecurityTunables};
use ab_db::{EphemeralDb, LibraryDb};
use ab_pipeline::cleanup::CleanupRegistry;
use ab_pipeline::{Dag, Scheduler, StageContext};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;

async fn fresh_setup() -> (axum::Router, ApiState, TempDir) {
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
        stage_name: "journal-capture-status-test",
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
        ab_api::doctor::DoctorRegistry::new(vec![]),
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

async fn seed_book(lib: &LibraryDb, title: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO books (title, duration_ms, raw_duration_ms) \
         VALUES (?, 60000, 60000) RETURNING book_id",
    )
    .bind(title)
    .fetch_one(lib.pool())
    .await
    .expect("insert book")
}

async fn patch_status(
    router: &axum::Router,
    token: &str,
    book_id: i64,
    status: &str,
) -> StatusCode {
    let body = format!(r#"{{"reading_status":"{status}"}}"#);
    let req = Request::builder()
        .method("PATCH")
        .uri(format!("/books/{book_id}/status"))
        .header("Authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("build req");
    router.clone().oneshot(req).await.expect("oneshot").status()
}

#[derive(sqlx::FromRow, Debug)]
struct JournalSnapshot {
    op_kind: String,
    target_kind: String,
    target_id: i64,
    progress: String,
    reversible: i64,
    pre_state_json: String,
    post_state_json: Option<String>,
}

async fn snapshot_journal(lib: &LibraryDb) -> Vec<JournalSnapshot> {
    sqlx::query_as::<_, JournalSnapshot>(
        "SELECT op_kind, target_kind, target_id, progress, reversible, \
                pre_state_json, post_state_json \
           FROM operation_journal ORDER BY op_id",
    )
    .fetch_all(lib.pool())
    .await
    .expect("read journal")
}

#[tokio::test]
async fn status_patch_writes_done_journal_row_on_success() {
    let (router, state, _tmp) = fresh_setup().await;
    let token = mint_token(&state).await;
    let book_id = seed_book(&state.inner.library, "Foo").await;

    let status = patch_status(&router, &token, book_id, "reading").await;
    assert_eq!(status, StatusCode::OK);

    let rows = snapshot_journal(&state.inner.library).await;
    assert_eq!(rows.len(), 1, "exactly one journal row");
    let r = &rows[0];
    assert_eq!(r.op_kind, OP_KIND_BOOK_STATUS_SET);
    assert_eq!(r.op_kind, "book-status-set");
    assert_eq!(r.target_kind, "book");
    assert_eq!(r.target_id, book_id);
    assert_eq!(r.progress, "done");
    assert_eq!(r.reversible, 1);

    let pre: Value = serde_json::from_str(&r.pre_state_json).expect("pre json");
    assert_eq!(pre["current"], "want_to_read"); // schema default
    assert_eq!(pre["intent"], "reading");

    let post: Value = serde_json::from_str(r.post_state_json.as_deref().expect("post present"))
        .expect("post json");
    assert_eq!(post["reading_status"], "reading");
}

#[tokio::test]
async fn status_patch_missing_book_returns_404_and_no_journal_row() {
    let (router, state, _tmp) = fresh_setup().await;
    let token = mint_token(&state).await;

    let status = patch_status(&router, &token, 9999, "reading").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let rows = snapshot_journal(&state.inner.library).await;
    assert!(
        rows.is_empty(),
        "no journal row should be created for a non-existent book; got {rows:?}"
    );
}

#[tokio::test]
async fn status_patch_records_each_call_distinctly() {
    let (router, state, _tmp) = fresh_setup().await;
    let token = mint_token(&state).await;
    let book_id = seed_book(&state.inner.library, "Iter").await;

    // Three flips: want_to_read → reading → finished → reading.
    assert_eq!(
        patch_status(&router, &token, book_id, "reading").await,
        StatusCode::OK,
    );
    assert_eq!(
        patch_status(&router, &token, book_id, "finished").await,
        StatusCode::OK,
    );
    assert_eq!(
        patch_status(&router, &token, book_id, "reading").await,
        StatusCode::OK,
    );

    let rows = snapshot_journal(&state.inner.library).await;
    assert_eq!(rows.len(), 3, "one row per patch");
    let intents: Vec<String> = rows
        .iter()
        .map(|r| {
            serde_json::from_str::<Value>(&r.pre_state_json).unwrap()["intent"]
                .as_str()
                .map_or_else(|| "?".to_owned(), str::to_owned)
        })
        .collect();
    assert_eq!(intents, vec!["reading", "finished", "reading"]);

    // pre_state.current must reflect the previous mutation, not the initial state.
    let pre0: Value = serde_json::from_str(&rows[0].pre_state_json).unwrap();
    let pre1: Value = serde_json::from_str(&rows[1].pre_state_json).unwrap();
    let pre2: Value = serde_json::from_str(&rows[2].pre_state_json).unwrap();
    assert_eq!(pre0["current"], "want_to_read");
    assert_eq!(pre1["current"], "reading");
    assert_eq!(pre2["current"], "finished");
}
