//! `SafeUnzip` — ZIP-only extractor with defence-in-depth caps
//! (ADR-0047, slice B.21).
//!
//! Three risks make naive extraction unsafe; `SafeUnzip` refuses
//! every entry that violates one of these caps:
//!
//! * **Zip-slip.** Entries with `..` traversals canonicalise
//!   outside the target dir. We canonicalise + verify the parent
//!   prefix before opening any output file.
//! * **Zip-bomb.** Small archive, multi-GB decompressed. We cap
//!   per-entry bytes AND running cumulative bytes.
//! * **Entry-count `DoS`.** Millions of empty entries. We cap
//!   total entries before decompression starts.
//!
//! Also: symlinks rejected by default (RAR / TAR carry them;
//! ZIP can too via Unix attrs), absolute paths rejected, depth
//! beyond N nested dirs rejected.
//!
//! Scan integration (the auto-extract-during-scan path) lives in
//! its own slice. This crate stays the pure-Rust extractor +
//! tracking-table accessor.

#![forbid(unsafe_code)]
#![allow(missing_docs)] // scaffold; tightened in follow-up slices

use std::path::{Component, Path, PathBuf};

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

#[derive(Debug, Clone, Default, Serialize)]
pub struct ExtractReport {
    pub source_path: PathBuf,
    pub extracted_path: PathBuf,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub entries_count: u32,
    pub entries_extracted: u32,
    pub entries_skipped: u32,
    /// Reasons indexed by skipped-entry name. Operator-facing
    /// audit; bounded by `entries_count` so the size stays sane.
    pub skipped: Vec<SkippedEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkippedEntry {
    pub name: String,
    pub reason: SkipReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    AbsolutePath,
    ZipSlip,
    Symlink,
    DepthExceeded,
    PerEntryByteCap,
    TotalByteCap,
}

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("source ZIP not found: {0}")]
    NotFound(PathBuf),
    #[error("entries count {actual} exceeds cap {cap}")]
    TooManyEntries { actual: u32, cap: u32 },
    #[error("failed to open zip {path}: {reason}")]
    Open { path: PathBuf, reason: String },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("zip read error: {0}")]
    Zip(String),
    #[error("blake3 error: {0}")]
    Hash(String),
    #[error("database error: {0}")]
    Db(String),
}

impl From<zip::result::ZipError> for ArchiveError {
    fn from(e: zip::result::ZipError) -> Self {
        Self::Zip(e.to_string())
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
/// Synchronous std-IO (the `zip` crate is sync-only); call from a
/// `spawn_blocking` task in async contexts. The function is
/// idempotent at the call-site level: re-running with an unchanged
/// `target_dir` overwrites contents.
pub fn extract_safe(
    source_zip: &Path,
    target_dir: &Path,
    caps: &ArchiveTunables,
) -> Result<ExtractReport, ArchiveError> {
    if !source_zip.exists() {
        return Err(ArchiveError::NotFound(source_zip.to_path_buf()));
    }
    let bytes_in = std::fs::metadata(source_zip)?.len();
    std::fs::create_dir_all(target_dir)?;
    let canonical_target = std::fs::canonicalize(target_dir)?;

    let file = std::fs::File::open(source_zip).map_err(|e| ArchiveError::Open {
        path: source_zip.to_path_buf(),
        reason: e.to_string(),
    })?;
    let mut zip = zip::ZipArchive::new(file)?;

    let entries_count_u32 = u32::try_from(zip.len()).unwrap_or(u32::MAX);
    if entries_count_u32 > caps.max_entries {
        return Err(ArchiveError::TooManyEntries {
            actual: entries_count_u32,
            cap: caps.max_entries,
        });
    }

    let mut report = ExtractReport {
        source_path: source_zip.to_path_buf(),
        extracted_path: canonical_target.clone(),
        bytes_in,
        entries_count: entries_count_u32,
        ..Default::default()
    };

    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let raw_name = entry.name().to_owned();

        // ── path-shape checks ────────────────────────────
        let entry_path = Path::new(&raw_name);
        if entry_path.is_absolute() {
            push_skip(&mut report, raw_name, SkipReason::AbsolutePath);
            continue;
        }
        let depth = entry_path.components().count();
        if depth > caps.max_depth as usize {
            push_skip(&mut report, raw_name, SkipReason::DepthExceeded);
            continue;
        }
        if has_parent_component(entry_path) {
            push_skip(&mut report, raw_name, SkipReason::ZipSlip);
            continue;
        }
        if caps.forbid_symlinks && is_symlink_entry(&entry) {
            push_skip(&mut report, raw_name, SkipReason::Symlink);
            continue;
        }

        let candidate = canonical_target.join(entry_path);
        if !path_inside(&canonical_target, &candidate) {
            push_skip(&mut report, raw_name, SkipReason::ZipSlip);
            continue;
        }

        // ── byte caps ────────────────────────────────────
        let entry_size = entry.size();
        if entry_size > caps.max_per_entry_bytes {
            push_skip(&mut report, raw_name, SkipReason::PerEntryByteCap);
            continue;
        }
        if report.bytes_out.saturating_add(entry_size) > caps.max_decompressed_bytes {
            push_skip(&mut report, raw_name, SkipReason::TotalByteCap);
            continue;
        }

        // ── write ────────────────────────────────────────
        if entry.is_dir() {
            std::fs::create_dir_all(&candidate)?;
            report.entries_extracted = report.entries_extracted.saturating_add(1);
            continue;
        }
        if let Some(parent) = candidate.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&candidate)?;
        let written = std::io::copy(&mut entry, &mut out)?;
        report.bytes_out = report.bytes_out.saturating_add(written);
        report.entries_extracted = report.entries_extracted.saturating_add(1);
    }

