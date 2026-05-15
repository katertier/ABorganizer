// Integration test target: `expect()` / `unwrap()` are the
// standard test-setup idiom (a panic on setup-failure is the
// expected outcome — the test simply cannot run), `panic!()` is
// the deliberate signal for "unexpected match arm", and the
// `books.X` shorthand in module docs is a column reference, not
// a struct-field reference. The lints are restored to defaults
// inside the production crates.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::doc_markdown
)]

//! Drift-detection integration test for the consensus →
//! `books.*` promotion path.
//!
//! Background (slice B3 / course correction): the project uses
//! a dual-storage pattern for catalog fields — every value
//! lives in `book_field_provenance` (audit trail of candidates)
//! AND in a promoted column on `books` (fast read for the
//! player). The promotion is the consensus stage's job. The
//! invariant we depend on is "books.X equals the value of
//! `book_field_provenance(field=X, is_winner=1)`."
//!
//! Nothing structural enforces that invariant — it's a property
//! of correct stage code. This test seeds a book with multiple
//! provenance candidates per field, runs the consensus stage,
//! and asserts that every promoted column matches its winner.
//! Any stage that breaks the invariant (writes `books.title`
//! without updating provenance, or vice versa) fails this test.
//!
//! Add a new promotable field to the catalog `PROMOTABLE_FIELDS`
//! list: extend the `(provenance_field, target_column)` table
//! below + the seed `candidates` list. The test pattern is the
//! same.

use ab_catalog::consensus::ConsensusStage;
use ab_core::BookId;
use ab_core::tunables::DbTunables;
use ab_db::{EphemeralDb, LibraryDb};
use ab_pipeline::{Stage, StageContext};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

/// Open a fresh library + ephemeral DB pair in a tempdir and
/// build the StageContext consensus expects.
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
        stage_name: "promote-consensus",
    };
    (ctx, lib)
}

#[tokio::test]
async fn books_columns_match_provenance_winners() {
    let tmp = TempDir::new().expect("tmpdir");
    let (ctx, library) = fresh_ctx(tmp.path()).await;

    // Seed a minimal book.
    sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'placeholder')")
        .execute(library.pool())
        .await
        .expect("seed book");

    // Insert a mix of candidates per field: one clear winner +
    // one or more competitors. Confidence values are picked so
    // the winner is unambiguous and easy to read in failure
    // output.
    let candidates: &[(&str, &str, f64, &str)] = &[
        // (provenance field, value, confidence, source)
        ("title", "Real Title", 0.95, "audnexus_asin"),
        ("title", "Wrong Title", 0.50, "tag_meta"),
        (
            "subtitle",
            "A Tale of Drift Detection",
            0.70,
            "audnexus_asin",
        ),
        (
            "description",
            "The book everyone reads.",
            0.90,
            "audnexus_asin",
        ),
        ("description", "Spoiler-laden version.", 0.55, "tag_meta"),
        ("language", "en", 0.85, "tag_meta"),
        ("language", "de", 0.60, "audible_search"),
        ("release_date", "2024-01-01", 0.95, "audnexus_asin"),
    ];
    for (field, value, conf, source) in candidates {
        // Map the test's source string back to a producing
        // stage so the NOT NULL stage column is populated. The
        // real `tag_meta` / `audible_search` / `audnexus_asin`
        // source values come from the corresponding stages.
        let stage = match *source {
            s if s.starts_with("audnexus") => "enrich-from-audnexus",
            "audible_search" => "search-audible",
            _ => "read-tags",
        };
        sqlx::query(
            "INSERT INTO book_field_provenance \
             (book_id, field, value, confidence, source, stage) \
             VALUES (1, ?, ?, ?, ?, ?)",
        )
        .bind(field)
        .bind(value)
        .bind(conf)
        .bind(source)
        .bind(stage)
        .execute(library.pool())
        .await
        .expect("seed provenance");
    }

    // Run consensus.
    let stage = ConsensusStage::new();
    stage.run(&ctx, BookId(1)).await.expect("consensus run");

    // Verify every promoted field: books.<target_column> ==
    // book_field_provenance(field, is_winner=1).value.
    //
    // When a new promotable field lands in catalog::consensus,
    // adding it here keeps the invariant covered. Pin the
    // closed set deliberately — a forgotten entry is a real
    // test bug, not a false negative.
    let fields: &[(&str, &str)] = &[
        ("title", "title"),
        ("subtitle", "subtitle"),
        ("description", "description"),
        ("language", "language"),
        ("release_date", "release_date"),
    ];
    for (provenance_field, target_column) in fields {
        let sql = format!("SELECT {target_column} FROM books WHERE book_id = 1");
        let books_val: Option<String> = sqlx::query_scalar(&sql)
            .fetch_one(library.pool())
            .await
            .unwrap_or_else(|e| panic!("read books.{target_column}: {e}"));

        let winner_val: Option<String> = sqlx::query_scalar(
            "SELECT value FROM book_field_provenance \
             WHERE book_id = 1 AND field = ? AND is_winner = 1 LIMIT 1",
        )
        .bind(provenance_field)
        .fetch_optional(library.pool())
        .await
        .unwrap_or_else(|e| panic!("read provenance.{provenance_field}: {e}"))
        .flatten();

        assert_eq!(
            books_val, winner_val,
            "drift on {provenance_field}: \
             books.{target_column}={books_val:?} \
             but provenance winner={winner_val:?}"
        );
    }
}

