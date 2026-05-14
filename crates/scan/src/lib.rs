//! Directory walker + audio-file enumeration.
//!
//! # Pipeline placement
//!
//! Scan is the producer in the pipeline. It walks a directory tree
//! and emits `BookId`s. Downstream stages (`tag-read`, `fingerprint`,
//! `audiologo`, `commit`) consume those `BookId`s via the scheduler.
//!
//! # Slice 1D behaviour
//!
//! 1. **Multi-file book detection** — a directory containing ≥2
//!    audio files of the same extension is treated as one book with
//!    multiple `book_files` rows (typical multi-CD m4b/mp3 rip).
//!    Mixed-extension directories are still file-per-book.
//! 2. **`file_hash`** — blake3 over (size + mtime + first 4KB) is
//!    cheap and uniquely identifies a file even after a path move.
//!    Re-scan after `mv` updates the `file_path` column on the
//!    existing `book_files` row instead of inserting a duplicate
//!    book.
//! 3. **`UNIQUE(file_path)` is no longer the sole identity** — hash
//!    is. Path-only matches still no-op (idempotent re-scan).
//!
//! # What's still NOT here
//!
//! - File probe (duration / bitrate / codec) — `tag-read` stage.
//! - Catalog enrichment — `enrich` stage (Theme 2).
//! - Audiologo trim — Theme 3.
//! - Watching for filesystem changes — daemon.rs has the watch
//!   primitives reserved; wire-up is in Theme 6.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use walkdir::WalkDir;

use ab_core::{BookId, Error, Result};
use ab_db::LibraryDb;

/// Audio file extensions recognised by the scanner. Matched
/// case-insensitively.
pub const AUDIO_EXTENSIONS: &[&str] = &["m4b", "m4a", "mp3", "flac", "opus", "ogg", "aax"];

/// Compile `PipelineTunables.scan_excludes` patterns into a matcher.
///
/// Suitable for [`scan_with_excludes`] / [`scan`] (B.4, tracker
/// #119). Patterns that fail to compile are logged + dropped from
/// the active set — a single broken entry doesn't disable
/// exclusions for the rest. Returns an empty `GlobSet` when
/// `patterns` is empty (`is_match` always returns `false`). The
/// caller can defer compilation to boot time and pass the same
/// `GlobSet` to every scan.
#[must_use]
pub fn compile_excludes(patterns: &[String]) -> GlobSet {
    let mut builder = GlobSetBuilder::new();
    for raw in patterns {
        match Glob::new(raw) {
            Ok(glob) => {
                builder.add(glob);
            }
            Err(e) => tracing::warn!(
                pattern = %raw,
                error = %e,
                "scan.exclude_pattern_invalid"
            ),
        }
    }
    builder.build().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "scan.exclude_globset_build_failed");
        GlobSet::empty()
    })
}

/// True iff any component of `path` matches `globset` — file
/// basename OR any directory component. Globset patterns like
/// `*.tmp` match basenames; bare names like `temp` or `sample`
/// match directory components.
///
/// Matching by component (not whole path) keeps patterns short
/// and intent-clear: an operator who excludes `temp` doesn't
/// have to write `**/temp/**`.
fn path_matches_excludes(path: &Path, globset: &GlobSet) -> bool {
    if globset.is_empty() {
        return false;
    }
    for component in path.components() {
        let Component::Normal(os) = component else {
            continue;
        };
        if globset.is_match(os) {
            return true;
        }
    }
    false
}

/// First N bytes of a file fed into the file-hash. Tiny payload (one
/// disk page), enough to disambiguate non-content-identical files
/// without reading the whole audio.
const HASH_HEAD_BYTES: usize = 4096;

