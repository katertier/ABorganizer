//! End-to-end tests against the shelf router.
//!
//! Spin up an axum `Router` over an in-memory `tempfile`-backed
//! `LibraryDb`, seed a book + files, and hit each endpoint
//! through `tower::ServiceExt::oneshot`. Mirrors the existing
//! api-crate test patterns.

#![cfg(test)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tempfile::TempDir;
use tower::ServiceExt as _;

use ab_core::tunables::DbTunables;
use ab_db::LibraryDb;

use crate::{ShelfState, build_router};

async fn fresh_router() -> (Router, LibraryDb, TempDir) {
    let tmp = TempDir::new().expect("tmpdir");
    let library = LibraryDb::open(&tmp.path().join("library.db"), &DbTunables::default())
        .await
        .expect("open library");
    let router = build_router(ShelfState::new(library.clone()));
    (router, library, tmp)
}

async fn seed_book(library: &LibraryDb) -> i64 {
    // Single book with a single file. Author + publisher rows
    // exist + are linked so the joined columns aren't NULL.
    sqlx::query("INSERT INTO authors (author_id, name) VALUES (1, 'A. Uthor')")
        .execute(library.pool())
        .await
        .expect("seed author");
    sqlx::query("INSERT INTO publishers (publisher_id, name) VALUES (1, 'Tor Books')")
        .execute(library.pool())
        .await
        .expect("seed publisher");
    sqlx::query(
        "INSERT INTO books \
         (book_id, title, subtitle, author_id, publisher_id, description, \
          language, duration_ms, asin, release_date) \
         VALUES (1, 'Test Title', 'The MVP', 1, 1, 'Some description.', \
                 'en', 3_600_000, 'B0TESTASIN', '2022-04-15')",
    )
    .execute(library.pool())
    .await
    .expect("seed book");
    1
}

async fn seed_file(library: &LibraryDb, book_id: i64, path: &str) -> i64 {
    sqlx::query(
        "INSERT INTO book_files \
         (book_id, file_path, file_size, format, duration_ms, is_active) \
         VALUES (?, ?, 12345, 'm4b', 3_600_000, 1) \
         RETURNING file_id",
    )
    .bind(book_id)
    .bind(path)
    .execute(library.pool())
    .await
    .expect("seed file");
    sqlx::query_scalar("SELECT file_id FROM book_files WHERE file_path = ?")
        .bind(path)
        .fetch_one(library.pool())
        .await
        .expect("fetch file_id")
}

async fn get(router: &Router, path: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .uri(path)
        .method("GET")
        .body(Body::empty())
        .expect("build req");
    let resp = router.clone().oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let body = to_bytes(resp.into_body(), 1_024 * 1_024)
        .await
        .expect("read body");
    let json: Value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).to_string()))
    };
    (status, json)
}

#[tokio::test]
async fn healthcheck_returns_ok_text() {
    let (router, _lib, _tmp) = fresh_router().await;
    let req = Request::builder()
        .uri("/healthcheck")
        .method("GET")
        .body(Body::empty())
        .expect("req");
    let resp = router.oneshot(req).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), 1024).await.expect("body");
    assert_eq!(body, "OK");
}

#[tokio::test]
async fn info_returns_abs_metadata() {
    let (router, _lib, _tmp) = fresh_router().await;
    let (status, body) = get(&router, "/api/info").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["api_version"], "2");
    assert!(body["server_version"].is_string());
}

#[tokio::test]
async fn libraries_returns_single_default() {
    let (router, _lib, _tmp) = fresh_router().await;
    let (status, body) = get(&router, "/api/libraries").await;
    assert_eq!(status, StatusCode::OK);
    let libs = body["libraries"].as_array().expect("libraries array");
    assert_eq!(libs.len(), 1);
    assert_eq!(libs[0]["id"], "aborg-default");
    assert_eq!(libs[0]["mediaType"], "book");
}

