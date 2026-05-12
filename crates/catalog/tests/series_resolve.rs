// Integration tests for the series resolution path landed in
// slice C5.6 (`identity-resolve` reads `book_series_candidate`
// rows and writes `series` + `book_series` junction).
//
// `expect()` / `unwrap()` on setup is the standard integration-test
// idiom — a panic on tempdir / DB-open is the intended outcome.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    // Test-doc shorthand: column / table names without backticks
    // (same convention as promote_drift.rs).
    clippy::doc_markdown,
)]

//! Coverage:
//! - Audnexus-only seed → series row + book_series row with
//!   ASIN + position + is_primary=1.
//! - Tag-only seed → series row by name (no ASIN, no position),
//!   book_series row with is_primary=1.
//! - Audnexus + tag for the same series → single series row,
//!   audible_id from Audnexus, position from Audnexus.
//! - Audnexus primary + secondary → two book_series rows with
//!   is_primary distinguishing them.
//! - Re-run is idempotent (book_series count stays at N).
//! - ASIN back-fill on existing name-matched series row.

use ab_catalog::identity::IdentityResolveStage;
use ab_core::BookId;
use ab_core::tunables::DbTunables;
use ab_db::{EphemeralDb, LibraryDb};
use ab_pipeline::{Stage, StageContext};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

async fn fresh_ctx(dir: &std::path::Path) -> (StageContext, LibraryDb) {
    let tunables = DbTunables::default();
    let lib = LibraryDb::open(&dir.join("library.db"), &tunables)
        .await
        .expect("open library");
    let eph = EphemeralDb::open(&dir.join("ephemeral.db"), &tunables)
        .await
        .expect("open ephemeral");
    let ctx = StageContext {
        library: lib.clone(),
        ephemeral: eph,
        cancel: CancellationToken::new(),
        stage_name: "identity-resolve",
    };
    (ctx, lib)
}

async fn seed_book(library: &LibraryDb, book_id: i64) {
    sqlx::query("INSERT INTO books (book_id, title) VALUES (?, 'placeholder')")
        .bind(book_id)
        .execute(library.pool())
        .await
        .expect("seed book");
}

async fn insert_candidate(
    library: &LibraryDb,
    book_id: i64,
    source: &str,
    series_name: &str,
    series_asin: Option<&str>,
    position: Option<f64>,
    is_primary: i64,
    confidence: f64,
) {
    sqlx::query(
        "INSERT INTO book_series_candidate \
         (book_id, source, series_name, series_asin, position, is_primary, confidence) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(book_id)
    .bind(source)
    .bind(series_name)
    .bind(series_asin)
    .bind(position)
    .bind(is_primary)
    .bind(confidence)
    .execute(library.pool())
    .await
    .expect("seed candidate");
}

#[tokio::test]
async fn audnexus_only_seeds_series_and_book_series() {
    let tmp = TempDir::new().expect("tmpdir");
    let (ctx, library) = fresh_ctx(tmp.path()).await;
    seed_book(&library, 1).await;
    insert_candidate(
        &library,
        1,
        "audnexus_asin_us",
        "Mistborn",
        Some("B0SERIES1"),
        Some(1.0),
        1,
        0.95,
    )
    .await;

    let stage = IdentityResolveStage::new();
    stage.run(&ctx, BookId(1)).await.expect("identity run");

    let (series_id, name, audible_id): (i64, String, Option<String>) =
        sqlx::query_as("SELECT series_id, name, audible_id FROM series WHERE name = 'Mistborn'")
            .fetch_one(library.pool())
            .await
            .expect("read series");
    assert_eq!(name, "Mistborn");
    assert_eq!(audible_id.as_deref(), Some("B0SERIES1"));

    let (position, is_primary): (Option<f64>, i64) = sqlx::query_as(
        "SELECT position, is_primary FROM book_series WHERE book_id = 1 AND series_id = ?",
    )
    .bind(series_id)
    .fetch_one(library.pool())
    .await
    .expect("read book_series");
    assert_eq!(position, Some(1.0));
    assert_eq!(is_primary, 1);
}

#[tokio::test]
async fn tag_only_seeds_by_name() {
    let tmp = TempDir::new().expect("tmpdir");
    let (ctx, library) = fresh_ctx(tmp.path()).await;
    seed_book(&library, 1).await;
    insert_candidate(&library, 1, "tag_file", "Wheel of Time", None, None, 1, 0.7).await;

    let stage = IdentityResolveStage::new();
    stage.run(&ctx, BookId(1)).await.expect("identity run");

    let audible_id: Option<String> =
        sqlx::query_scalar("SELECT audible_id FROM series WHERE name = 'Wheel of Time'")
            .fetch_one(library.pool())
            .await
            .expect("read series");
    assert!(
        audible_id.is_none(),
        "tag-only seed must not set audible_id"
    );

    let (position, is_primary): (Option<f64>, i64) = sqlx::query_as(
        "SELECT position, is_primary FROM book_series \
         WHERE book_id = 1 AND series_id = (SELECT series_id FROM series WHERE name = 'Wheel of Time')",
    )
    .fetch_one(library.pool())
    .await
    .expect("read book_series");
    assert!(position.is_none(), "tag-only must leave position NULL");
    assert_eq!(is_primary, 1);
}

#[tokio::test]
async fn audnexus_and_tag_for_same_series_merge_into_one_row() {
    let tmp = TempDir::new().expect("tmpdir");
    let (ctx, library) = fresh_ctx(tmp.path()).await;
    seed_book(&library, 1).await;
    // Tag fires first (lower confidence, no ASIN, no position).
    insert_candidate(
        &library,
        1,
        "tag_file",
        "Stormlight Archive",
        None,
        None,
        1,
        0.7,
    )
    .await;
    // Audnexus fires after (higher confidence, with ASIN + position).
    insert_candidate(
        &library,
        1,
        "audnexus_asin_us",
        "Stormlight Archive",
        Some("B0SERIES2"),
        Some(2.0),
        1,
        0.95,
    )
    .await;

    let stage = IdentityResolveStage::new();
    stage.run(&ctx, BookId(1)).await.expect("identity run");

    // Exactly one `series` row exists for this name.
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM series WHERE lower(name) = lower('Stormlight Archive')",
    )
    .fetch_one(library.pool())
    .await
    .expect("count series");
    assert_eq!(count, 1, "merge into one series row");

    let (audible_id, position): (Option<String>, Option<f64>) = sqlx::query_as(
        "SELECT s.audible_id, bs.position FROM series s \
         JOIN book_series bs ON bs.series_id = s.series_id \
         WHERE s.name = 'Stormlight Archive' AND bs.book_id = 1",
    )
    .fetch_one(library.pool())
    .await
    .expect("read merged row");
    assert_eq!(
        audible_id.as_deref(),
        Some("B0SERIES2"),
        "ASIN from Audnexus"
    );
    assert_eq!(position, Some(2.0), "position from Audnexus");
}

