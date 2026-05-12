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
        stage_name: "consensus",
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
        sqlx::query(
            "INSERT INTO book_field_provenance \
             (book_id, field, value, confidence, source) VALUES (1, ?, ?, ?, ?)",
        )
        .bind(field)
        .bind(value)
        .bind(conf)
        .bind(source)
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
