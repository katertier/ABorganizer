// Integration test target — same lint relaxation as other
// ab-api integration tests.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::doc_markdown
)]

//! Integration tests for the ASIN auto-learn capture flow.
//!
//! When the operator sets an ASIN via `PATCH /api/v1/books/{id}`,
//! the catalogue layer records `(title, author, asin)` into
//! `asin_learnings`. A follow-up slice will consume that table
//! from `audible-search` as a lookup hint; this test covers the
//! capture side only.

use std::sync::Arc;

use ab_api::ApiState;
use ab_api::build_router;
use ab_core::auth::{hash_api_token, mint_api_token};
use ab_core::tunables::{DbTunables, SchedulerTunables, SecurityTunables};
use ab_db::{EphemeralDb, LibraryDb};
use ab_pipeline::cleanup::CleanupRegistry;
use ab_pipeline::{Dag, Scheduler, StageContext};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;

async fn fresh_setup() -> (axum::Router, ApiState, CancellationToken, TempDir) {
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
        stage_name: "asin-auto-learn-test",
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
        cancel.clone(),
        SecurityTunables::default(),
        globset::GlobSet::empty(),
        ab_background::BackgroundRegistry::new(vec![]),
        ab_api::doctor::DoctorRegistry::new(vec![]),
    );
    let router = build_router(state.clone());
    (router, state, cancel, tmp)
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

async fn seed_book_with_author(lib: &LibraryDb, title: &str, author: &str) -> i64 {
    let author_id: i64 =
        sqlx::query_scalar("INSERT INTO authors (name) VALUES (?) RETURNING author_id")
            .bind(author)
            .fetch_one(lib.pool())
            .await
            .expect("insert author");
    let book_id: i64 = sqlx::query_scalar(
        "INSERT INTO books (title, author_id, duration_ms, raw_duration_ms) \
         VALUES (?, ?, 60000, 60000) RETURNING book_id",
    )
    .bind(title)
    .bind(author_id)
    .fetch_one(lib.pool())
    .await
    .expect("insert book");
    book_id
}

async fn count_learnings(lib: &LibraryDb, asin: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM asin_learnings WHERE asin = ?")
        .bind(asin)
        .fetch_one(lib.pool())
        .await
        .expect("count asin_learnings")
}

async fn read_learning_norm(lib: &LibraryDb, asin: &str) -> Option<(String, String)> {
    sqlx::query_as::<_, (String, String)>(
        "SELECT title_norm, author_norm FROM asin_learnings WHERE asin = ?",
    )
    .bind(asin)
    .fetch_optional(lib.pool())
    .await
    .expect("read learning")
}

async fn patch_asin(router: &axum::Router, token: &str, book_id: i64, asin: &str) -> StatusCode {
    let body = format!(r#"{{"asin":"{asin}"}}"#);
    let req = Request::builder()
        .method("PATCH")
        .uri(format!("/books/{book_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("request builder");
    router.clone().oneshot(req).await.expect("oneshot").status()
}

#[tokio::test]
async fn patch_books_asin_captures_learning_row() {
    let (router, state, _cancel, _tmp) = fresh_setup().await;
    let token = mint_token(&state).await;
    let book_id =
        seed_book_with_author(&state.inner.library, "Mistborn", "Brandon Sanderson").await;

    let status = patch_asin(&router, &token, book_id, "B002UZJ8TG").await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(count_learnings(&state.inner.library, "B002UZJ8TG").await, 1);
    let learned = read_learning_norm(&state.inner.library, "B002UZJ8TG")
        .await
        .expect("learning row");
    assert_eq!(learned.0, "mistborn");
    assert_eq!(learned.1, "brandon sanderson");
}

#[tokio::test]
async fn repeated_patch_with_same_asin_is_idempotent() {
    let (router, state, _cancel, _tmp) = fresh_setup().await;
    let token = mint_token(&state).await;
    let book_id = seed_book_with_author(
        &state.inner.library,
        "The Way Of Kings",
        "Brandon Sanderson",
    )
    .await;

    assert_eq!(
        patch_asin(&router, &token, book_id, "B003P2WO5E").await,
        StatusCode::OK
    );
    assert_eq!(
        patch_asin(&router, &token, book_id, "B003P2WO5E").await,
        StatusCode::OK
    );

    assert_eq!(count_learnings(&state.inner.library, "B003P2WO5E").await, 1);
}

#[tokio::test]
async fn patch_books_asin_without_author_skips_learning() {
    // Book has no author_id — the capture path silently skips so
    // we don't pollute the lookup index with an empty author_norm
    // key.
    let (router, state, _cancel, _tmp) = fresh_setup().await;
    let token = mint_token(&state).await;
    let book_id: i64 = sqlx::query_scalar(
        "INSERT INTO books (title, duration_ms, raw_duration_ms) \
         VALUES ('Orphan', 60000, 60000) RETURNING book_id",
    )
    .fetch_one(state.inner.library.pool())
    .await
    .expect("insert book");

    assert_eq!(
        patch_asin(&router, &token, book_id, "B0FAKE0001").await,
        StatusCode::OK
    );

    assert_eq!(count_learnings(&state.inner.library, "B0FAKE0001").await, 0);
}
