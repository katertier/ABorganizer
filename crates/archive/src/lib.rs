//! ZIP archive extraction glue (ADR-0047, B.21 + follow-up code
//! swap 2026-05-15).
//!
//! `ab-archive` is the **thin glue layer** between ABorganizer's
//! scan/pipeline integration and the [`safe_unzip`](https://crates.io/crates/safe_unzip)
//! crate. The defence-in-depth posture (zip-slip / zip-bomb /
//! symlink / depth caps) is owned by `safe_unzip` upstream;
//! this crate owns only the ABorganizer-specific pieces:
//!
//! * [`ArchiveTunables`] — operator-configurable caps that map
//!   onto [`safe_unzip::Limits`] + [`safe_unzip::SymlinkPolicy`]
//!   at extractor-build time.
//! * [`extract_safe`] — the operator-facing extraction call;
//!   delegates to [`safe_unzip::Extractor`] and converts the
//!   upstream report into our [`ExtractReport`] shape.
//! * [`record_extract`] + [`recorded_hash`] — the
//!   `zip_archive_extracts` tracking table accessors that drive
//!   idempotent rescan.
//! * [`blake3_file`] — streamed BLAKE3 over the source ZIP for
//!   the rescan source-hash check.
//!
//! ## Semantics
//!
//! `safe_unzip` is **secure-by-default**: a violation (zip-slip,
//! over-size, too-many-entries, depth, symlink) aborts the entire
//! extraction with a typed [`safe_unzip::Error`]. Our
//! [`ArchiveError`] mirrors those variants so callers can pattern-
//! match without taking a direct dependency on `safe_unzip`'s
//! types.
//!
//! This is a deliberate tightening from the prior hand-rolled
//! extractor (which skipped bad entries and continued); leaving
//! a half-extracted directory after a security violation makes
//! recovery harder than refusing the archive outright. ADR-0047's
//! § Migration documents the shift.

#![forbid(unsafe_code)]
#![allow(missing_docs)] // scaffold; tightened in follow-up slices

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveTunables {
    /// Total decompressed bytes across all entries. Default 10 GB.
    pub max_decompressed_bytes: u64,
    /// Per-entry decompressed bytes. Default 2 GB.
    pub max_per_entry_bytes: u64,
    /// Maximum entry count. Default `50_000`.
    pub max_entries: u32,
    /// Maximum nested directory depth. Default 8.
    pub max_depth: u32,
    /// Reject symlink entries. Default true.
    pub forbid_symlinks: bool,
}

impl Default for ArchiveTunables {
    fn default() -> Self {
        Self {
            max_decompressed_bytes: 10 * 1024 * 1024 * 1024,
            max_per_entry_bytes: 2 * 1024 * 1024 * 1024,
            max_entries: 50_000,
            max_depth: 8,
            forbid_symlinks: true,
        }
    }
}

impl ArchiveTunables {
    /// Project these tunables onto a [`safe_unzip::Limits`].
    const fn to_limits(&self) -> safe_unzip::Limits {
        safe_unzip::Limits {
            max_total_bytes: self.max_decompressed_bytes,
            max_file_count: self.max_entries as usize,
            max_single_file: self.max_per_entry_bytes,
            max_path_depth: self.max_depth as usize,
        }
    }
}

/// Successful extraction report. Mirrors the
/// `zip_archive_extracts` row plus the upstream files/dirs split.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ExtractReport {
    pub source_path: PathBuf,
    pub extracted_path: PathBuf,
    pub bytes_in: u64,
    pub bytes_out: u64,
    /// `files_extracted + dirs_created` from `safe_unzip::Report`,
    /// stored as a single count for the tracking row.
    pub entries_count: u32,
    pub files_extracted: u32,
    pub dirs_created: u32,
}

