// Integration test target — same lint relaxation as other
// ab-api integration tests.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::doc_markdown
)]

//! Integration tests for the soft-delete behaviour of the
//! books endpoints (slice #102 — `books.deleted_at` migration
//! 024).
//!
//! Covers:
//! - **DELETE without `?force=true`** → soft-delete (sets
//!   `deleted_at`, row remains).
//! - **DELETE with `?force=true`** → hard-delete (CASCADE).
//! - **Soft-delete is idempotent** — a second soft-delete on
//!   the same book preserves the original timestamp.
//! - **GET /books filters soft-deleted by default**, opts in
//!   via `?include_deleted=true`.
//! - **GET /books/{id} returns soft-deleted books** with
//!   `deleted_at` populated (so callers can render a "restore"
//!   UI).
//!
//! Dispatcher-level filtering (`sweep_eligible` skipping
//! soft-deleted books) is tested separately in
//! `crates/pipeline/`'s own test suite (or implicitly via the
//! integration-test harness once the pipeline is exercised
//! end-to-end).

use std::sync::Arc;

use ab_api::ApiState;
use ab_api::build_router;
use ab_core::tunables::{DbTunables, SchedulerTunables, SecurityTunables};
use ab_db::{EphemeralDb, LibraryDb};
use ab_pipeline::cleanup::CleanupRegistry;
use ab_pipeline::{Dag, Scheduler, StageContext};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

/// Helper: build a router + the cancel token its scheduler
/// worker is wired to. Same shape as the router-smoke harness;
/// duplicated here rather than shared because the test crates
/// don't have a shared fixtures module.
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
        stage_name: "books-soft-delete-test",
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

/// Insert a minimal book + one file. Returns the book_id.
async fn fixture_book(lib: &LibraryDb, title: &str) -> i64 {
    let book_id: i64 = sqlx::query_scalar(
        "INSERT INTO books (title, duration_ms, raw_duration_ms) \
         VALUES (?, 60000, 60000) RETURNING book_id",
    )
    .bind(title)
    .fetch_one(lib.pool())
    .await
    .expect("insert book");
    let _: i64 = sqlx::query_scalar(
        "INSERT INTO book_files (book_id, file_path, duration_ms) \
         VALUES (?, ?, 60000) RETURNING file_id",
    )
    .bind(book_id)
    .bind(format!("/test/{title}/0.m4b"))
    .fetch_one(lib.pool())
    .await
    .expect("insert book_file");
    book_id
}

/// Read the `deleted_at` column directly — bypasses any handler
/// filter that would hide soft-deleted rows.
async fn read_deleted_at(lib: &LibraryDb, book_id: i64) -> Option<i64> {
    sqlx::query_scalar::<_, Option<i64>>("SELECT deleted_at FROM books WHERE book_id = ?")
        .bind(book_id)
        .fetch_one(lib.pool())
        .await
        .expect("select deleted_at")
}

/// Read `EXISTS` for the row (`true` if the row is in the
/// table, regardless of `deleted_at` state).
async fn book_row_exists(lib: &LibraryDb, book_id: i64) -> bool {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM books WHERE book_id = ?")
        .bind(book_id)
        .fetch_one(lib.pool())
        .await
        .expect("count books");
    n > 0
}

/// Invoke a route directly with a bearer token. Issues a token
/// row first, then sends the request with the matching
/// `Authorization` header. Returns the response.
async fn auth_request(
    router: &axum::Router,
    state: &ApiState,
    method: &str,
    uri: &str,
) -> axum::http::Response<axum::body::Body> {
    use ab_core::auth::{hash_api_token, mint_api_token};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt as _;

    // Issue a token row so the auth middleware lets us in.
    let raw = mint_api_token();
    let hash = hash_api_token(&raw);
    // user_id=1 is seeded by migration 001 ("default" user).
    sqlx::query(
        "INSERT INTO tokens (user_id, token_hash, nickname, scopes) VALUES (1, ?, ?, '[]')",
    )
    .bind(&hash)
    .bind("test-token")
    .execute(state.inner.library.pool())
    .await
    .expect("insert token");

    let req = Request::builder()
        .uri(uri)
        .method(method)
        .header("Authorization", format!("Bearer {raw}"))
        .body(Body::empty())
        .expect("request builder");
    router.clone().oneshot(req).await.expect("oneshot")
}

#[tokio::test]
async fn delete_without_force_soft_deletes() {
    let (router, state, cancel, _tmp) = fresh_setup().await;
    let book_id = fixture_book(&state.inner.library, "soft-test").await;
    assert_eq!(
        read_deleted_at(&state.inner.library, book_id).await,
        None,
        "fresh book: deleted_at should be NULL"
    );

    let resp = auth_request(&router, &state, "DELETE", &format!("/books/{book_id}")).await;
    assert_eq!(
        resp.status(),
        axum::http::StatusCode::NO_CONTENT,
        "soft-delete should 204"
    );

    let deleted_at = read_deleted_at(&state.inner.library, book_id).await;
    assert!(
        deleted_at.is_some(),
        "soft-delete should populate deleted_at, got NULL"
    );
    assert!(
        book_row_exists(&state.inner.library, book_id).await,
        "soft-delete must NOT remove the row"
    );
    cancel.cancel();
}

