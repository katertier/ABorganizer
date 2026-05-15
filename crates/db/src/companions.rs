//! `book_companions` + `companion_nearby_books` helpers (ADR-0043).
//!
//! Pairs with the future C.2b2 scanner integration in `ab-scan`
//! (or `ab-companion`'s scan module — orphan rules permit either).
//! This module owns the CRUD over the two companion tables + the
//! denormalised `book_files.companion_paired_count` maintenance.
//!
//! Behaviour:
//!
//!   - [`upsert_companion`] — insert or update by path (UNIQUE).
//!     The scanner calls this once per discovered file. Returns
//!     the `companion_id` so callers can attach pair / nearby
//!     relationships.
//!   - [`set_pair`] — set `book_companions.book_id` and bump the
//!     matching `book_files.companion_paired_count`. Atomic via a
//!     transaction.
//!   - [`clear_pair`] — set `book_companions.book_id = NULL` and
//!     decrement the previous book's counters.
//!   - [`replace_nearby`] — replace the entire junction-hint set
//!     for a companion in one transaction. Used when the geometry
//!     re-runs (e.g. a new audiobook lands and its directory now
//!     overlaps an existing orphan).
//!   - [`paired_count_for_book`] — read-only diagnostic.
//!
//! All functions take a `&SqlitePool` and use sqlx compile-time
//! `query!` macros so schema drift surfaces at build time.

use ab_core::{BookId, Error, Result};
use sqlx::SqlitePool;

/// Newtype around `book_companions.companion_id`. Public so the
/// scanner can pass it back into [`set_pair`] / [`replace_nearby`]
/// in a typed way rather than juggling bare `i64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CompanionId(pub i64);

/// One companion as discovered by the scanner — payload for
/// [`upsert_companion`].
#[derive(Debug, Clone)]
pub struct CompanionRecord<'a> {
    /// Absolute path on disk; UNIQUE in the schema.
    pub path: &'a str,
    /// Token from `book_companions.format` CHECK constraint
    /// (`pdf`, `epub`, `cbz`, …, `unknown`).
    pub format: &'a str,
    /// Token from `book_companions.parse_tier` CHECK constraint.
    pub parse_tier: &'a str,
    /// BLAKE3 hex of the file's bytes. Drives the rescan dedupe.
    pub content_hash: &'a str,
    /// File size at discovery, bytes.
    pub bytes: i64,
    /// Unix seconds of the original discovery scan. On update
    /// the schema preserves the existing value via the
    /// `ON CONFLICT … DO UPDATE` clause that omits this column.
    pub discovered_at: i64,
}

/// Insert (or update by `path`) one companion row.
///
/// `path` is the UNIQUE key; a re-scan that re-discovers the
/// same path updates the format / tier / hash / bytes fields
/// to the latest read but preserves the `discovered_at`
/// timestamp from the original insert. `parsed_at` stays NULL
/// until C.4 EPUB name-dict extraction completes.
///
/// Returns the row's `companion_id` whether inserted or updated.
///
/// # Errors
///
/// `Error::Database` on any sqlx failure (transient connection
/// loss, foreign-key violation when `book_id` references a
/// deleted book row, CHECK violation if `format` / `parse_tier`
/// don't match the schema enums).
pub async fn upsert_companion(pool: &SqlitePool, rec: CompanionRecord<'_>) -> Result<CompanionId> {
    let row = sqlx::query!(
        "INSERT INTO book_companions \
            (path, format, parse_tier, content_hash, bytes, discovered_at) \
         VALUES (?, ?, ?, ?, ?, ?) \
         ON CONFLICT(path) DO UPDATE SET \
            format = excluded.format, \
            parse_tier = excluded.parse_tier, \
            content_hash = excluded.content_hash, \
            bytes = excluded.bytes \
         RETURNING companion_id",
        rec.path,
        rec.format,
        rec.parse_tier,
        rec.content_hash,
        rec.bytes,
        rec.discovered_at,
    )
    .fetch_one(pool)
    .await
    .map_err(|e| Error::Database(format!("upsert_companion: {e}")))?;
    Ok(CompanionId(row.companion_id))
}