#[tokio::test]
async fn no_winner_leaves_books_columns_unchanged() {
    // Defensive: when provenance has zero candidates for a
    // field, consensus shouldn't touch the books column. Books
    // pre-populated with a placeholder must keep the
    // placeholder.
    let tmp = TempDir::new().expect("tmpdir");
    let (ctx, library) = fresh_ctx(tmp.path()).await;

    sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'placeholder')")
        .execute(library.pool())
        .await
        .expect("seed book");
    // No provenance rows.

    let stage = ConsensusStage::new();
    stage.run(&ctx, BookId(1)).await.expect("consensus run");

    let title: String = sqlx::query_scalar("SELECT title FROM books WHERE book_id = 1")
        .fetch_one(library.pool())
        .await
        .expect("read title");
    assert_eq!(title, "placeholder");
}

#[tokio::test]
async fn check_constraint_rejects_off_vocabulary_field() {
    // Migration 005 (slice C5.3) pins the `field` vocabulary at
    // the DB layer with a CHECK constraint matching the
    // `ab_core::Field` enum. The Rust path can never produce an
    // off-vocabulary value because every write goes through
    // `Field::*.as_str()`, but a runtime `sqlx::query()` could
    // bypass that — the CHECK is the storage-layer net.
    let tmp = TempDir::new().expect("tmpdir");
    let (_ctx, library) = fresh_ctx(tmp.path()).await;

    sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'placeholder')")
        .execute(library.pool())
        .await
        .expect("seed book");

    let res = sqlx::query(
        "INSERT INTO book_field_provenance \
         (book_id, field, value, source, stage, confidence) VALUES (1, ?, ?, ?, ?, ?)",
    )
    .bind("not_a_field")
    .bind("anything")
    .bind("manual")
    .bind("read-tags")
    .bind(0.5_f64)
    .execute(library.pool())
    .await;

    assert!(
        res.is_err(),
        "expected CHECK constraint to reject off-vocabulary field; \
         got Ok response — migration 005 must have regressed"
    );
    let err = format!("{}", res.unwrap_err());
    assert!(
        err.to_lowercase().contains("check"),
        "expected CHECK constraint error, got: {err}"
    );
}

#[tokio::test]
async fn check_constraint_accepts_every_field_variant() {
    // Mirror of the above: every variant of `ab_core::Field`
    // must be accepted by the CHECK constraint. If a new variant
    // is added without extending migration 005's `field IN (…)`
    // list, this test catches it.
    use ab_core::Field;
    let tmp = TempDir::new().expect("tmpdir");
    let (_ctx, library) = fresh_ctx(tmp.path()).await;

    sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'placeholder')")
        .execute(library.pool())
        .await
        .expect("seed book");

    let all_variants = [
        Field::Title,
        Field::Subtitle,
        Field::Description,
        Field::Language,
        Field::ReleaseDate,
        Field::DurationSeconds,
        Field::Asin,
        Field::Isbn,
        Field::Author,
        Field::Narrator,
        Field::Publisher,
        Field::Series,
        Field::Genre,
        Field::CoverUrl,
        Field::Abridged,
        Field::Explicit,
    ];
    for f in all_variants {
        let res = sqlx::query(
            "INSERT INTO book_field_provenance \
             (book_id, field, value, source, stage, confidence) VALUES (1, ?, ?, ?, ?, ?)",
        )
        .bind(f.as_str())
        .bind("v")
        .bind("manual")
        .bind("read-tags")
        .bind(0.5_f64)
        .execute(library.pool())
        .await;
        assert!(
            res.is_ok(),
            "CHECK constraint rejected variant {f:?} — migration 005 IN list out of sync"
        );
    }
}
