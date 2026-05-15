//! Companion-file scan + pair integration (ADR-0043, slice C.2c).
//!
//! Walks a root, discovers companion files via
//! [`ab_companion::discover_companions`], decides pairing via
//! [`ab_companion::auto_pair`], and persists the outcome through
//! the [`ab_db::companions`] helpers — all in one pass.
//!
//! Sits in `ab-scan` rather than `ab-companion` so the latter
//! stays DB-free; `ab-scan` already owns the library-side
//! audio-scan loop and so the DB dep is already paid for.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ab_companion::{AudiobookCandidate, AutoPairResult, DiscoveredCompanion, auto_pair};
use ab_core::{BookId, Error, Result};
use ab_db::LibraryDb;
use ab_db::companions::{CompanionId, CompanionRecord, replace_nearby, set_pair, upsert_companion};

/// Outcome counts from one companion-scan run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CompanionScanReport {
    /// Files surfaced by [`ab_companion::discover_companions`]
    /// after the magic-byte filter (Unknown bytes already
    /// dropped).
    pub discovered: u64,
    /// Auto-paired to exactly one audiobook.
    pub paired: u64,
    /// Ambiguous (geometry hits multiple audiobooks); junction-
    /// hint rows landed in `companion_nearby_books`.
    pub ambiguous: u64,
    /// Unpaired (no audiobook subtree contains the companion).
    /// The row exists with `book_id = NULL`; no junction rows.
    pub orphan: u64,
}

/// Walk `root`, persist every discovered companion via
/// `ab_db::companions::upsert_companion`, then apply the
/// auto-pair decision for each (`set_pair` / `replace_nearby`).
///
/// `now` is the `discovered_at` timestamp the upsert uses for
/// new rows; it's also reused for the junction-hint rows in the
/// ambiguous branch so a re-scan with stable timestamps doesn't
/// fight the `StaleCompanionHintsTarget` cleanup.
///
/// # Errors
///
/// [`Error::Io`] if `root` can't be canonicalised,
/// [`Error::Database`] on any sqlx failure during persist,
/// any [`ab_companion::ScanError`] surfaces as
/// `Error::Io(...)` since the scanner already downgrades
/// per-file issues to warnings.
pub async fn scan_companions(
    root: &Path,
    library: &LibraryDb,
    now: i64,
) -> Result<CompanionScanReport> {
    if !root.exists() {
        return Err(Error::PathOutsideAllowed(root.to_path_buf()));
    }
    let canonical = std::fs::canonicalize(root).map_err(Error::Io)?;
    let audiobooks = load_audiobook_directories(library).await?;

    let discovered = match ab_companion::discover_companions(&canonical) {
        Ok(items) => items,
        Err(e) => {
            return Err(Error::Io(std::io::Error::other(e.to_string())));
        }
    };

    let mut report = CompanionScanReport {
        discovered: u64::try_from(discovered.len()).unwrap_or(0),
        ..CompanionScanReport::default()
    };

    for found in discovered {
        let cid = persist_one(library, &found, now).await?;
        match auto_pair(&found.path, &audiobooks) {
            AutoPairResult::Paired(book_id) => {
                set_pair(library.pool(), cid, book_id).await?;
                report.paired += 1;
            }
            AutoPairResult::Ambiguous(candidates) => {
                replace_nearby(library.pool(), cid, &candidates, now).await?;
                report.ambiguous += 1;
            }
            AutoPairResult::Unpaired => {
                report.orphan += 1;
            }
        }
    }
    Ok(report)
}

async fn persist_one(
    library: &LibraryDb,
    found: &DiscoveredCompanion,
    now: i64,
) -> Result<CompanionId> {
    let path_str = found.path.to_string_lossy();
    upsert_companion(
        library.pool(),
        CompanionRecord {
            path: path_str.as_ref(),
            format: found.format.as_str(),
            parse_tier: found.parse_tier.as_str(),
            content_hash: &found.content_hash,
            bytes: found.bytes,
            discovered_at: now,
        },
    )
    .await
}