/// Pair a companion to an audiobook.
///
/// Clears any previous pair + every `companion_nearby_books` hint
/// for this companion. Atomic: the previous pair's
/// `book_files.companion_paired_count` (if any) is decremented in
/// the same transaction the new pair is set.
///
/// # Errors
///
/// `Error::Database` on any sqlx failure.
pub async fn set_pair(pool: &SqlitePool, companion_id: CompanionId, book_id: BookId) -> Result<()> {
    let cid = companion_id.0;
    let bid = book_id.0;
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| Error::Database(format!("set_pair begin: {e}")))?;

    // Read previous book_id so we can decrement its denormalised
    // counter. If unchanged, skip the dec/inc pair entirely.
    let prev_book_id: Option<i64> = sqlx::query_scalar!(
        "SELECT book_id FROM book_companions WHERE companion_id = ?",
        cid,
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| Error::Database(format!("set_pair lookup prev: {e}")))?;

    if prev_book_id != Some(bid) {
        if let Some(prev) = prev_book_id {
            decrement_paired_count(&mut tx, prev).await?;
        }
        increment_paired_count(&mut tx, bid).await?;
    }

    sqlx::query!(
        "UPDATE book_companions SET book_id = ? WHERE companion_id = ?",
        bid,
        cid,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| Error::Database(format!("set_pair update: {e}")))?;

    sqlx::query!(
        "DELETE FROM companion_nearby_books WHERE companion_id = ?",
        cid,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| Error::Database(format!("set_pair clear nearby: {e}")))?;

    tx.commit()
        .await
        .map_err(|e| Error::Database(format!("set_pair commit: {e}")))?;
    Ok(())
}

/// Clear a companion's `book_id` (mark unpaired / orphan).
///
/// Decrements the previous book's counter if any. Junction-hint
/// rows are left in place — the caller is the right place to
/// decide whether to repopulate them via [`replace_nearby`].
///
/// # Errors
///
/// `Error::Database` on any sqlx failure.
pub async fn clear_pair(pool: &SqlitePool, companion_id: CompanionId) -> Result<()> {
    let cid = companion_id.0;
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| Error::Database(format!("clear_pair begin: {e}")))?;

    let prev_book_id: Option<i64> = sqlx::query_scalar!(
        "SELECT book_id FROM book_companions WHERE companion_id = ?",
        cid,
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| Error::Database(format!("clear_pair lookup: {e}")))?;

    if let Some(prev) = prev_book_id {
        decrement_paired_count(&mut tx, prev).await?;
    }

    sqlx::query!(
        "UPDATE book_companions SET book_id = NULL WHERE companion_id = ?",
        cid,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| Error::Database(format!("clear_pair update: {e}")))?;

    tx.commit()
        .await
        .map_err(|e| Error::Database(format!("clear_pair commit: {e}")))?;
    Ok(())
}

/// Replace the full junction-hint set for an unpaired companion.
///
/// Deletes every existing row for this companion then re-inserts
/// the supplied `book_ids`. Used when the auto-pair geometry runs
/// fresh and the candidate set may have changed.
///
/// # Errors
///
/// `Error::Database` on any sqlx failure.
pub async fn replace_nearby(
    pool: &SqlitePool,
    companion_id: CompanionId,
    book_ids: &[BookId],
    discovered_at: i64,
) -> Result<()> {
    let cid = companion_id.0;
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| Error::Database(format!("replace_nearby begin: {e}")))?;

    sqlx::query!(
        "DELETE FROM companion_nearby_books WHERE companion_id = ?",
        cid,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| Error::Database(format!("replace_nearby clear: {e}")))?;

    for book in book_ids {
        let bid = book.0;
        sqlx::query!(
            "INSERT INTO companion_nearby_books \
                (companion_id, book_id, discovered_at) \
             VALUES (?, ?, ?)",
            cid,
            bid,
            discovered_at,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| Error::Database(format!("replace_nearby insert: {e}")))?;
    }

    tx.commit()
        .await
        .map_err(|e| Error::Database(format!("replace_nearby commit: {e}")))?;
    Ok(())
}

/// Sum of `book_files.companion_paired_count` for one book. Used
/// by tests + diagnostics. Returns 0 when the book has no active
/// `book_files` rows.
///
/// # Errors
///
/// `Error::Database` on any sqlx failure.
pub async fn paired_count_for_book(pool: &SqlitePool, book_id: BookId) -> Result<i64> {
    let bid = book_id.0;
    // CAST forces a concrete INTEGER return so sqlx doesn't
    // pick `Option<()>` for the SUM(...) result. `COALESCE` to
    // zero when there are no active book_files rows yet.
    let row = sqlx::query_scalar!(
        "SELECT CAST(COALESCE(SUM(companion_paired_count), 0) AS INTEGER) AS \"sum!: i64\" \
         FROM book_files WHERE book_id = ?",
        bid,
    )
    .fetch_one(pool)
    .await
    .map_err(|e| Error::Database(format!("paired_count_for_book: {e}")))?;
    Ok(row)
}