    Ok(report)
}

fn push_skip(report: &mut ExtractReport, name: String, reason: SkipReason) {
    report.entries_skipped = report.entries_skipped.saturating_add(1);
    report.skipped.push(SkippedEntry { name, reason });
}

fn has_parent_component(p: &Path) -> bool {
    p.components()
        .any(|c| matches!(c, Component::ParentDir | Component::RootDir))
}

fn is_symlink_entry(entry: &zip::read::ZipFile<'_, std::fs::File>) -> bool {
    // Unix mode 0o12_0000 == symlink. The zip crate exposes the
    // attribute mask via `unix_mode`; missing modes default to
    // not-a-symlink, which is what we want for cross-platform
    // archives generated on Windows.
    entry
        .unix_mode()
        .is_some_and(|m| (m & 0o17_0000) == 0o12_0000)
}

fn path_inside(root: &Path, candidate: &Path) -> bool {
    // Canonicalise the candidate's parent (the file itself doesn't
    // exist yet); compare against the canonical root.
    let parent = candidate.parent().unwrap_or(candidate);
    std::fs::create_dir_all(parent).ok();
    std::fs::canonicalize(parent).is_ok_and(|c| c.starts_with(root))
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
        assert_eq!(report.entries_extracted, 3);
        assert!(target.join("book.m4b").exists());
        assert!(target.join("notes/readme.txt").exists());
    }

    #[test]
    fn rejects_zip_slip_entry() {
        let tmp = TempDir::new().expect("tempdir");
        let src = tmp.path().join("evil.zip");
        write_zip(&src, &[("normal.txt", b"ok"), ("../escape.txt", b"NO")]);
        let target = tmp.path().join("evil.extracted");
        let report = extract_safe(&src, &target, &ArchiveTunables::default()).expect("extract");
        assert_eq!(report.entries_extracted, 1);
        assert_eq!(report.entries_skipped, 1);
        assert!(matches!(report.skipped[0].reason, SkipReason::ZipSlip));
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
        assert!(matches!(err, ArchiveError::TooManyEntries { .. }));
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
        let report = extract_safe(&src, &target, &caps).expect("extract");
        // 1 KiB extracted; second entry would push past 1500.
        assert_eq!(report.entries_extracted, 1);
        assert!(
            report
                .skipped
                .iter()
                .any(|s| matches!(s.reason, SkipReason::TotalByteCap))
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
            entries_extracted: 3,
            entries_skipped: 0,
            skipped: vec![],
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