#[tokio::test]
async fn primary_and_secondary_series_both_recorded() {
    let tmp = TempDir::new().expect("tmpdir");
    let (ctx, library) = fresh_ctx(tmp.path()).await;
    seed_book(&library, 1).await;
    insert_candidate(
        &library,
        1,
        "audnexus_asin_us",
        "Mistborn",
        Some("B0SERIES3"),
        Some(1.0),
        1,
        0.95,
    )
    .await;
    insert_candidate(
        &library,
        1,
        "audnexus_asin_us",
        "Cosmere",
        Some("B0COSMERE"),
        None,
        0,
        0.95,
    )
    .await;

    let stage = IdentityResolveStage::new();
    stage.run(&ctx, BookId(1)).await.expect("identity run");

    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT s.name, bs.is_primary FROM series s \
         JOIN book_series bs ON bs.series_id = s.series_id \
         WHERE bs.book_id = 1 ORDER BY bs.is_primary DESC",
    )
    .fetch_all(library.pool())
    .await
    .expect("read both rows");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], ("Mistborn".to_owned(), 1));
    assert_eq!(rows[1], ("Cosmere".to_owned(), 0));
}

#[tokio::test]
async fn rerun_is_idempotent() {
    let tmp = TempDir::new().expect("tmpdir");
    let (ctx, library) = fresh_ctx(tmp.path()).await;
    seed_book(&library, 1).await;
    insert_candidate(
        &library,
        1,
        "audnexus_asin_us",
        "Foundation",
        Some("B0FOUNDATION"),
        Some(3.0),
        1,
        0.95,
    )
    .await;

    let stage = IdentityResolveStage::new();
    stage.run(&ctx, BookId(1)).await.expect("first run");
    stage.run(&ctx, BookId(1)).await.expect("second run");
    stage.run(&ctx, BookId(1)).await.expect("third run");

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM book_series WHERE book_id = 1")
        .fetch_one(library.pool())
        .await
        .expect("count book_series");
    assert_eq!(count, 1, "re-run must not duplicate book_series rows");
}

#[tokio::test]
async fn asin_backfills_name_matched_existing_row() {
    let tmp = TempDir::new().expect("tmpdir");
    let (ctx, library) = fresh_ctx(tmp.path()).await;
    seed_book(&library, 1).await;
    // First: tag-only insert creates the series row WITHOUT audible_id.
    insert_candidate(&library, 1, "tag_file", "Dune", None, None, 1, 0.7).await;
    let stage = IdentityResolveStage::new();
    stage.run(&ctx, BookId(1)).await.expect("first run");

    // Second book in the same series with an ASIN candidate.
    seed_book(&library, 2).await;
    insert_candidate(
        &library,
        2,
        "audnexus_asin_us",
        "Dune",
        Some("B0DUNE"),
        Some(1.0),
        1,
        0.95,
    )
    .await;
    stage.run(&ctx, BookId(2)).await.expect("second run");

    // The pre-existing name-matched row should now carry the ASIN.
    let audible_id: Option<String> =
        sqlx::query_scalar("SELECT audible_id FROM series WHERE name = 'Dune'")
            .fetch_one(library.pool())
            .await
            .expect("read series");
    assert_eq!(
        audible_id.as_deref(),
        Some("B0DUNE"),
        "back-fill on existing row"
    );
}
