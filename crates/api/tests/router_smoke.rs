// Integration test target — same lint relaxation as the other
// ab-api integration tests: `expect()` / `unwrap()` are setup
// idioms, `panic!()` is for "unexpected branch", and route
// URIs are deliberately bare in doc-comments.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::doc_markdown
)]

//! Router-level smoke tests for every `Router::route(LIT, …)`
//! call in `ab_api::build_router`.
//!
//! ## Why this file exists
//!
//! Slice #83's `cargo xtask route-tests` lint requires every route
//! declared in a crate be exercised by some test URI in the same
//! crate. Before this file landed, the api crate was the
//! workspace's lone `CRATE_EXEMPTIONS` entry (~30 routes with no
//! harness). This file is the harness; the exemption is dropped
//! in the same commit.
//!
//! ## Scope of what's asserted
//!
//! For protected routes (everything except the
//! [`ab_api::auth::PUBLIC_PATHS`] allow-list — `/health`,
//! `/version`, `/pairing/consume`): a request **without** a Bearer
//! token returns **401 Unauthorized**. The auth middleware fires
//! before any handler runs, so the assertion proves three things at
//! once:
//!
//! 1. The route literal is well-formed (axum's matchit accepted it).
//! 2. The middleware wrapping reached the route.
//! 3. The middleware rejects unauthenticated requests with the
//!    documented status code (consumed by the ABS client + the
//!    `aborg-tools` retry harness).
//!
//! Handler-body correctness is **not** what these tests cover —
//! that belongs in dedicated tests per handler (e.g.
//! `audiologo_apply.rs`).
//!
//! For public routes: we assert the response is not 401 (proving
//! the bypass works). Specific shapes vary — `/health` and
//! `/version` return 200; `/pairing/consume` with an empty body
//! returns a 4xx the Json extractor produced, which is still proof
//! the middleware didn't intercept.
//!
//! ## State construction
//!
//! `fresh_router()` builds a **real** `ApiState` with an empty
//! `Dag`, an empty `CleanupRegistry`, and a fresh
//! `CancellationToken`. The `Scheduler::spawn` worker idles
//! forever on its select loop; each test cancels at the end so the
//! worker exits cleanly. The cost is one task per test (cheap on
//! tokio's multi-thread runtime). No production handler code runs
//! since the middleware short-circuits.

use std::sync::Arc;

use ab_api::ApiState;
use ab_api::build_router;
use ab_core::tunables::{DbTunables, SchedulerTunables, SecurityTunables};
use ab_db::{EphemeralDb, LibraryDb};
use ab_pipeline::cleanup::CleanupRegistry;
use ab_pipeline::{Dag, Scheduler, StageContext};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;

/// Build an isolated router + the cancel token its scheduler worker
/// is wired to. Caller is expected to call `cancel.cancel()` at end
/// of test so the worker exits.
async fn fresh_router() -> (Router, CancellationToken, TempDir) {
    let tmp = TempDir::new().expect("tempdir");
    let library = LibraryDb::open(&tmp.path().join("library.db"), &DbTunables::default())
        .await
        .expect("open library");
    let ephemeral = EphemeralDb::open(&tmp.path().join("ephemeral.db"), &DbTunables::default())
        .await
        .expect("open ephemeral");

    // Empty DAG — the smoke tests never reach handler bodies, so
    // no stages are needed. The Scheduler still spawns its worker
    // task; the worker idles on select! until cancel fires.
    let dag = Arc::new(Dag::build(vec![]).expect("empty dag is valid"));
    let cancel = CancellationToken::new();
    let ctx = StageContext {
        library: library.clone(),
        ephemeral: ephemeral.clone(),
        cancel: cancel.clone(),
        stage_name: "router-smoke-test",
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
        CleanupRegistry::new(vec![]),
        cancel.clone(),
        SecurityTunables::default(),
    );
    let router = build_router(state);
    (router, cancel, tmp)
}

/// Fire one no-body request at the router and return the status.
async fn request_status(router: Router, method: &str, uri: &str) -> StatusCode {
    let req = Request::builder()
        .uri(uri)
        .method(method)
        .body(Body::empty())
        .expect("request builder");
    router.oneshot(req).await.expect("oneshot").status()
}

/// Helper: assert protected routes return 401 without a Bearer token.
async fn assert_protected(method: &str, uri: &str) {
    let (router, cancel, _tmp) = fresh_router().await;
    let status = request_status(router, method, uri).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "expected 401 for unauthenticated {method} {uri}, got {status}"
    );
    cancel.cancel();
}

// ── Public routes ──────────────────────────────────────────────

#[tokio::test]
async fn health_returns_200() {
    let (router, cancel, _tmp) = fresh_router().await;
    let status = request_status(router, "GET", "/health").await;
    assert_eq!(status, StatusCode::OK);
    cancel.cancel();
}