/// Summary of one `scan` invocation.
#[derive(Debug, Clone, Default)]
pub struct ScanReport {
    /// `BookId`s for newly-inserted books.
    pub new_book_ids: Vec<BookId>,
    /// File paths skipped because they already exist (same path or
    /// same hash with a recognised path).
    pub skipped_paths: Vec<PathBuf>,
    /// `BookId`s whose `book_files.file_path` was updated because
    /// the same file (same hash) reappeared at a different path.
    pub moved_book_ids: Vec<BookId>,
    /// File paths walked but rejected as non-audio.
    pub non_audio_count: u64,
    /// File paths walked total (audio + non-audio).
    pub total_walked: u64,
}

/// Walk `root` recursively and persist audio files as books.
///
/// Audio files are grouped by parent directory; each group becomes
/// one book (single-file or multi-file). Files previously seen — by
/// path or by `file_hash` — don't double-insert; a moved file gets
/// its `file_path` updated in place.
///
/// Convenience wrapper around [`scan_with_excludes`] with an
/// empty exclude set — kept for tests + simple callers.
///
/// # Errors
///
/// Returns [`Error::Io`] on FS errors,
/// [`Error::Database`] on SQL failures,
/// [`Error::PathOutsideAllowed`] if `root` doesn't exist.
pub async fn scan(root: &Path, db: &LibraryDb) -> Result<ScanReport> {
    scan_with_excludes(root, db, &GlobSet::empty()).await
}

/// `scan()` with watch-folder exclusion globs applied during the
/// walk (B.4). Build the `GlobSet` once at boot via
/// [`compile_excludes`] and pass on every scan.
///
/// Excluded paths short-circuit before any `is_audio_file` test
/// and never enter `book_files`. Matches by file basename OR any
/// path component — `*.tmp` skips the file, `temp` skips every
/// path with `temp` in any directory level.
///
/// # Errors
///
/// Same error surface as [`scan`].
pub async fn scan_with_excludes(
    root: &Path,
    db: &LibraryDb,
    excludes: &GlobSet,
) -> Result<ScanReport> {
    if !root.exists() {
        return Err(Error::PathOutsideAllowed(root.to_path_buf()));
    }
    let canonical_root = std::fs::canonicalize(root)?;
    tracing::info!(root = %canonical_root.display(), "scan.start");

    let mut report = ScanReport::default();

    // Phase 1: walk, count, collect audio files grouped by parent.
    let groups = walk_audio_files(&canonical_root, &mut report, excludes);

    // Phase 2: per group, decide single-file vs multi-file then upsert.
    for (parent_dir, audio_files) in groups {
        if let Err(e) = process_group(db, &parent_dir, &audio_files, &mut report).await {
            tracing::warn!(dir = %parent_dir.display(), error = %e, "scan.group_failed");
        }
    }

    tracing::info!(
        new = report.new_book_ids.len(),
        moved = report.moved_book_ids.len(),
        skipped = report.skipped_paths.len(),
        non_audio = report.non_audio_count,
        total = report.total_walked,
        "scan.complete"
    );
    Ok(report)
}

/// Walk `root` and return audio files grouped by their parent
/// directory. Mutates `report` in place to count
/// non-audio + walked counters. Each group is the list of
/// (path, metadata) pairs.
///
/// B.4: `excludes` is consulted before the `is_audio_file` test;
/// a path whose basename or any directory component matches a
/// pattern is skipped entirely (no `total_walked` / `non_audio`
/// increment, no tracing — the operator chose to ignore it).
fn walk_audio_files(
    root: &Path,
    report: &mut ScanReport,
    excludes: &GlobSet,
) -> BTreeMap<PathBuf, Vec<(PathBuf, std::fs::Metadata)>> {
    let mut groups: BTreeMap<PathBuf, Vec<(PathBuf, std::fs::Metadata)>> = BTreeMap::new();
    for entry in WalkDir::new(root).follow_links(false) {
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
        let path = entry.path();
        if path_matches_excludes(path, excludes) {
            tracing::trace!(file = %path.display(), "scan.exclude_match");
            continue;
        }
        report.total_walked += 1;
        if !is_audio_file(path) {
            report.non_audio_count += 1;
            tracing::debug!(file = %path.display(), "scan.non_audio_skipped");
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            tracing::warn!(file = %path.display(), "scan.metadata_failed");
            continue;
        };
        let parent = path
            .parent()
            .map_or_else(|| PathBuf::from("/"), Path::to_path_buf);
        groups
            .entry(parent)
            .or_default()
            .push((path.to_path_buf(), metadata));
    }
    groups
}

