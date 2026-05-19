// Integration test target — same lint relaxation as the other
// ab-api integration tests.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::doc_markdown
)]

//! Integration tests for the `operation_journal` capture half of
//! `PATCH /books/{id}` (ADR-0039).
//!
//! The cycle-33 #219 capture for the title field was gated on the
//! request being title-only. Cycle 34 slot 1 lifts that guard:
//! title now captures even when the PATCH includes other fields.
//! The journal-finalize pass runs uniformly across every captured
//! field on tx commit (or rollback) so future per-field captures
//! plug in without touching the finalize loop.

use std::sync::Arc;

use ab_api::ApiState;
use ab_api::build_router;
use ab_api::router::OP_KIND_BOOK_TITLE_SET;
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
        stage_name: "journal-capture-books-patch-test",
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

async fn patch_books(
    router: &axum::Router,
    token: &str,
    book_id: i64,
    body_json: &str,
) -> StatusCode {
    let req = Request::builder()
        .method("PATCH")
        .uri(format!("/books/{book_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body_json.to_owned()))
        .expect("build req");
    router.clone().oneshot(req).await.expect("oneshot").status()
}

#[derive(sqlx::FromRow, Debug)]
struct TitleJournalSnapshot {
    op_kind: String,
    target_kind: String,
    target_id: i64,
    progress: String,
    pre_state_json: String,
    post_state_json: Option<String>,
}

async fn title_journal_rows(lib: &LibraryDb) -> Vec<TitleJournalSnapshot> {
    sqlx::query_as::<_, TitleJournalSnapshot>(
        "SELECT op_kind, target_kind, target_id, progress, \
                pre_state_json, post_state_json \
           FROM operation_journal \
          WHERE op_kind = 'book-title-set' \
          ORDER BY op_id",
    )
    .fetch_all(lib.pool())
    .await
    .expect("read title journal rows")
}

#[tokio::test]
async fn title_only_patch_writes_done_journal_row() {
    let (router, state, _tmp) = fresh_setup().await;
    let token = mint_token(&state).await;
    let book_id = seed_book(&state.inner.library, "Old Title").await;

    let status = patch_books(&router, &token, book_id, r#"{"title":"New Title"}"#).await;
    assert_eq!(status, StatusCode::OK);

    let rows = title_journal_rows(&state.inner.library).await;
    assert_eq!(rows.len(), 1, "title-only PATCH writes one journal row");
    let r = &rows[0];
    assert_eq!(r.op_kind, OP_KIND_BOOK_TITLE_SET);
    assert_eq!(r.target_kind, "book");
    assert_eq!(r.target_id, book_id);
    assert_eq!(r.progress, "done");

    let pre: Value = serde_json::from_str(&r.pre_state_json).expect("pre json");
    assert_eq!(pre["current"], "Old Title");
    assert_eq!(pre["intent"], "New Title");

    let post: Value = serde_json::from_str(r.post_state_json.as_deref().expect("post present"))
        .expect("post json");
    assert_eq!(post["title"], "New Title");
}

#[tokio::test]
async fn multi_field_patch_with_title_captures_title_journal() {
    // The cycle-33 #219 title-only guard meant a multi-field PATCH
    // including title would skip the journal write entirely. Cycle
    // 34 slot 1 removes that guard: title captures whether or not
    // other fields are present.
    let (router, state, _tmp) = fresh_setup().await;
    let token = mint_token(&state).await;
    let book_id = seed_book(&state.inner.library, "Old Title").await;

    let status = patch_books(
        &router,
        &token,
        book_id,
        r#"{"title":"New Title","subtitle":"a subtitle","language":"en"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let rows = title_journal_rows(&state.inner.library).await;
    assert_eq!(
        rows.len(),
        1,
        "multi-field PATCH still captures title journal"
    );
    let r = &rows[0];
    assert_eq!(r.op_kind, OP_KIND_BOOK_TITLE_SET);
    assert_eq!(r.progress, "done");

    let pre: Value = serde_json::from_str(&r.pre_state_json).expect("pre json");
    assert_eq!(pre["current"], "Old Title");
    assert_eq!(pre["intent"], "New Title");

    let post: Value = serde_json::from_str(r.post_state_json.as_deref().expect("post present"))
        .expect("post json");
    assert_eq!(post["title"], "New Title");

    // The other fields landed too (the tx committed atomically).
    let (subtitle, language): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT subtitle, language FROM books WHERE book_id = ?")
            .bind(book_id)
            .fetch_one(state.inner.library.pool())
            .await
            .expect("read book");
    assert_eq!(subtitle.as_deref(), Some("a subtitle"));
    assert_eq!(language.as_deref(), Some("en"));
}

#[tokio::test]
async fn multi_field_patch_without_title_writes_no_title_journal_row() {
    let (router, state, _tmp) = fresh_setup().await;
    let token = mint_token(&state).await;
    let book_id = seed_book(&state.inner.library, "Existing Title").await;

    let status = patch_books(
        &router,
        &token,
        book_id,
        r#"{"subtitle":"a subtitle","language":"en"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let rows = title_journal_rows(&state.inner.library).await;
    assert!(
        rows.is_empty(),
        "PATCH without title field produces no title-journal row; got {rows:?}",
    );
}
