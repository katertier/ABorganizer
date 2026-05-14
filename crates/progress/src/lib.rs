//! Reading state + cross-device playback progress (ADR-0033).
//!
//! Owns three book-level columns (`reading_status`, `rating`,
//! `notes`) plus the `media_progress` table. API + CLI consume the
//! functions here; storage is `library.db` (durable, nightly
//! backup).
//!
//! Conflict resolution is last-write-wins on `last_synced_at`. The
//! optional play-lock (ADR-0033 § Sync semantics) is deferred — it
//! lives in `ephemeral.db` and arrives with the multi-device
//! testing slice once the player ships.

#![forbid(unsafe_code)]
#![allow(missing_docs)] // scaffold; will be tightened in follow-up slices

use ab_core::{BookId, Error, ReadingStatus, Result};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

/// Position + finished-state for a book, last-writer-wins.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaProgress {
    pub book_id: BookId,
    pub current_time_ms: i64,
    pub is_finished: bool,
    pub last_listened_at: Option<i64>,
    pub last_synced_from: Option<String>,
    pub last_synced_at: Option<i64>,
}

/// Body of a `POST /api/v1/session/{book_id}/sync` call.
#[derive(Debug, Clone, Deserialize)]
pub struct SyncRequest {
    pub current_time_ms: i64,
    pub is_finished: bool,
    /// Pairing-token name of the device reporting in. Recorded as
    /// `last_synced_from` so the UI can surface "you listened on Mac".
    pub from: String,
}