/// Decide whether `audio_files` is a single multi-file book or a
/// set of independent single-file books. Multi-file requires:
///   * ≥ 2 files
///   * All files share the same extension
///
/// Multi-format dirs (e.g. m4b + companion mp3 sample) are treated
/// as independent books to be safe — a wrong group is harder to
/// undo than two correctly-separate ones.
async fn process_group(
    db: &LibraryDb,
    parent_dir: &Path,
    audio_files: &[(PathBuf, std::fs::Metadata)],
    report: &mut ScanReport,
) -> Result<()> {
    if audio_files.len() >= 2 && same_extension(audio_files) {
        process_multi_file_book(db, parent_dir, audio_files, report).await
    } else {
        for (path, meta) in audio_files {
            process_single_file(db, path, meta, report).await?;
        }
        Ok(())
    }
}

fn same_extension(files: &[(PathBuf, std::fs::Metadata)]) -> bool {
    let mut iter = files.iter().filter_map(|(p, _)| {
        p.extension()
            .and_then(|e| e.to_str())
            .map(str::to_lowercase)
    });
    let Some(first) = iter.next() else {
        return false;
    };
    iter.all(|e| e == first)
}

/// Process one folder containing N audio files as a single book.
/// Book title = parent dir name.
async fn process_multi_file_book(
    db: &LibraryDb,
    parent_dir: &Path,
    audio_files: &[(PathBuf, std::fs::Metadata)],
    report: &mut ScanReport,
) -> Result<()> {
    let title = parent_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("Untitled")
        .to_owned();

    // First file: insert the book row (if needed); subsequent files
    // get appended to the same book. If the first file already
    // exists (by hash), we reuse its book_id.
    let mut book_id: Option<BookId> = None;
    for (idx, (path, meta)) in audio_files.iter().enumerate() {
        let row = NewBookFileRow {
            title: book_id.map_or(&title, |_| ""),
            file_path: path,
            metadata: meta,
        };
        match upsert_book_file(db, &row, book_id).await? {
            UpsertOutcome::Inserted(id) => {
                if idx == 0 {
                    report.new_book_ids.push(id);
                }
                book_id = Some(id);
            }
            UpsertOutcome::PathKnown(id) => {
                book_id = Some(id);
                report.skipped_paths.push(path.clone());
            }
            UpsertOutcome::Moved(id) => {
                book_id = Some(id);
                report.moved_book_ids.push(id);
            }
        }
    }
    Ok(())
}

async fn process_single_file(
    db: &LibraryDb,
    path: &Path,
    meta: &std::fs::Metadata,
    report: &mut ScanReport,
) -> Result<()> {
    let title = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Untitled")
        .to_owned();
    let row = NewBookFileRow {
        title: &title,
        file_path: path,
        metadata: meta,
    };
    match upsert_book_file(db, &row, None).await? {
        UpsertOutcome::Inserted(book_id) => report.new_book_ids.push(book_id),
        UpsertOutcome::PathKnown(_) => report.skipped_paths.push(path.to_path_buf()),
        UpsertOutcome::Moved(book_id) => report.moved_book_ids.push(book_id),
    }
    Ok(())
}

/// True when `path`'s extension is in [`AUDIO_EXTENSIONS`].
pub fn is_audio_file(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    let lower = ext.to_lowercase();
    AUDIO_EXTENSIONS.iter().any(|allowed| *allowed == lower)
}

