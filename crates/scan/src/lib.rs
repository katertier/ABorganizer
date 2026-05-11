//! Directory walker + audio-file enumeration.
//!
//! # What this crate does
//!
//! Take a path; walk it; identify audio files by extension; insert
//! a `books` + `book_files` row per file into `library.db`. Returns
//! the [`BookId`]s of newly-inserted books for downstream pipeline
//! stages to operate on.
//!
//! # What this crate does NOT do (yet)
//!
//! - Probe files (duration, bitrate, codec) — that's the `tag-read`
//!   stage in slice 1B.
//! - Detect multi-file books (one folder = one book of N parts) —
//!   slice 1D.
//! - Compute `file_hash` for idempotent re-scan — slice 1D.
//! - Probe tag metadata (title, author, ASIN) — slice 1B / `tag-read`.
//!
//! For now, **one audio file = one book**. Title is the file stem.
//!
//! # Architectural placement
//!
//! Scan is NOT a [`Stage`](ab_pipeline::Stage) — Stages operate on
//! existing `BookId`s, and scan is what produces them. It's called
//! directly by the daemon's `POST /api/v1/library/scan` handler and
//! by the `aborg library scan` CLI command (via the same API).
//! Downstream stages (`tag-read`, `fingerprint`, `audiologo`,
//! `commit`) consume the `BookId`s scan emits via the scheduler.

use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use ab_core::{BookId, Error, Result};
use ab_db::LibraryDb;

/// Audio file extensions recognised by the scanner. Matched
/// case-insensitively. Any file outside this set is skipped (logged
/// at debug level).
///
/// AAX is included for completeness; full AAX support (decryption,
/// tag-write) lands in later slices but enumeration is fine now.
pub const AUDIO_EXTENSIONS: &[&str] = &["m4b", "m4a", "mp3", "flac", "opus", "ogg", "aax"];

/// Summary of one `scan` invocation.
#[derive(Debug, Clone, Default)]
pub struct ScanReport {
    /// `BookId`s for rows newly inserted by this scan.
    pub new_book_ids: Vec<BookId>,
    /// File paths skipped because they already exist in `book_files`
    /// (idempotent re-scan — without yet checking content).
    pub skipped_paths: Vec<PathBuf>,
    /// File paths walked but rejected as non-audio.
    pub non_audio_count: u64,
    /// File paths walked total (audio + non-audio).
    pub total_walked: u64,
}

/// Walk `root` recursively, register every audio file as a book.
///
/// One audio file = one book (multi-file detection lands in slice 1D).
/// File paths already present in `book_files` are skipped (the
/// `UNIQUE(file_path)` constraint prevents duplicate inserts at the
/// SQL layer; we check first for clean idempotency).
///
/// # Errors
///
/// Returns [`Error::Io`] on filesystem failures (e.g., unreadable
/// directory), [`Error::Database`] on SQL failures, or
/// [`Error::PathOutsideAllowed`] if `root` doesn't exist.
pub async fn scan(root: &Path, db: &LibraryDb) -> Result<ScanReport> {
    if !root.exists() {
        return Err(Error::PathOutsideAllowed(root.to_path_buf()));
    }
    let canonical_root = std::fs::canonicalize(root)?;
    tracing::info!(root = %canonical_root.display(), "scan.start");

    let mut report = ScanReport::default();

    for entry in WalkDir::new(&canonical_root).follow_links(false) {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "scan.walk_entry_error");
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        report.total_walked += 1;
        let path = entry.path();
        if !is_audio_file(path) {
            report.non_audio_count += 1;
            tracing::debug!(file = %path.display(), "scan.non_audio_skipped");
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(file = %path.display(), error = %e, "scan.metadata_failed");
                continue;
            }
        };
        let file_size = i64::try_from(metadata.len()).unwrap_or(i64::MAX);
        let modified_at = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        let format = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_lowercase);
        let title = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Untitled")
            .to_owned();
        let file_path = path.to_string_lossy().to_string();

        let row = NewBookFileRow {
            title: &title,
            file_path: &file_path,
            file_size,
            modified_at,
            format: format.as_deref(),
        };
        match insert_book_with_file(db, &row).await {
            Ok(Some(book_id)) => {
                report.new_book_ids.push(book_id);
            }
            Ok(None) => {
                // Already known.
                report.skipped_paths.push(path.to_path_buf());
            }
            Err(e) => {
                tracing::warn!(file = %path.display(), error = %e, "scan.insert_failed");
            }
        }
    }

    tracing::info!(
        new = report.new_book_ids.len(),
        skipped = report.skipped_paths.len(),
        non_audio = report.non_audio_count,
        total = report.total_walked,
        "scan.complete"
    );
    Ok(report)
}

/// True when `path`'s extension is in [`AUDIO_EXTENSIONS`]
/// (case-insensitive).
pub fn is_audio_file(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    let lower = ext.to_lowercase();
    AUDIO_EXTENSIONS.iter().any(|allowed| *allowed == lower)
}