#[tokio::test]
async fn delete_with_force_hard_deletes() {
    let (router, state, cancel, _tmp) = fresh_setup().await;
    let book_id = fixture_book(&state.inner.library, "hard-test").await;

    let resp = auth_request(
        &router,
        &state,
        "DELETE",
        &format!("/books/{book_id}?force=true"),
    )
    .await;
    assert_eq!(
        resp.status(),
        axum::http::StatusCode::NO_CONTENT,
        "hard-delete should 204"
    );

    assert!(
        !book_row_exists(&state.inner.library, book_id).await,
        "hard-delete must remove the row via CASCADE"
    );
    cancel.cancel();
}

#[tokio::test]
async fn soft_delete_is_idempotent_and_preserves_timestamp() {
    let (router, state, cancel, _tmp) = fresh_setup().await;
    let book_id = fixture_book(&state.inner.library, "idempotent-test").await;

    // First soft-delete.
    let resp1 = auth_request(&router, &state, "DELETE", &format!("/books/{book_id}")).await;
    assert_eq!(resp1.status(), axum::http::StatusCode::NO_CONTENT);
    let first_ts = read_deleted_at(&state.inner.library, book_id)
        .await
        .expect("deleted_at set after first soft-delete");

    // Sleep just enough that a re-write with a fresh unix-now
    // would have a different value (1s resolution + jitter).
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    // Second soft-delete on the SAME book.
    let resp2 = auth_request(&router, &state, "DELETE", &format!("/books/{book_id}")).await;
    assert_eq!(
        resp2.status(),
        axum::http::StatusCode::NO_CONTENT,
        "second soft-delete should still 204"
    );
    let second_ts = read_deleted_at(&state.inner.library, book_id)
        .await
        .expect("deleted_at still set after second soft-delete");

    assert_eq!(
        first_ts, second_ts,
        "second soft-delete must NOT overwrite the original timestamp"
    );
    cancel.cancel();
}

#[tokio::test]
async fn list_hides_soft_deleted_by_default() {
    use axum::body::to_bytes;
    let (router, state, cancel, _tmp) = fresh_setup().await;
    let active = fixture_book(&state.inner.library, "active-book").await;
    let soft = fixture_book(&state.inner.library, "soft-book").await;

    // Soft-delete one of them.
    let _ = auth_request(&router, &state, "DELETE", &format!("/books/{soft}")).await;

    // Default list: should NOT include the soft-deleted book.
    let resp = auth_request(&router, &state, "GET", "/books").await;
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let body = to_bytes(resp.into_body(), 1 << 20).await.expect("body");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("parse json");
    let ids: Vec<i64> = json["books"]
        .as_array()
        .expect("books array")
        .iter()
        .map(|b| b["book_id"].as_i64().expect("book_id is i64"))
        .collect();
    assert!(
        ids.contains(&active),
        "active book {active} should be in the default list, got {ids:?}"
    );
    assert!(
        !ids.contains(&soft),
        "soft-deleted book {soft} should be hidden by default, got {ids:?}"
    );
    // total mirrors the soft-delete predicate — exactly one
    // active book seeded, the soft one is excluded.
    let total = json["total"].as_i64().expect("total is i64");
    assert_eq!(
        total, 1,
        "default list total should exclude soft-deleted book; got {total}",
    );
    cancel.cancel();
}

#[tokio::test]
async fn list_includes_soft_deleted_with_query_param() {
    use axum::body::to_bytes;
    let (router, state, cancel, _tmp) = fresh_setup().await;
    let active = fixture_book(&state.inner.library, "active-book-2").await;
    let soft = fixture_book(&state.inner.library, "soft-book-2").await;

    let _ = auth_request(&router, &state, "DELETE", &format!("/books/{soft}")).await;

    let resp = auth_request(&router, &state, "GET", "/books?include_deleted=true").await;
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let body = to_bytes(resp.into_body(), 1 << 20).await.expect("body");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("parse json");
    let ids: Vec<i64> = json["books"]
        .as_array()
        .expect("books array")
        .iter()
        .map(|b| b["book_id"].as_i64().expect("book_id is i64"))
        .collect();
    assert!(
        ids.contains(&active),
        "active book must still appear with include_deleted=true"
    );
    assert!(
        ids.contains(&soft),
        "soft-deleted book must appear with include_deleted=true"
    );
    let total = json["total"].as_i64().expect("total is i64");
    assert_eq!(
        total, 2,
        "include_deleted=true total should count active + soft; got {total}",
    );
    cancel.cancel();
}