/// Compute blake3 over (size + mtime + first 4KB). Cheap (single
/// disk seek + tiny hash) and unique enough to distinguish files
/// in practice. Returns lowercase hex.
pub fn compute_file_hash(path: &Path) -> std::io::Result<String> {
    use std::io::Read;

    let meta = std::fs::metadata(path)?;
    let size = meta.len();
    let mtime_secs = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0_u64, |d| d.as_secs());

    let mut head = vec![0_u8; HASH_HEAD_BYTES];
    let mut f = std::fs::File::open(path)?;
    let n = f.read(&mut head)?;
    head.truncate(n);

    let mut hasher = blake3::Hasher::new();
    hasher.update(&size.to_le_bytes());
    hasher.update(&mtime_secs.to_le_bytes());
    hasher.update(&head);
    Ok(String::from(hasher.finalize().to_hex().as_str()))
}

// ── Upsert path ──────────────────────────────────────────────────

/// Single-row input for upsert.
struct NewBookFileRow<'a> {
    /// Title to use IF a new book row is created. Ignored when
    /// appending to an existing book.
    title: &'a str,
    file_path: &'a Path,
    metadata: &'a std::fs::Metadata,
}

/// What `upsert_book_file` did.
enum UpsertOutcome {
    /// New `book_files` row inserted. Returns the `book_id` (new or
    /// existing depending on the caller's `book_id_hint`).
    Inserted(BookId),
    /// `file_path` was already known; no change.
    PathKnown(BookId),
    /// File matched an existing row by `file_hash` but at a
    /// different path. The row was updated to the new path.
    Moved(BookId),
}