/// Read the active `book_files` rows and group by parent
/// directory. One audiobook's multi-file rows share a parent
/// directory (the per-book grouping invariant the audio
/// scanner enforces); we collapse to one
/// [`AudiobookCandidate`] per `(book_id, parent_dir)` pair.
///
/// The geometry rule says "audiobook A claims companion C when
/// C's path is under A's directory subtree." So we hand the
/// candidate set the audiobook's directory, not the audio
/// file's path.
async fn load_audiobook_directories(
    library: &LibraryDb,
) -> Result<Vec<AudiobookCandidate<BookId>>> {
    let rows = sqlx::query!("SELECT book_id, file_path FROM book_files WHERE is_active = 1",)
        .fetch_all(library.pool())
        .await
        .map_err(|e| Error::Database(format!("load_audiobook_directories: {e}")))?;

    let mut by_book: HashMap<i64, PathBuf> = HashMap::new();
    for row in rows {
        let path = PathBuf::from(row.file_path);
        if let Some(parent) = path.parent() {
            // For multi-file books every row shares a parent;
            // first-write-wins is fine.
            by_book
                .entry(row.book_id)
                .or_insert_with(|| parent.to_path_buf());
        }
    }
    Ok(by_book
        .into_iter()
        .map(|(book_id, directory)| AudiobookCandidate {
            key: BookId(book_id),
            directory,
        })
        .collect())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use ab_core::tunables::DbTunables;
    use tempfile::TempDir;

    async fn fresh_library() -> (LibraryDb, TempDir) {
        let tmp = TempDir::new().expect("tmpdir");
        let library = LibraryDb::open(&tmp.path().join("library.db"), &DbTunables::default())
            .await
            .expect("open library");
        (library, tmp)
    }

    async fn seed_book(library: &LibraryDb, book_id: i64, file_path: &str) {
        sqlx::query("INSERT INTO books (book_id, title) VALUES (?, ?)")
            .bind(book_id)
            .bind(format!("Book {book_id}"))
            .execute(library.pool())
            .await
            .expect("seed book");
        sqlx::query(
            "INSERT INTO book_files \
                (file_id, book_id, file_path, content_hash, is_active) \
             VALUES (?, ?, ?, ?, 1)",
        )
        .bind(book_id * 100)
        .bind(book_id)
        .bind(file_path)
        .bind(format!("hash-{book_id}"))
        .execute(library.pool())
        .await
        .expect("seed book_files");
    }

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        use std::fs;
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(&path, bytes).expect("write");
        path
    }

    #[tokio::test]
    async fn pairs_companion_in_audiobook_subdir() {
        let tmp = TempDir::new().expect("tmpdir");
        let root = tmp.path();
        // One audiobook at root/book-a/; one companion next to it.
        write(root, "book-a/audio.m4b", b"\0\0\0\0ftypM4B ");
        write(root, "book-a/notes.pdf", b"%PDF-1.7 fake");
        let (library, _db_tmp) = fresh_library().await;
        let book_a_audio =
            std::fs::canonicalize(root.join("book-a/audio.m4b")).expect("canonicalise");
        seed_book(&library, 1, &book_a_audio.to_string_lossy()).await;

        let report = scan_companions(root, &library, 42).await.expect("scan");
        assert_eq!(report.discovered, 1);
        assert_eq!(report.paired, 1);
        assert_eq!(report.ambiguous, 0);
        assert_eq!(report.orphan, 0);
    }

    #[tokio::test]
    async fn orphan_when_no_audiobook_contains_companion() {
        let tmp = TempDir::new().expect("tmpdir");
        let root = tmp.path();
        write(root, "book-a/audio.m4b", b"\0\0\0\0ftypM4B ");
        write(root, "orphans/notes.pdf", b"%PDF-1.7 fake");
        let (library, _db_tmp) = fresh_library().await;
        let book_a_audio =
            std::fs::canonicalize(root.join("book-a/audio.m4b")).expect("canonicalise");
        seed_book(&library, 1, &book_a_audio.to_string_lossy()).await;

        let report = scan_companions(root, &library, 42).await.expect("scan");
        assert_eq!(report.discovered, 1);
        assert_eq!(report.paired, 0);
        assert_eq!(report.orphan, 1);
        // Row exists with NULL book_id.
        let row: Option<i64> = sqlx::query_scalar("SELECT book_id FROM book_companions LIMIT 1")
            .fetch_one(library.pool())
            .await
            .expect("read back");
        assert_eq!(row, None);
    }

    #[tokio::test]
    async fn ambiguous_records_nearby_when_two_books_claim_companion() {
        // Two audiobooks sharing the same parent dir. A companion
        // in the parent dir is auto-paired against the parent
        // dir's books only when the parent dir IS that audiobook's
        // canonical directory. Here we put the companion in
        // /root/notes.pdf and two audiobooks in /root/a/x.m4b
        // + /root/b/y.m4b — neither audiobook's directory is an
        // ancestor of /root/notes.pdf, so the result is Unpaired.
        //
        // To exercise the ambiguous branch we need TWO audiobook
        // dirs that both ARE ancestors of the companion. Easiest:
        // both audiobooks have their audio at /root level
        // (parent = root); a companion at /root/notes.pdf is then
        // inside both audiobook subtrees.
        let tmp = TempDir::new().expect("tmpdir");
        let root = tmp.path();
        write(root, "book-a.m4b", b"\0\0\0\0ftypM4B ");
        write(root, "book-b.m4b", b"\0\0\0\0ftypM4B ");
        write(root, "notes.pdf", b"%PDF-1.7 shared");
        let (library, _db_tmp) = fresh_library().await;
        let a_audio = std::fs::canonicalize(root.join("book-a.m4b")).expect("canonicalise a");
        let b_audio = std::fs::canonicalize(root.join("book-b.m4b")).expect("canonicalise b");
        seed_book(&library, 1, &a_audio.to_string_lossy()).await;
        seed_book(&library, 2, &b_audio.to_string_lossy()).await;

        let report = scan_companions(root, &library, 42).await.expect("scan");
        assert_eq!(report.discovered, 1);
        assert_eq!(report.ambiguous, 1);
        let hint_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM companion_nearby_books")
            .fetch_one(library.pool())
            .await
            .expect("count hints");
        assert_eq!(hint_count, 2, "one junction-hint per candidate book");
    }

    #[tokio::test]
    async fn rescan_is_idempotent_on_unchanged_tree() {
        let tmp = TempDir::new().expect("tmpdir");
        let root = tmp.path();
        write(root, "book-a/audio.m4b", b"\0\0\0\0ftypM4B ");
        write(root, "book-a/notes.pdf", b"%PDF-1.7 fake");
        let (library, _db_tmp) = fresh_library().await;
        let book_a_audio =
            std::fs::canonicalize(root.join("book-a/audio.m4b")).expect("canonicalise");
        seed_book(&library, 1, &book_a_audio.to_string_lossy()).await;

        let _ = scan_companions(root, &library, 42).await.expect("scan 1");
        let _ = scan_companions(root, &library, 50).await.expect("scan 2");
        let companion_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM book_companions")
            .fetch_one(library.pool())
            .await
            .expect("count");
        assert_eq!(companion_count, 1, "re-scan upserts in place");
    }
}