#[tokio::test]
async fn get_detail_returns_soft_deleted_with_timestamp() {
    use axum::body::to_bytes;
    let (router, state, cancel, _tmp) = fresh_setup().await;
    let book_id = fixture_book(&state.inner.library, "detail-test").await;

    let _ = auth_request(&router, &state, "DELETE", &format!("/books/{book_id}")).await;

    // GET /books/{id} returns the row even when soft-deleted —
    // the caller checks the `deleted_at` field to know.
    let resp = auth_request(&router, &state, "GET", &format!("/books/{book_id}")).await;
    assert_eq!(
        resp.status(),
        axum::http::StatusCode::OK,
        "GET /books/{{id}} on soft-deleted should still 200 (with deleted_at populated)"
    );
    let body = to_bytes(resp.into_body(), 1 << 20).await.expect("body");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("parse json");
    assert!(
        json["deleted_at"].is_i64(),
        "response must include deleted_at as a unix-seconds integer, got {:?}",
        json["deleted_at"]
    );
    cancel.cancel();
}

// ── Restore (slice #103) ─────────────────────────────────────────

#[tokio::test]
async fn restore_flips_deleted_at_back_to_null() {
    use axum::body::to_bytes;
    let (router, state, cancel, _tmp) = fresh_setup().await;
    let book_id = fixture_book(&state.inner.library, "restore-test").await;

    // Soft-delete first.
    let _ = auth_request(&router, &state, "DELETE", &format!("/books/{book_id}")).await;
    assert!(
        read_deleted_at(&state.inner.library, book_id)
            .await
            .is_some(),
        "precondition: book should be soft-deleted"
    );

    // Restore.
    let resp = auth_request(
        &router,
        &state,
        "POST",
        &format!("/books/{book_id}/restore"),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let body = to_bytes(resp.into_body(), 1 << 20).await.expect("body");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("parse json");
    assert_eq!(
        json["book_id"].as_i64(),
        Some(book_id),
        "response book_id mismatch"
    );
    assert_eq!(
        json["restored"].as_bool(),
        Some(true),
        "restoring a soft-deleted book should report restored=true"
    );

    assert_eq!(
        read_deleted_at(&state.inner.library, book_id).await,
        None,
        "deleted_at should be NULL after restore"
    );
    cancel.cancel();
}

#[tokio::test]
async fn restore_on_active_book_is_noop() {
    use axum::body::to_bytes;
    let (router, state, cancel, _tmp) = fresh_setup().await;
    let book_id = fixture_book(&state.inner.library, "restore-noop-test").await;

    // Don't soft-delete first — book is already active.
    let resp = auth_request(
        &router,
        &state,
        "POST",
        &format!("/books/{book_id}/restore"),
    )
    .await;
    assert_eq!(
        resp.status(),
        axum::http::StatusCode::OK,
        "restoring an already-active book should still 200 (idempotent)"
    );
    let body = to_bytes(resp.into_body(), 1 << 20).await.expect("body");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("parse json");
    assert_eq!(
        json["restored"].as_bool(),
        Some(false),
        "active-book restore should report restored=false (no-op)"
    );
    cancel.cancel();
}

#[tokio::test]
async fn restore_returns_404_for_missing_book() {
    let (router, state, cancel, _tmp) = fresh_setup().await;
    let resp = auth_request(&router, &state, "POST", "/books/99999/restore").await;
    assert_eq!(
        resp.status(),
        axum::http::StatusCode::NOT_FOUND,
        "restore on nonexistent book should 404"
    );
    cancel.cancel();
}

#[tokio::test]
async fn restore_re_includes_book_in_default_list() {
    use axum::body::to_bytes;
    let (router, state, cancel, _tmp) = fresh_setup().await;
    let book_id = fixture_book(&state.inner.library, "restore-list-test").await;

    // Soft-delete → hidden from default list.
    let _ = auth_request(&router, &state, "DELETE", &format!("/books/{book_id}")).await;
    let resp = auth_request(&router, &state, "GET", "/books").await;
    let body = to_bytes(resp.into_body(), 1 << 20).await.expect("body");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("parse json");
    let ids: Vec<i64> = json["books"]
        .as_array()
        .expect("books array")
        .iter()
        .map(|b| b["book_id"].as_i64().expect("book_id is i64"))
        .collect();
    assert!(
        !ids.contains(&book_id),
        "precondition: soft-deleted book should be hidden, got {ids:?}"
    );

    // Restore → re-included in default list.
    let _ = auth_request(
        &router,
        &state,
        "POST",
        &format!("/books/{book_id}/restore"),
    )
    .await;
    let resp = auth_request(&router, &state, "GET", "/books").await;
    let body = to_bytes(resp.into_body(), 1 << 20).await.expect("body");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("parse json");
    let ids: Vec<i64> = json["books"]
        .as_array()
        .expect("books array")
        .iter()
        .map(|b| b["book_id"].as_i64().expect("book_id is i64"))
        .collect();
    assert!(
        ids.contains(&book_id),
        "restored book should re-appear in default list, got {ids:?}"
    );
    cancel.cancel();
}