/// Upsert a file. If `book_id_hint` is `Some`, the file is attached
/// to that existing book (multi-file branch); otherwise a new book
/// is created.
async fn upsert_book_file(
    db: &LibraryDb,
    row: &NewBookFileRow<'_>,
    book_id_hint: Option<BookId>,
) -> Result<UpsertOutcome> {
    let file_path_str: String = row.file_path.to_string_lossy().into_owned();
    let file_size = i64::try_from(row.metadata.len()).unwrap_or(i64::MAX);
    let modified_at = row
        .metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    let format = row
        .file_path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase);
    let file_hash = compute_file_hash(row.file_path).ok();

    let mut tx = db
        .pool()
        .begin()
        .await
        .map_err(|e| Error::Database(format!("begin tx: {e}")))?;

    // Path-known shortcut.
    let existing_by_path: Option<i64> = sqlx::query_scalar!(
        "SELECT book_id FROM book_files WHERE file_path = ? LIMIT 1",
        file_path_str
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| Error::Database(format!("check path: {e}")))?;
    if let Some(bid) = existing_by_path {
        return Ok(UpsertOutcome::PathKnown(BookId(bid)));
    }

    // Hash-known: existing row at different path → update the path.
    if let Some(hash) = &file_hash {
        let existing_by_hash = sqlx::query!(
            "SELECT file_id, book_id FROM book_files WHERE file_hash = ? LIMIT 1",
            hash
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| Error::Database(format!("check hash: {e}")))?;
        if let Some(row_match) = existing_by_hash {
            let file_id = row_match.file_id;
            let book_id = row_match.book_id;
            sqlx::query!(
                "UPDATE book_files SET file_path = ?, modified_at = ?, file_size = ?, \
                                       checked_at = strftime('%s','now') \
                 WHERE file_id = ?",
                file_path_str,
                modified_at,
                file_size,
                file_id,
            )
            .execute(&mut *tx)
            .await
            .map_err(|e| Error::Database(format!("update moved: {e}")))?;
            tx.commit()
                .await
                .map_err(|e| Error::Database(format!("commit: {e}")))?;
            return Ok(UpsertOutcome::Moved(BookId(book_id)));
        }
    }

    // Fresh insert. Reuse caller-supplied book_id (multi-file
    // append) or create a new one (single-file or first-of-multi).
    let book_id: i64 = if let Some(BookId(b)) = book_id_hint {
        b
    } else {
        let title = row.title;
        sqlx::query_scalar!(
            "INSERT INTO books (title) VALUES (?) RETURNING book_id",
            title
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| Error::Database(format!("insert book: {e}")))?
    };

    let format_str = format.as_deref();
    let file_hash_str = file_hash.as_deref();
    sqlx::query!(
        "INSERT INTO book_files \
             (book_id, file_path, file_size, modified_at, format, file_hash, is_active) \
         VALUES (?, ?, ?, ?, ?, ?, 1)",
        book_id,
        file_path_str,
        file_size,
        modified_at,
        format_str,
        file_hash_str,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| Error::Database(format!("insert book_file: {e}")))?;

    tx.commit()
        .await
        .map_err(|e| Error::Database(format!("commit: {e}")))?;

    Ok(UpsertOutcome::Inserted(BookId(book_id)))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::fs;

    use ab_core::tunables::DbTunables;
    use tempfile::TempDir;

    use super::*;

    fn touch(dir: &Path, name: &str, contents: &[u8]) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, contents).expect("write fixture file");
        p
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
    async fn scan_inserts_one_book_per_audio_file_when_extensions_mix() {
        let tmp = TempDir::new().expect("tmpdir");
        let db = fresh_db(tmp.path()).await;
        let lib = tmp.path().join("lib");
        fs::create_dir_all(&lib).expect("mkdir lib");

        // Different extensions → two single-file books. Distinct
        // content so the per-file `file_hash` doesn't collapse them
        // into a single moved-file row.
        touch(&lib, "a.m4b", b"this-is-book-a");
        touch(&lib, "b.mp3", b"this-is-book-b");
        touch(&lib, "notes.txt", b"junk");

        let r = scan(&lib, &db).await.expect("scan");
        assert_eq!(r.new_book_ids.len(), 2);
        assert_eq!(r.non_audio_count, 1);
    }

    #[tokio::test]
    async fn scan_groups_multi_file_book_when_same_extension() {
        let tmp = TempDir::new().expect("tmpdir");
        let db = fresh_db(tmp.path()).await;
        let book_dir = tmp.path().join("Author - Title");
        fs::create_dir_all(&book_dir).expect("mkdir book");
        touch(&book_dir, "01 - Part One.mp3", b"chunk-one-content");
        touch(&book_dir, "02 - Part Two.mp3", b"chunk-two-different");
        touch(&book_dir, "03 - Part Three.mp3", b"chunk-three-also-unique");

        let r = scan(&book_dir, &db).await.expect("scan");
        // One book with three files.
        assert_eq!(r.new_book_ids.len(), 1);

        // Verify book_files row count. Test queries stay runtime-
        // checked: trivial COUNT(*) doesn't benefit from compile-
        // time validation and `cargo sqlx prepare` doesn't reach
        // test code cleanly.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM book_files")
            .fetch_one(db.pool())
            .await
            .expect("count");
        assert_eq!(count, 3);

        let book_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM books")
            .fetch_one(db.pool())
            .await
            .expect("count");
        assert_eq!(book_count, 1);
    }

    #[tokio::test]
    async fn rescan_after_path_move_updates_existing_row() {
        let tmp = TempDir::new().expect("tmpdir");
        let db = fresh_db(tmp.path()).await;
        let lib_a = tmp.path().join("a");
        let lib_b = tmp.path().join("b");
        fs::create_dir_all(&lib_a).expect("mkdir a");
        fs::create_dir_all(&lib_b).expect("mkdir b");

        let original = touch(&lib_a, "book.m4b", b"this is the content header");
        let r1 = scan(&lib_a, &db).await.expect("scan a");
        assert_eq!(r1.new_book_ids.len(), 1);

        // Move the file to a new dir; rename in the process.
        let moved = lib_b.join("relocated.m4b");
        fs::rename(&original, &moved).expect("rename");

        let r2 = scan(&lib_b, &db).await.expect("scan b");
        assert_eq!(r2.new_book_ids.len(), 0, "no new books");
        assert_eq!(r2.moved_book_ids.len(), 1, "one moved row");

        let books_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM books")
            .fetch_one(db.pool())
            .await
            .expect("count books");
        assert_eq!(books_count, 1, "still exactly one book after the move");
    }

    #[tokio::test]
    async fn missing_root_returns_path_error() {
        let tmp = TempDir::new().expect("tmpdir");
        let db = fresh_db(tmp.path()).await;
        let missing = tmp.path().join("nope");
        let err = scan(&missing, &db).await.expect_err("should fail");
        assert!(matches!(err, Error::PathOutsideAllowed(_)));
    }

    #[test]
    fn file_hash_is_deterministic_for_same_content() {
        let tmp = TempDir::new().expect("tmpdir");
        let p1 = touch(tmp.path(), "a.bin", b"hello world");
        let h1 = compute_file_hash(&p1).expect("hash 1");
        let h2 = compute_file_hash(&p1).expect("hash 2 same file");
        assert_eq!(h1, h2);
    }

    #[test]
    fn file_hash_differs_when_size_differs() {
        let tmp = TempDir::new().expect("tmpdir");
        let a = touch(tmp.path(), "a.bin", b"hello world");
        let b = touch(tmp.path(), "b.bin", b"hello worldd");
        assert_ne!(
            compute_file_hash(&a).unwrap(),
            compute_file_hash(&b).unwrap()
        );
    }

    // ---- B.4 — watch-folder exclusion globs ----

    #[test]
    fn compile_excludes_drops_bad_patterns() {
        // `[` is an unterminated character class — globset rejects it.
        // Valid neighbours stay in the set.
        let set = compile_excludes(&[
            "*.tmp".into(),
            "[".into(),
            ".DS_Store".into(),
        ]);
        assert_eq!(set.len(), 2, "two valid patterns survived");
    }

    #[test]
    fn path_matches_excludes_by_basename_or_component() {
        let set = compile_excludes(&[
            "*.tmp".into(),
            ".DS_Store".into(),
            "temp".into(),
        ]);
        // Basename glob.
        assert!(path_matches_excludes(
            &PathBuf::from("/a/b/in-progress.tmp"),
            &set
        ));
        // Exact basename match.
        assert!(path_matches_excludes(
            &PathBuf::from("/a/b/.DS_Store"),
            &set
        ));
        // Directory-component match anywhere in the path.
        assert!(path_matches_excludes(
            &PathBuf::from("/library/temp/book.m4b"),
            &set
        ));
        assert!(path_matches_excludes(
            &PathBuf::from("/library/author/temp/book.m4b"),
            &set
        ));
        // No match.
        assert!(!path_matches_excludes(
            &PathBuf::from("/library/author/book.m4b"),
            &set
        ));
    }

    #[test]
    fn path_matches_excludes_empty_set_never_matches() {
        let set = GlobSet::empty();
        assert!(!path_matches_excludes(
            &PathBuf::from("/anything.tmp"),
            &set
        ));
    }

    #[tokio::test]
    async fn scan_with_excludes_skips_matching_files() {
        let tmp = TempDir::new().expect("tmpdir");
        let db = fresh_db(tmp.path()).await;
        let root = tmp.path().join("library");
        fs::create_dir_all(&root).expect("mkdir");
        // Three audio files: one normal, one excluded by extension
        // (.part-style suffix simulated via `.crdownload`), one
        // excluded by parent directory (`temp/`).
        touch(&root, "book.m4b", b"audio bytes 0");
        let temp_dir = root.join("temp");
        fs::create_dir(&temp_dir).expect("mkdir temp");
        touch(&temp_dir, "wip.m4b", b"audio bytes 1");
        touch(&root, "download.crdownload", b"audio bytes 2");

        let excludes = compile_excludes(&["*.crdownload".into(), "temp".into()]);
        let report = scan_with_excludes(&root, &db, &excludes)
            .await
            .expect("scan");
        assert_eq!(
            report.new_book_ids.len(),
            1,
            "only the non-excluded audio file becomes a book"
        );
        // total_walked excludes the skipped files entirely.
        assert_eq!(report.total_walked, 1);
    }
}