/// All ABorganizer-facing failure modes during a ZIP extract.
///
/// Variants align with [`safe_unzip::Error`] but stay decoupled
/// from the upstream type so callers don't need to depend on
/// `safe_unzip` directly. New `safe_unzip` variants fold into
/// [`Self::Other`] until we promote them explicitly.
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("source ZIP not found: {0}")]
    NotFound(PathBuf),
    #[error("path '{entry}' escapes destination ({detail})")]
    PathEscape { entry: String, detail: String },
    #[error("symlink entry '{entry}' rejected (target '{target}')")]
    SymlinkRejected { entry: String, target: String },
    #[error("total decompressed bytes would be {would_be}, limit {limit}")]
    TotalSizeExceeded { limit: u64, would_be: u64 },
    #[error("file count would be {attempted}, limit {limit}")]
    FileCountExceeded { limit: usize, attempted: usize },
    #[error("entry '{entry}' size {size} exceeds per-entry limit {limit}")]
    FileTooLarge {
        entry: String,
        limit: u64,
        size: u64,
    },
    #[error("entry '{entry}' depth {depth} exceeds limit {limit}")]
    PathTooDeep {
        entry: String,
        depth: usize,
        limit: usize,
    },
    #[error("entry '{entry}' has invalid filename ({reason})")]
    InvalidFilename { entry: String, reason: String },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("zip read error: {0}")]
    Zip(String),
    #[error("blake3 error: {0}")]
    Hash(String),
    #[error("database error: {0}")]
    Db(String),
    #[error("safe_unzip error: {0}")]
    Other(String),
}

impl From<safe_unzip::Error> for ArchiveError {
    fn from(e: safe_unzip::Error) -> Self {
        use safe_unzip::Error as U;
        match e {
            U::PathEscape { entry, detail } => Self::PathEscape { entry, detail },
            U::SymlinkNotAllowed { entry, target } => Self::SymlinkRejected { entry, target },
            U::TotalSizeExceeded { limit, would_be } => Self::TotalSizeExceeded { limit, would_be },
            U::FileCountExceeded { limit, attempted } => {
                Self::FileCountExceeded { limit, attempted }
            }
            U::FileTooLarge { entry, limit, size } => Self::FileTooLarge { entry, limit, size },
            U::PathTooDeep {
                entry,
                depth,
                limit,
            } => Self::PathTooDeep {
                entry,
                depth,
                limit,
            },
            U::InvalidFilename { entry, reason } => Self::InvalidFilename { entry, reason },
            U::Io(io) => Self::Io(io),
            U::Zip(z) => Self::Zip(z.to_string()),
            other => Self::Other(other.to_string()),
        }
    }
}

impl From<sqlx::Error> for ArchiveError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e.to_string())
    }
}

/// Compute `BLAKE3` hash of a file. Streams through 64 `KiB`
/// chunks so multi-GB archives don't load into RAM.
pub fn blake3_file(path: &Path) -> Result<String, ArchiveError> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Extract a ZIP archive with the configured safety caps.
///
/// Synchronous std-IO (the upstream `safe_unzip::Extractor` is
/// sync). Call from a `spawn_blocking` task in async contexts.
///
/// A security violation (zip-slip, over-size, depth, symlink,
/// etc.) aborts the entire extraction with a typed
/// [`ArchiveError`]; nothing partial lands in `target_dir`'s
/// final state beyond what `safe_unzip` had committed before the
/// violation was detected. The caller is responsible for cleaning
/// up `target_dir` on Err if a redo is unwanted.
pub fn extract_safe(
    source_zip: &Path,
    target_dir: &Path,
    caps: &ArchiveTunables,
) -> Result<ExtractReport, ArchiveError> {
    if !source_zip.exists() {
        return Err(ArchiveError::NotFound(source_zip.to_path_buf()));
    }
    let bytes_in = std::fs::metadata(source_zip)?.len();

    let symlink_policy = if caps.forbid_symlinks {
        safe_unzip::SymlinkPolicy::Error
    } else {
        // `Skip` is the safe default — silently ignore symlink
        // entries instead of following them. The crate does not
        // expose a "follow" policy by design.
        safe_unzip::SymlinkPolicy::Skip
    };

    let report = safe_unzip::Extractor::new_or_create(target_dir)?
        .limits(caps.to_limits())
        .symlinks(symlink_policy)
        .overwrite(safe_unzip::OverwritePolicy::Overwrite)
        .extract_file(source_zip)?;

    let canonical_target = std::fs::canonicalize(target_dir)?;
    let files_extracted = u32::try_from(report.files_extracted).unwrap_or(u32::MAX);
    let dirs_created = u32::try_from(report.dirs_created).unwrap_or(u32::MAX);
    let entries_count = files_extracted.saturating_add(dirs_created);

    Ok(ExtractReport {
        source_path: source_zip.to_path_buf(),
        extracted_path: canonical_target,
        bytes_in,
        bytes_out: report.bytes_written,
        entries_count,
        files_extracted,
        dirs_created,
    })
}