/// Bundled args for one row insertion. Keeps
/// [`insert_book_with_file`] under the workspace's 5-parameter cap
/// (clippy `too_many_arguments`).
struct NewBookFileRow<'a> {
    title: &'a str,
    file_path: &'a str,
    file_size: i64,
    modified_at: Option<i64>,
    format: Option<&'a str>,
}

/// Insert a book + `book_files` row. Returns the new [`BookId`] on a
/// fresh insert, or `Ok(None)` when the `file_path` already exists.
async fn insert_book_with_file(db: &LibraryDb, row: &NewBookFileRow<'_>) -> Result<Option<BookId>> {
    let mut tx = db
        .pool()
        .begin()
        .await
        .map_err(|e| Error::Database(format!("begin tx: {e}")))?;

    // Cheap pre-check by UNIQUE(file_path); if present, no-op.
    let existing: Option<i64> =
        sqlx::query_scalar("SELECT book_id FROM book_files WHERE file_path = ? LIMIT 1")
            .bind(row.file_path)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| Error::Database(format!("check existing: {e}")))?;

    if existing.is_some() {
        // Nothing to do.
        return Ok(None);
    }

    let book_id: i64 = sqlx::query_scalar("INSERT INTO books (title) VALUES (?) RETURNING book_id")
        .bind(row.title)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| Error::Database(format!("insert book: {e}")))?;

    sqlx::query(
        "INSERT INTO book_files (book_id, file_path, file_size, modified_at, format, is_active) \
         VALUES (?, ?, ?, ?, ?, 1)",
    )
    .bind(book_id)
    .bind(row.file_path)
    .bind(row.file_size)
    .bind(row.modified_at)
    .bind(row.format)
    .execute(&mut *tx)
    .await
    .map_err(|e| Error::Database(format!("insert book_file: {e}")))?;

    tx.commit()
        .await
        .map_err(|e| Error::Database(format!("commit: {e}")))?;

    Ok(Some(BookId(book_id)))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::fs;

    use ab_core::tunables::DbTunables;
    use tempfile::TempDir;

    use super::*;

    fn touch(dir: &Path, name: &str) {
        fs::write(dir.join(name), b"\0\0\0\0").expect("write fixture file");
    }

    async fn fresh_db(dir: &Path) -> LibraryDb {
        let path = dir.join("library.db");
        LibraryDb::open(&path, &DbTunables::default())
            .await
            .expect("open db")
    }

    #[test]
    fn extension_check_is_case_insensitive() {
        assert!(is_audio_file(Path::new("foo.M4B")));
        assert!(is_audio_file(Path::new("foo.mp3")));
        assert!(is_audio_file(Path::new("foo.FLAC")));
        assert!(!is_audio_file(Path::new("foo.txt")));
        assert!(!is_audio_file(Path::new("foo")));
    }

    #[tokio::test]
    async fn scan_inserts_one_book_per_audio_file() {
        let tmp = TempDir::new().expect("tmpdir");
        let db_dir = tmp.path().join("db");
        fs::create_dir_all(&db_dir).expect("mkdir db");
        let lib_dir = tmp.path().join("lib");
        fs::create_dir_all(&lib_dir).expect("mkdir lib");

        touch(&lib_dir, "book1.m4b");
        touch(&lib_dir, "book2.mp3");
        touch(&lib_dir, "notes.txt");

        let db = fresh_db(&db_dir).await;
        let report = scan(&lib_dir, &db).await.expect("scan");

        assert_eq!(report.new_book_ids.len(), 2);
        assert_eq!(report.non_audio_count, 1);
        assert_eq!(report.total_walked, 3);
    }

    #[tokio::test]
    async fn rescan_is_idempotent() {
        let tmp = TempDir::new().expect("tmpdir");
        let db_dir = tmp.path().join("db");
        fs::create_dir_all(&db_dir).expect("mkdir db");
        let lib_dir = tmp.path().join("lib");
        fs::create_dir_all(&lib_dir).expect("mkdir lib");

        touch(&lib_dir, "a.m4b");

        let db = fresh_db(&db_dir).await;
        let r1 = scan(&lib_dir, &db).await.expect("scan 1");
        assert_eq!(r1.new_book_ids.len(), 1);

        let r2 = scan(&lib_dir, &db).await.expect("scan 2");
        assert_eq!(
            r2.new_book_ids.len(),
            0,
            "second scan should detect no new books"
        );
        assert_eq!(r2.skipped_paths.len(), 1);
    }

    #[tokio::test]
    async fn missing_root_returns_path_error() {
        let tmp = TempDir::new().expect("tmpdir");
        let db_dir = tmp.path().join("db");
        fs::create_dir_all(&db_dir).expect("mkdir db");
        let db = fresh_db(&db_dir).await;

        let missing = tmp.path().join("does-not-exist");
        let err = scan(&missing, &db).await.expect_err("should fail");
        assert!(matches!(err, Error::PathOutsideAllowed(_)));
    }
}