#[tokio::test]
async fn item_returns_book_with_files() {
    let (router, library, _tmp) = fresh_router().await;
    let book_id = seed_book(&library).await;
    let _file_id = seed_file(&library, book_id, "/tmp/x.m4b").await;

    let (status, body) = get(&router, &format!("/api/items/{book_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], book_id.to_string());
    assert_eq!(body["libraryId"], "aborg-default");
    assert_eq!(body["mediaType"], "book");
    assert_eq!(body["media"]["metadata"]["title"], "Test Title");
    assert_eq!(body["media"]["metadata"]["subtitle"], "The MVP");
    assert_eq!(body["media"]["metadata"]["authorName"], "A. Uthor");
    assert_eq!(body["media"]["metadata"]["publisher"], "Tor Books");
    assert_eq!(body["media"]["metadata"]["language"], "en");
    assert_eq!(body["media"]["metadata"]["asin"], "B0TESTASIN");
    assert_eq!(body["media"]["metadata"]["publishedYear"], "2022");
    // 3_600_000 ms → 3600.0 s
    assert!((body["media"]["duration"].as_f64().unwrap() - 3600.0).abs() < 1e-9);
    let files = body["media"]["audioFiles"].as_array().expect("audioFiles");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["mimeType"], "audio/mp4");
    assert_eq!(files[0]["metadata"]["filename"], "x.m4b");
}

#[tokio::test]
async fn item_missing_id_is_not_found() {
    let (router, _lib, _tmp) = fresh_router().await;
    let (status, _body) = get(&router, "/api/items/99999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn item_non_numeric_id_is_bad_request() {
    let (router, _lib, _tmp) = fresh_router().await;
    let (status, _body) = get(&router, "/api/items/notanumber").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn item_omits_optional_fields_when_null() {
    let (router, library, _tmp) = fresh_router().await;
    // Book with only the required `title` set.
    sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'Minimal')")
        .execute(library.pool())
        .await
        .expect("seed");
    let (status, body) = get(&router, "/api/items/1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["media"]["metadata"]["title"], "Minimal");
    // skip_serializing_if = "Option::is_none" → keys absent
    assert!(body["media"]["metadata"].get("subtitle").is_none());
    assert!(body["media"]["metadata"].get("authorName").is_none());
    assert!(body["media"]["metadata"].get("publisher").is_none());
    // duration = 0.0 when null
    assert!((body["media"]["duration"].as_f64().unwrap() - 0.0).abs() < 1e-9);
}

#[tokio::test]
async fn stream_file_returns_content_with_correct_mime() {
    let (router, library, tmp) = fresh_router().await;
    let book_id = seed_book(&library).await;
    let path = tmp.path().join("audio.m4b");
    tokio::fs::write(&path, b"fake-m4b-bytes")
        .await
        .expect("write");
    let file_id = seed_file(&library, book_id, path.to_str().expect("utf8")).await;

    let req = Request::builder()
        .uri(format!("/api/items/{book_id}/file/{file_id}"))
        .method("GET")
        .body(Body::empty())
        .expect("req");
    let resp = router.oneshot(req).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .expect("Content-Type")
        .to_str()
        .expect("utf8");
    assert_eq!(ct, "audio/mp4");
    assert!(
        resp.headers()
            .get(axum::http::header::ACCEPT_RANGES)
            .is_some()
    );
    let body = to_bytes(resp.into_body(), 1024).await.expect("body");
    assert_eq!(body.as_ref(), b"fake-m4b-bytes");
}

#[tokio::test]
async fn stream_file_wrong_book_id_is_not_found() {
    let (router, library, tmp) = fresh_router().await;
    let book_id = seed_book(&library).await;
    let path = tmp.path().join("x.m4b");
    tokio::fs::write(&path, b"x").await.expect("write");
    let file_id = seed_file(&library, book_id, path.to_str().expect("utf8")).await;

    // Right file_id but wrong book_id → 404 (no cross-book read).
    let (status, _body) = get(&router, &format!("/api/items/99999/file/{file_id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn stream_file_unknown_ino_is_not_found() {
    let (router, library, _tmp) = fresh_router().await;
    let book_id = seed_book(&library).await;
    let (status, _body) = get(&router, &format!("/api/items/{book_id}/file/99999")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