// ── zip_archive_extracts persistence ──────────────────────────────

/// Record (or update) the extract row for a source ZIP.
pub async fn record_extract(
    pool: &SqlitePool,
    source_path: &Path,
    extracted_path: &Path,
    source_hash: &str,
    report: &ExtractReport,
) -> Result<i64, ArchiveError> {
    let source_path_str = source_path.to_string_lossy().into_owned();
    let extracted_path_str = extracted_path.to_string_lossy().into_owned();
    let bytes_in: i64 = i64::try_from(report.bytes_in).unwrap_or(i64::MAX);
    let bytes_out: i64 = i64::try_from(report.bytes_out).unwrap_or(i64::MAX);
    let entries_count: i64 = i64::from(report.entries_count);
    let id = sqlx::query!(
        "INSERT INTO zip_archive_extracts
            (source_path, extracted_path, source_hash, bytes_in, bytes_out, entries_count)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(source_path) DO UPDATE SET
            extracted_path = excluded.extracted_path,
            source_hash    = excluded.source_hash,
            bytes_in       = excluded.bytes_in,
            bytes_out      = excluded.bytes_out,
            entries_count  = excluded.entries_count,
            extracted_at   = strftime('%s','now')
         RETURNING archive_id",
        source_path_str,
        extracted_path_str,
        source_hash,
        bytes_in,
        bytes_out,
        entries_count,
    )
    .fetch_one(pool)
    .await?
    .archive_id;
    Ok(id)
}