#[tokio::test]
async fn version_returns_200() {
    let (router, cancel, _tmp) = fresh_router().await;
    let status = request_status(router, "GET", "/version").await;
    assert_eq!(status, StatusCode::OK);
    cancel.cancel();
}

#[tokio::test]
async fn pairing_consume_is_public_not_401() {
    // POST without a body → the Json<…> extractor will reject with
    // 4xx (typically 400 Bad Request or 415 Unsupported Media Type
    // depending on how axum routes the missing Content-Type). All
    // we assert is "not 401" — which proves the auth middleware
    // didn't intercept (it's on the PUBLIC_PATHS allow-list).
    let (router, cancel, _tmp) = fresh_router().await;
    let status = request_status(router, "POST", "/pairing/consume").await;
    assert_ne!(
        status,
        StatusCode::UNAUTHORIZED,
        "/pairing/consume should bypass auth; got 401"
    );
    cancel.cancel();
}

// ── Protected routes — auth middleware fires before handler ──
//
// One test per `.route(LIT, …)` declaration. Status code is
// always 401; the diversity is in the route literal, which is
// what the lint cares about.

#[tokio::test]
async fn library_scan_protected() {
    assert_protected("POST", "/library/scan").await;
}

#[tokio::test]
async fn library_duplicates_protected() {
    assert_protected("GET", "/library/duplicates").await;
}

#[tokio::test]
async fn library_roots_protected() {
    assert_protected("GET", "/library_roots").await;
}

#[tokio::test]
async fn library_roots_delete_protected() {
    assert_protected("DELETE", "/library_roots/1").await;
}

#[tokio::test]
async fn tokens_protected() {
    assert_protected("GET", "/tokens").await;
}

#[tokio::test]
async fn tokens_delete_protected() {
    assert_protected("DELETE", "/tokens/1").await;
}

#[tokio::test]
async fn pairing_codes_protected() {
    assert_protected("GET", "/pairing/codes").await;
}

#[tokio::test]
async fn pairing_codes_delete_protected() {
    assert_protected("DELETE", "/pairing/codes/1").await;
}

#[tokio::test]
async fn library_pending_speech_installs_protected() {
    assert_protected("GET", "/library/pending_speech_installs").await;
}

#[tokio::test]
async fn library_pending_speech_installs_retry_protected() {
    assert_protected("POST", "/library/pending_speech_installs/retry").await;
}

#[tokio::test]
async fn doctor_speech_protected() {
    assert_protected("GET", "/doctor/speech").await;
}

#[tokio::test]
async fn doctor_speech_install_protected() {
    assert_protected("POST", "/doctor/speech/install").await;
}

#[tokio::test]
async fn books_protected() {
    assert_protected("GET", "/books").await;
}

#[tokio::test]
async fn books_get_protected() {
    assert_protected("GET", "/books/1").await;
}

#[tokio::test]
async fn books_patch_protected() {
    assert_protected("PATCH", "/books/1").await;
}

#[tokio::test]
async fn books_delete_protected() {
    assert_protected("DELETE", "/books/1").await;
}

#[tokio::test]
async fn books_retry_protected() {
    assert_protected("POST", "/books/1/retry").await;
}

#[tokio::test]
async fn books_restore_protected() {
    assert_protected("POST", "/books/1/restore").await;
}

#[tokio::test]
async fn books_audiologo_protected() {
    assert_protected("POST", "/books/1/audiologo").await;
}

#[tokio::test]
async fn audiologos_review_protected() {
    assert_protected("GET", "/audiologos/review").await;
}

#[tokio::test]
async fn audiologos_approve_protected() {
    assert_protected("POST", "/audiologos/1/approve").await;
}

#[tokio::test]
async fn audiologos_reject_protected() {
    assert_protected("POST", "/audiologos/1/reject").await;
}

#[tokio::test]
async fn clean_usage_protected() {
    assert_protected("GET", "/clean/usage").await;
}

#[tokio::test]
async fn clean_run_protected() {
    assert_protected("POST", "/clean/run").await;
}

#[tokio::test]
async fn names_alias_protected() {
    assert_protected("POST", "/names/author/1/alias").await;
}

#[tokio::test]
async fn names_exalt_protected() {
    assert_protected("POST", "/names/author/1/exalt").await;
}

#[tokio::test]
async fn names_pending_protected() {
    assert_protected("GET", "/names/pending").await;
}

#[tokio::test]
async fn names_pending_resolve_protected() {
    assert_protected("POST", "/names/pending/1/resolve").await;
}

#[tokio::test]
async fn report_gaps_protected() {
    assert_protected("GET", "/report/gaps").await;
}

#[tokio::test]
async fn upcoming_protected() {
    assert_protected("GET", "/upcoming").await;
}
