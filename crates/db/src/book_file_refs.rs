//! Source-file refcount helpers (ADR-0027).
//!
//! Stages that read a book's audio file (transcribe, fingerprint,
//! audiologo detect, future audiologo apply) acquire a ref at
//! run-start and release at run-end. The future transcode-to-m4b
//! stage and the `post-transcode-sources` cleanup target consult
//! [`live_ref_count`] to decide whether the source file can be
//! reaped after a successful transcode.
//!
//! ## RAII guard
//!
//! Holding a [`RefHandle`] is the operational contract; dropping
//! it without calling [`RefHandle::release`] does NOT release the
//! row (Drop can't be async). Stages should use an explicit
//! release at the end of `run()` — see the doc-example below.
//!
//! ```ignore
//! let handle = book_file_refs::acquire(
//!     library.pool(), file_id, STAGE_ID, book_id,
//! ).await?;
//! let result = do_audio_work(&handle).await;
//! handle.release(library.pool()).await?;
//! result
//! ```
//!
//! A future `tokio::task::JoinSet`-driven scope will move this
//! to RAII without losing async-ness.

use ab_core::{Error, Result};
use sqlx::SqlitePool;

/// Returned by [`acquire`]. Carries the row id so [`release`]
/// can target the exact row even if multiple refs are held.
///
/// The `ref_id` field is private so callers cannot synthesise a
/// `RefHandle` and call `release` on it — the only legal way to
/// produce one is via [`acquire`], which guarantees the row
/// exists. Use [`RefHandle::ref_id`] when an audit / diagnostic
/// path needs the raw id.
#[derive(Debug)]
pub struct RefHandle {
    /// `book_file_refs.ref_id` of the live row this handle
    /// represents.
    ref_id: i64,
}

impl RefHandle {
    /// Row id for audit / diagnostic surfaces. Production code
    /// uses [`Self::release`] instead of poking the id directly.
    #[must_use]
    pub const fn ref_id(&self) -> i64 {
        self.ref_id
    }
}

impl RefHandle {
    /// Release this ref (mark `released_at = now`). Idempotent:
    /// re-releasing the same handle is a no-op (the UPDATE
    /// affects zero rows the second time).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] on SQLite failure.
    pub async fn release(&self, pool: &SqlitePool) -> Result<()> {
        sqlx::query!(
            "UPDATE book_file_refs \
                SET released_at = strftime('%s','now') \
              WHERE ref_id = ? AND released_at IS NULL",
            self.ref_id,
        )
        .execute(pool)
        .await
        .map_err(|e| Error::Database(format!("book_file_refs release: {e}")))?;
        Ok(())
    }
}

/// Acquire a refcount on `file_id` for the given stage + book.
///
/// Inserts a new row at `released_at = NULL`. Multiple acquires
/// from the same `(file_id, holder_stage, holder_book_id)` triple
/// are permitted (each `acquired_at` differs); each yields its
/// own [`RefHandle`].
///
/// # Errors
///
/// Returns [`Error::Database`] on insert failure (e.g. `file_id`
/// doesn't exist → FK violation).
pub async fn acquire(
    pool: &SqlitePool,
    file_id: i64,
    holder_stage: &str,
    holder_book_id: i64,
) -> Result<RefHandle> {
    let row = sqlx::query!(
        r#"INSERT INTO book_file_refs (file_id, holder_stage, holder_book_id)
                 VALUES (?, ?, ?)
           RETURNING ref_id AS "ref_id!: i64""#,
        file_id,
        holder_stage,
        holder_book_id,
    )
    .fetch_one(pool)
    .await
    .map_err(|e| Error::Database(format!("book_file_refs acquire: {e}")))?;
    Ok(RefHandle { ref_id: row.ref_id })
}

/// Count live refs on a file. Used by the post-transcode-sources
/// cleanup target to decide whether the source can be deleted.
///
/// # Errors
///
/// Returns [`Error::Database`] on SQLite failure.
pub async fn live_ref_count(pool: &SqlitePool, file_id: i64) -> Result<i64> {
    let row = sqlx::query!(
        r#"SELECT COUNT(*) AS "n!: i64" FROM book_file_refs
            WHERE file_id = ? AND released_at IS NULL"#,
        file_id,
    )
    .fetch_one(pool)
    .await
    .map_err(|e| Error::Database(format!("book_file_refs count: {e}")))?;
    Ok(row.n)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::LibraryDb;
    use ab_core::tunables::DbTunables;
    use tempfile::TempDir;

    async fn fresh(dir: &std::path::Path) -> LibraryDb {
        LibraryDb::open(&dir.join("library.db"), &DbTunables::default())
            .await
            .expect("open")
    }

    async fn seed_book_and_file(library: &LibraryDb) {
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'fixture')")
            .execute(library.pool())
            .await
            .expect("seed book");
        sqlx::query(
            "INSERT INTO book_files (file_id, book_id, file_path, is_active) \
             VALUES (10, 1, '/tmp/a.m4b', 1)",
        )
        .execute(library.pool())
        .await
        .expect("seed file");
    }

    #[tokio::test]
    async fn acquire_increments_live_count() {
        let tmp = TempDir::new().expect("tmpdir");
        let library = fresh(tmp.path()).await;
        seed_book_and_file(&library).await;

        assert_eq!(live_ref_count(library.pool(), 10).await.expect("count0"), 0);
        let _h = acquire(library.pool(), 10, "transcribe-head-tail", 1)
            .await
            .expect("acquire");
        assert_eq!(live_ref_count(library.pool(), 10).await.expect("count1"), 1);
    }

    #[tokio::test]
    async fn release_drops_live_count() {
        let tmp = TempDir::new().expect("tmpdir");
        let library = fresh(tmp.path()).await;
        seed_book_and_file(&library).await;

        let h = acquire(library.pool(), 10, "fingerprint", 1)
            .await
            .expect("acquire");
        assert_eq!(live_ref_count(library.pool(), 10).await.expect("c"), 1);
        h.release(library.pool()).await.expect("release");
        assert_eq!(live_ref_count(library.pool(), 10).await.expect("c2"), 0);
    }

    #[tokio::test]
    async fn parallel_refs_compose() {
        let tmp = TempDir::new().expect("tmpdir");
        let library = fresh(tmp.path()).await;
        seed_book_and_file(&library).await;

        let h1 = acquire(library.pool(), 10, "transcribe-head-tail", 1)
            .await
            .expect("a1");
        let h2 = acquire(library.pool(), 10, "fingerprint", 1)
            .await
            .expect("a2");
        let h3 = acquire(library.pool(), 10, "detect-audiologo", 1)
            .await
            .expect("a3");
        assert_eq!(live_ref_count(library.pool(), 10).await.expect("c"), 3);

        h2.release(library.pool()).await.expect("r2");
        assert_eq!(live_ref_count(library.pool(), 10).await.expect("c2"), 2);

        h1.release(library.pool()).await.expect("r1");
        h3.release(library.pool()).await.expect("r3");
        assert_eq!(live_ref_count(library.pool(), 10).await.expect("c0"), 0);
    }

    #[tokio::test]
    async fn double_release_is_no_op() {
        let tmp = TempDir::new().expect("tmpdir");
        let library = fresh(tmp.path()).await;
        seed_book_and_file(&library).await;

        let h = acquire(library.pool(), 10, "fingerprint", 1)
            .await
            .expect("acquire");
        h.release(library.pool()).await.expect("release");
        // Second release should not error.
        h.release(library.pool()).await.expect("idempotent");
        assert_eq!(live_ref_count(library.pool(), 10).await.expect("c"), 0);
    }
}