/// Read the recorded source hash for a ZIP. Returns `None` if
/// the archive hasn't been extracted yet.
pub async fn recorded_hash(
    pool: &SqlitePool,
    source_path: &Path,
) -> Result<Option<String>, ArchiveError> {
    let source_path_str = source_path.to_string_lossy().into_owned();
    let row = sqlx::query!(
        r#"SELECT source_hash AS "hash!: String"
         FROM zip_archive_extracts
         WHERE source_path = ?"#,
        source_path_str,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.hash))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use ab_core::tunables::DbTunables;
    use ab_db::LibraryDb;
    use std::io::Write as _;
    use tempfile::TempDir;
    use zip::write::SimpleFileOptions;

    fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let file = std::fs::File::create(path).expect("create");
        let mut zip = zip::ZipWriter::new(file);
        let opts: SimpleFileOptions = SimpleFileOptions::default();
        for (name, data) in entries {
            zip.start_file(*name, opts).expect("start_file");
            zip.write_all(data).expect("write");
        }
        zip.finish().expect("finish");
    }

    async fn db() -> (TempDir, LibraryDb) {
        let dir = TempDir::new().expect("tempdir");
        let lib = LibraryDb::open(&dir.path().join("library.db"), &DbTunables::default())
            .await
            .expect("open");
        (dir, lib)
    }

    #[test]
    fn extracts_clean_archive() {
        let tmp = TempDir::new().expect("tempdir");
        let src = tmp.path().join("clean.zip");
        write_zip(
            &src,
            &[
                ("book.m4b", b"audio-bytes"),
                ("cover.jpg", b"jpeg-bytes"),
                ("notes/readme.txt", b"hello"),
            ],
        );
        let target = tmp.path().join("clean.extracted");
        let report = extract_safe(&src, &target, &ArchiveTunables::default()).expect("extract");
        assert!(report.files_extracted >= 3);
        assert!(target.join("book.m4b").exists());
        assert!(target.join("notes/readme.txt").exists());
    }

    #[test]
    fn rejects_zip_slip_entry() {
        let tmp = TempDir::new().expect("tempdir");
        let src = tmp.path().join("evil.zip");
        write_zip(&src, &[("normal.txt", b"ok"), ("../escape.txt", b"NO")]);
        let target = tmp.path().join("evil.extracted");
        let err = extract_safe(&src, &target, &ArchiveTunables::default())
            .expect_err("path-escape rejected");
        assert!(
            matches!(err, ArchiveError::PathEscape { .. }),
            "expected PathEscape, got {err:?}"
        );
        // Nothing extracted outside the target.
        assert!(!tmp.path().join("escape.txt").exists());
    }

    #[test]
    fn rejects_too_many_entries() {
        let tmp = TempDir::new().expect("tempdir");
        let src = tmp.path().join("many.zip");
        let entries: Vec<(String, Vec<u8>)> = (0..20)
            .map(|i| (format!("f{i:03}.txt"), b"x".to_vec()))
            .collect();
        let entries_ref: Vec<(&str, &[u8])> = entries
            .iter()
            .map(|(n, d)| (n.as_str(), d.as_slice()))
            .collect();
        write_zip(&src, &entries_ref);
        let target = tmp.path().join("many.extracted");
        let caps = ArchiveTunables {
            max_entries: 5,
            ..ArchiveTunables::default()
        };
        let err = extract_safe(&src, &target, &caps).expect_err("cap");
        assert!(matches!(err, ArchiveError::FileCountExceeded { .. }));
    }

    #[test]
    fn caps_total_bytes() {
        let tmp = TempDir::new().expect("tempdir");
        let src = tmp.path().join("big.zip");
        write_zip(
            &src,
            &[
                ("a.bin", &vec![0u8; 1024]),
                ("b.bin", &vec![0u8; 1024]),
                ("c.bin", &vec![0u8; 1024]),
            ],
        );
        let target = tmp.path().join("big.extracted");
        let caps = ArchiveTunables {
            max_decompressed_bytes: 1500,
            ..ArchiveTunables::default()
        };
        let err = extract_safe(&src, &target, &caps).expect_err("cap");
        assert!(
            matches!(err, ArchiveError::TotalSizeExceeded { .. }),
            "expected TotalSizeExceeded, got {err:?}"
        );
    }

    #[test]
    fn blake3_file_round_trips() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("hash.bin");
        std::fs::write(&path, b"hello world").expect("write");
        let hex = blake3_file(&path).expect("hash");
        assert_eq!(hex.len(), 64);
    }

    #[tokio::test]
    async fn record_extract_upserts() {
        let (_dir, db) = db().await;
        let report = ExtractReport {
            source_path: Path::new("/x.zip").into(),
            extracted_path: Path::new("/x.extracted").into(),
            bytes_in: 100,
            bytes_out: 200,
            entries_count: 3,
            files_extracted: 3,
            dirs_created: 0,
        };
        let id1 = record_extract(
            db.pool(),
            Path::new("/x.zip"),
            Path::new("/x.extracted"),
            "deadbeef",
            &report,
        )
        .await
        .expect("record");
        let id2 = record_extract(
            db.pool(),
            Path::new("/x.zip"),
            Path::new("/x.extracted2"),
            "feedface",
            &report,
        )
        .await
        .expect("upsert");
        // Same row; upsert refreshes the hash.
        assert_eq!(id1, id2);
        let hash = recorded_hash(db.pool(), Path::new("/x.zip"))
            .await
            .expect("read")
            .expect("present");
        assert_eq!(hash, "feedface");
    }
}