async fn increment_paired_count(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    book_id: i64,
) -> Result<()> {
    // Bump every active book_files row for the book. Inactive
    // rows (is_active = 0) stay untouched — the denormalised
    // count is meant for the list-view JOIN against active rows.
    sqlx::query!(
        "UPDATE book_files \
         SET companion_paired_count = companion_paired_count + 1 \
         WHERE book_id = ? AND is_active = 1",
        book_id,
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("increment_paired_count: {e}")))?;
    Ok(())
}

async fn decrement_paired_count(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    book_id: i64,
) -> Result<()> {
    // Floor at 0 — a counter underflow is a bug but should never
    // panic the daemon. The MAX() clamp keeps the column
    // monotonic non-negative.
    sqlx::query!(
        "UPDATE book_files \
         SET companion_paired_count = MAX(0, companion_paired_count - 1) \
         WHERE book_id = ? AND is_active = 1",
        book_id,
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("decrement_paired_count: {e}")))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::LibraryDb;
    use ab_core::tunables::DbTunables;
    use tempfile::TempDir;

    async fn fresh_library() -> (LibraryDb, TempDir) {
        let tmp = TempDir::new().expect("tmpdir");
        let library = LibraryDb::open(&tmp.path().join("library.db"), &DbTunables::default())
            .await
            .expect("open library");
        (library, tmp)
    }

    async fn seed_book(library: &LibraryDb, book_id: i64) {
        sqlx::query("INSERT INTO books (book_id, title) VALUES (?, ?)")
            .bind(book_id)
            .bind(format!("Book {book_id}"))
            .execute(library.pool())
            .await
            .expect("seed book");
    }

    fn rec<'a>(path: &'a str, hash: &'a str, bytes: i64) -> CompanionRecord<'a> {
        CompanionRecord {
            path,
            format: "pdf",
            parse_tier: "document",
            content_hash: hash,
            bytes,
            discovered_at: 42,
        }
    }

    async fn seed_book_file(library: &LibraryDb, book_id: i64, file_id: i64, path: &str) {
        // Minimal book_files row — the active flag drives the
        // counter-maintenance UPDATE filter.
        sqlx::query(
            "INSERT INTO book_files \
                (file_id, book_id, file_path, file_hash, is_active) \
             VALUES (?, ?, ?, ?, 1)",
        )
        .bind(file_id)
        .bind(book_id)
        .bind(path)
        .bind(format!("hash-{file_id}"))
        .execute(library.pool())
        .await
        .expect("seed book_files");
    }

    #[tokio::test]
    async fn upsert_companion_returns_companion_id() {
        let (library, _tmp) = fresh_library().await;
        let id = upsert_companion(library.pool(), rec("/lib/a/notes.pdf", "deadbeef", 1024))
            .await
            .expect("insert");
        assert!(id.0 > 0);
    }

    #[tokio::test]
    async fn upsert_companion_updates_on_repeated_path() {
        let (library, _tmp) = fresh_library().await;
        let id1 = upsert_companion(library.pool(), rec("/lib/a/notes.pdf", "deadbeef", 1024))
            .await
            .expect("first insert");
        let id2 = upsert_companion(library.pool(), rec("/lib/a/notes.pdf", "feedface", 2048))
            .await
            .expect("second insert (update)");
        assert_eq!(id1, id2, "same path → same row");
        let (hash, bytes): (String, i64) = sqlx::query_as(
            "SELECT content_hash, bytes FROM book_companions WHERE companion_id = ?",
        )
        .bind(id1.0)
        .fetch_one(library.pool())
        .await
        .expect("read back");
        assert_eq!(hash, "feedface");
        assert_eq!(bytes, 2048);
    }

    #[tokio::test]
    async fn set_pair_marks_book_and_increments_counter() {
        let (library, _tmp) = fresh_library().await;
        seed_book(&library, 1).await;
        seed_book_file(&library, 1, 10, "/lib/a/book.m4b").await;
        let cid = upsert_companion(library.pool(), rec("/lib/a/notes.pdf", "deadbeef", 1024))
            .await
            .expect("insert");
        set_pair(library.pool(), cid, BookId(1))
            .await
            .expect("pair");
        assert_eq!(
            paired_count_for_book(library.pool(), BookId(1))
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn set_pair_decrements_previous_book() {
        let (library, _tmp) = fresh_library().await;
        seed_book(&library, 1).await;
        seed_book(&library, 2).await;
        seed_book_file(&library, 1, 10, "/lib/a/book1.m4b").await;
        seed_book_file(&library, 2, 11, "/lib/b/book2.m4b").await;
        let cid = upsert_companion(library.pool(), rec("/lib/notes.pdf", "x", 1))
            .await
            .expect("insert");
        set_pair(library.pool(), cid, BookId(1))
            .await
            .expect("pair 1");
        set_pair(library.pool(), cid, BookId(2))
            .await
            .expect("repair");
        assert_eq!(
            paired_count_for_book(library.pool(), BookId(1))
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            paired_count_for_book(library.pool(), BookId(2))
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn set_pair_is_idempotent_on_same_book() {
        let (library, _tmp) = fresh_library().await;
        seed_book(&library, 1).await;
        seed_book_file(&library, 1, 10, "/lib/a/book.m4b").await;
        let cid = upsert_companion(library.pool(), rec("/lib/a/notes.pdf", "x", 1))
            .await
            .expect("insert");
        set_pair(library.pool(), cid, BookId(1))
            .await
            .expect("pair");
        set_pair(library.pool(), cid, BookId(1))
            .await
            .expect("pair again");
        assert_eq!(
            paired_count_for_book(library.pool(), BookId(1))
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn clear_pair_unpairs_and_decrements() {
        let (library, _tmp) = fresh_library().await;
        seed_book(&library, 1).await;
        seed_book_file(&library, 1, 10, "/lib/a/book.m4b").await;
        let cid = upsert_companion(library.pool(), rec("/lib/a/notes.pdf", "x", 1))
            .await
            .expect("insert");
        set_pair(library.pool(), cid, BookId(1))
            .await
            .expect("pair");
        clear_pair(library.pool(), cid).await.expect("clear");
        assert_eq!(
            paired_count_for_book(library.pool(), BookId(1))
                .await
                .unwrap(),
            0
        );
        let book_id: Option<i64> =
            sqlx::query_scalar("SELECT book_id FROM book_companions WHERE companion_id = ?")
                .bind(cid.0)
                .fetch_one(library.pool())
                .await
                .expect("read back");
        assert_eq!(book_id, None);
    }

    #[tokio::test]
    async fn replace_nearby_swaps_the_junction_set() {
        let (library, _tmp) = fresh_library().await;
        seed_book(&library, 1).await;
        seed_book(&library, 2).await;
        seed_book(&library, 3).await;
        let cid = upsert_companion(library.pool(), rec("/lib/notes.pdf", "x", 1))
            .await
            .expect("insert");
        replace_nearby(library.pool(), cid, &[BookId(1), BookId(2)], 42)
            .await
            .expect("first nearby");
        let count1: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM companion_nearby_books WHERE companion_id = ?",
        )
        .bind(cid.0)
        .fetch_one(library.pool())
        .await
        .expect("count1");
        assert_eq!(count1, 2);
        replace_nearby(library.pool(), cid, &[BookId(3)], 50)
            .await
            .expect("replace");
        let books: Vec<i64> = sqlx::query_scalar(
            "SELECT book_id FROM companion_nearby_books WHERE companion_id = ? ORDER BY book_id",
        )
        .bind(cid.0)
        .fetch_all(library.pool())
        .await
        .expect("read back");
        assert_eq!(books, vec![3]);
    }

    #[tokio::test]
    async fn replace_nearby_with_empty_list_clears() {
        let (library, _tmp) = fresh_library().await;
        seed_book(&library, 1).await;
        let cid = upsert_companion(library.pool(), rec("/lib/notes.pdf", "x", 1))
            .await
            .expect("insert");
        replace_nearby(library.pool(), cid, &[BookId(1)], 42)
            .await
            .expect("seed");
        replace_nearby(library.pool(), cid, &[], 42)
            .await
            .expect("clear");
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM companion_nearby_books WHERE companion_id = ?",
        )
        .bind(cid.0)
        .fetch_one(library.pool())
        .await
        .expect("count");
        assert_eq!(count, 0);
    }
}