/// Errors specific to this crate.
#[derive(Debug, thiserror::Error)]
pub enum ProgressError {
    /// Book referenced by id does not exist.
    #[error("book {0} not found")]
    NotFound(i64),
    /// Persistence-layer failure; wraps the upstream error.
    #[error(transparent)]
    Core(#[from] Error),
}

impl From<sqlx::Error> for ProgressError {
    fn from(e: sqlx::Error) -> Self {
        Self::Core(Error::Database(e.to_string()))
    }
}

async fn ensure_book(pool: &SqlitePool, book_id: i64) -> Result<(), ProgressError> {
    let row = sqlx::query!(
        r#"SELECT EXISTS(SELECT 1 FROM books WHERE book_id = ?) AS "exists!: i64""#,
        book_id,
    )
    .fetch_one(pool)
    .await?;
    if row.exists == 0 {
        return Err(ProgressError::NotFound(book_id));
    }
    Ok(())
}

/// Update `books.reading_status`.
///
/// Setting `Finished` also flips `media_progress.is_finished = 1`
/// (idempotent, upserts on first finish). Setting back to
/// `Reading` clears `is_finished` but preserves `current_time_ms`.
pub async fn set_status(
    pool: &SqlitePool,
    book_id: BookId,
    status: ReadingStatus,
) -> Result<(), ProgressError> {
    ensure_book(pool, book_id.0).await?;

    let status_str = status.as_str();
    let book_id_raw = book_id.0;
    let mut tx = pool.begin().await?;

    sqlx::query!(
        "UPDATE books SET reading_status = ?, updated_at = strftime('%s','now') \
         WHERE book_id = ?",
        status_str,
        book_id_raw,
    )
    .execute(&mut *tx)
    .await?;

    match status {
        ReadingStatus::Finished => {
            sqlx::query!(
                "INSERT INTO media_progress (book_id, is_finished, last_listened_at) \
                 VALUES (?, 1, strftime('%s','now')) \
                 ON CONFLICT(book_id) DO UPDATE SET is_finished = 1",
                book_id_raw,
            )
            .execute(&mut *tx)
            .await?;
        }
        ReadingStatus::Reading => {
            sqlx::query!(
                "UPDATE media_progress SET is_finished = 0 WHERE book_id = ?",
                book_id_raw,
            )
            .execute(&mut *tx)
            .await?;
        }
        ReadingStatus::WantToRead | ReadingStatus::Dnf => {}
    }

    tx.commit().await?;
    Ok(())
}

/// Update `books.rating`. `None` clears the rating; `Some(1..=5)`
/// sets it. Values outside `1..=5` are rejected by the schema CHECK.
pub async fn set_rating(
    pool: &SqlitePool,
    book_id: BookId,
    rating: Option<u8>,
) -> Result<(), ProgressError> {
    ensure_book(pool, book_id.0).await?;
    let rating_i64 = rating.map(i64::from);
    let book_id_raw = book_id.0;
    sqlx::query!(
        "UPDATE books SET rating = ?, updated_at = strftime('%s','now') \
         WHERE book_id = ?",
        rating_i64,
        book_id_raw,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Update `books.notes`. Empty string clears the note (mapped to NULL).
pub async fn set_notes(
    pool: &SqlitePool,
    book_id: BookId,
    notes: Option<&str>,
) -> Result<(), ProgressError> {
    ensure_book(pool, book_id.0).await?;
    let normalised: Option<String> = notes
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let book_id_raw = book_id.0;
    sqlx::query!(
        "UPDATE books SET notes = ?, updated_at = strftime('%s','now') \
         WHERE book_id = ?",
        normalised,
        book_id_raw,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Apply a sync report from a device. Last-write-wins on
/// `last_synced_at`: if the incoming `now` is older than the
/// stored value, the write is dropped (returns `Ok(false)`).
///
/// Returns `Ok(true)` when the row was written.
pub async fn sync(
    pool: &SqlitePool,
    book_id: BookId,
    req: &SyncRequest,
) -> Result<bool, ProgressError> {
    ensure_book(pool, book_id.0).await?;

    let now = current_unix_seconds();
    let is_finished_i64 = i64::from(req.is_finished);
    let book_id_raw = book_id.0;

    let prior_synced_at: Option<i64> = sqlx::query_scalar!(
        r#"SELECT last_synced_at AS "v?: i64" FROM media_progress WHERE book_id = ?"#,
        book_id_raw,
    )
    .fetch_optional(pool)
    .await?
    .flatten();

    if let Some(prior) = prior_synced_at {
        if prior > now {
            return Ok(false);
        }
    }

    sqlx::query!(
        "INSERT INTO media_progress (
            book_id, current_time_ms, is_finished,
            last_listened_at, last_synced_from, last_synced_at
         ) VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(book_id) DO UPDATE SET
            current_time_ms  = excluded.current_time_ms,
            is_finished      = excluded.is_finished,
            last_listened_at = excluded.last_listened_at,
            last_synced_from = excluded.last_synced_from,
            last_synced_at   = excluded.last_synced_at",
        book_id_raw,
        req.current_time_ms,
        is_finished_i64,
        now,
        req.from,
        now,
    )
    .execute(pool)
    .await?;

    if req.is_finished {
        sqlx::query!(
            "UPDATE books SET reading_status = 'finished', \
             updated_at = strftime('%s','now') WHERE book_id = ?",
            book_id_raw,
        )
        .execute(pool)
        .await?;
    }

    Ok(true)
}

/// Read current progress for a book. Returns `Ok(None)` when the
/// book exists but no progress row has been written yet.
pub async fn get(
    pool: &SqlitePool,
    book_id: BookId,
) -> Result<Option<MediaProgress>, ProgressError> {
    ensure_book(pool, book_id.0).await?;
    let book_id_raw = book_id.0;
    let row = sqlx::query!(
        r#"SELECT
            current_time_ms  AS "current_time_ms!: i64",
            is_finished      AS "is_finished!: i64",
            last_listened_at AS "last_listened_at: i64",
            last_synced_from AS "last_synced_from: String",
            last_synced_at   AS "last_synced_at: i64"
         FROM media_progress WHERE book_id = ?"#,
        book_id_raw,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| MediaProgress {
        book_id,
        current_time_ms: r.current_time_ms,
        is_finished: r.is_finished != 0,
        last_listened_at: r.last_listened_at,
        last_synced_from: r.last_synced_from,
        last_synced_at: r.last_synced_at,
    }))
}

fn current_unix_seconds() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    secs.try_into().unwrap_or(i64::MAX)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use ab_core::tunables::DbTunables;
    use ab_db::LibraryDb;
    use tempfile::TempDir;

    async fn open_db() -> (TempDir, LibraryDb) {
        let dir = TempDir::new().expect("tempdir");
        let tunables = DbTunables::default();
        let db = LibraryDb::open(&dir.path().join("library.db"), &tunables)
            .await
            .expect("open library");
        (dir, db)
    }

    async fn insert_book(db: &LibraryDb, title: &str) -> i64 {
        sqlx::query!("INSERT INTO books (title) VALUES (?)", title)
            .execute(db.pool())
            .await
            .expect("insert book")
            .last_insert_rowid()
    }

    #[tokio::test]
    async fn status_round_trip() {
        let (_dir, db) = open_db().await;
        let id = BookId(insert_book(&db, "Test").await);

        set_status(db.pool(), id, ReadingStatus::Reading)
            .await
            .expect("set reading");
        let row = sqlx::query_scalar!("SELECT reading_status FROM books WHERE book_id = ?", id.0)
            .fetch_one(db.pool())
            .await
            .expect("read status");
        assert_eq!(row, "reading");

        set_status(db.pool(), id, ReadingStatus::Finished)
            .await
            .expect("set finished");
        let is_finished: i64 = sqlx::query_scalar!(
            "SELECT is_finished FROM media_progress WHERE book_id = ?",
            id.0
        )
        .fetch_one(db.pool())
        .await
        .expect("read progress");
        assert_eq!(is_finished, 1);
    }

    #[tokio::test]
    async fn rating_clears_and_sets() {
        let (_dir, db) = open_db().await;
        let id = BookId(insert_book(&db, "Test").await);

        set_rating(db.pool(), id, Some(4)).await.expect("set 4");
        let r: Option<i64> = sqlx::query_scalar!(
            r#"SELECT rating AS "v?: i64" FROM books WHERE book_id = ?"#,
            id.0,
        )
        .fetch_one(db.pool())
        .await
        .expect("read rating");
        assert_eq!(r, Some(4));

        set_rating(db.pool(), id, None).await.expect("clear");
        let r: Option<i64> = sqlx::query_scalar!(
            r#"SELECT rating AS "v?: i64" FROM books WHERE book_id = ?"#,
            id.0,
        )
        .fetch_one(db.pool())
        .await
        .expect("read cleared");
        assert_eq!(r, None);
    }

    #[tokio::test]
    async fn notes_trim_to_null() {
        let (_dir, db) = open_db().await;
        let id = BookId(insert_book(&db, "Test").await);
        set_notes(db.pool(), id, Some("   ")).await.expect("ws");
        let n: Option<String> = sqlx::query_scalar!(
            r#"SELECT notes AS "v?: String" FROM books WHERE book_id = ?"#,
            id.0,
        )
        .fetch_one(db.pool())
        .await
        .expect("read");
        assert_eq!(n, None);
    }

    #[tokio::test]
    async fn sync_upserts_and_returns_value() {
        let (_dir, db) = open_db().await;
        let id = BookId(insert_book(&db, "Test").await);
        let req = SyncRequest {
            current_time_ms: 12_345,
            is_finished: false,
            from: "mac-mini".into(),
        };
        let wrote = sync(db.pool(), id, &req).await.expect("sync");
        assert!(wrote);
        let p = get(db.pool(), id).await.expect("get").expect("row");
        assert_eq!(p.current_time_ms, 12_345);
        assert_eq!(p.last_synced_from.as_deref(), Some("mac-mini"));
    }

    #[tokio::test]
    async fn sync_finished_flips_book_status() {
        let (_dir, db) = open_db().await;
        let id = BookId(insert_book(&db, "Test").await);
        sync(
            db.pool(),
            id,
            &SyncRequest {
                current_time_ms: 1_000_000,
                is_finished: true,
                from: "imac".into(),
            },
        )
        .await
        .expect("sync");
        let s = sqlx::query_scalar!("SELECT reading_status FROM books WHERE book_id = ?", id.0)
            .fetch_one(db.pool())
            .await
            .expect("read status");
        assert_eq!(s, "finished");
    }

    #[tokio::test]
    async fn missing_book_returns_not_found() {
        let (_dir, db) = open_db().await;
        let err = set_status(db.pool(), BookId(9999), ReadingStatus::Reading)
            .await
            .expect_err("missing book should error");
        assert!(matches!(err, ProgressError::NotFound(9999)));
    }
}
