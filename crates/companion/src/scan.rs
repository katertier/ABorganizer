//! Filesystem walk for companion-file discovery (ADR-0043).
//!
//! Sits above the byte-level [`crate::detect_format`] +
//! [`crate::is_companion_extension`] primitives. Given a root
//! directory the scanner produces a `Vec<DiscoveredCompanion>`
//! suitable for handing to the DB layer (`ab_db::companions::
//! upsert_companion`) + the geometry helper
//! ([`crate::auto_pair`]).
//!
//! Behaviour:
//!
//! 1. Walk the root with `walkdir::WalkDir` (`follow_links = false`
//!    by default; the C.2c integration slice flips this from a
//!    tunable if operators ask).
//! 2. For each non-directory entry:
//!    - Skip if the extension is in the audio set (the audio
//!      scan owns those).
//!    - Skip if the extension is plain text / markdown / dotfile
//!      (README / LICENSE / notes — explicit non-companions).
//!    - If the extension is in [`is_companion_extension`]'s
//!      known set, read the first 512 bytes; run
//!      [`detect_format`]; compute BLAKE3.
//!    - If the format is `Unknown`, the entry is dropped (we
//!      don't surface random sidecar files like `.nfo` /
//!      `.ds_store`).
//! 3. Return one record per discovered companion. The caller
//!    is responsible for transactional upserts + the auto-pair
//!    geometry decision; the scanner itself is purely declarative.

use std::path::{Path, PathBuf};

use crate::{CompanionFormat, ParseTier, detect_format, is_companion_extension, parse_tier_for};

/// One companion as discovered on disk. The caller turns this
/// into an `ab_db::companions::CompanionRecord` plus, optionally,
/// a pairing decision via [`crate::auto_pair`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredCompanion {
    /// Absolute (canonicalised) path on disk.
    pub path: PathBuf,
    /// Format from magic-byte dispatch. May differ from the
    /// extension; bytes win.
    pub format: CompanionFormat,
    /// Parse tier from [`parse_tier_for`].
    pub parse_tier: ParseTier,
    /// File size in bytes.
    pub bytes: i64,
    /// BLAKE3 hex of the full file contents.
    pub content_hash: String,
}

/// Typed scanner failure.
#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    /// `walkdir` failed to enumerate the tree (permissions,
    /// I/O).
    #[error("scan walk: {0}")]
    Walk(String),
    /// Per-entry I/O failure when reading bytes / probing
    /// metadata. `path` identifies the offending file; `message`
    /// is the underlying I/O error rendered as a string.
    #[error("scan read {path:?}: {message}")]
    Read {
        /// The offending path.
        path: PathBuf,
        /// Underlying I/O error message.
        message: String,
    },
}

/// Read-prefix size used by [`detect_format`].
///
/// The first 512 bytes capture every magic-byte signature
/// ADR-0043 cares about (MOBI's offset-60 marker is the deepest
/// at byte 68; tar `ustar` lives at byte 257..262; everything
/// else is in the first 256 bytes).
pub const DETECT_PREFIX_BYTES: usize = 512;

/// Walk `root` and return every recognised companion.
///
/// Synchronous I/O — the scanner is invoked from a `tokio::task::
/// spawn_blocking` in production. Pure for testability: handed
/// the same tree it produces the same result.
///
/// # Errors
///
/// [`ScanError::Walk`] if `walkdir` can't enumerate (permissions,
/// I/O at the directory level). Per-file errors are downgraded:
/// the offending file is skipped + a warning logged, the scan
/// continues. Surfacing every per-file error would make the
/// scanner brittle on libraries with even one bad file.
pub fn discover_companions(root: &Path) -> Result<Vec<DiscoveredCompanion>, ScanError> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(|r| match r {
            Ok(e) => Some(e),
            Err(e) => {
                tracing::warn!(error = %e, "companion.scan.walk_skip");
                None
            }
        })
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let Some(ext_os) = path.extension() else {
            continue;
        };
        let Some(ext) = ext_os.to_str() else {
            continue;
        };
        let ext_lower = ext.to_ascii_lowercase();
        if is_companion_extension(&ext_lower).is_none() {
            continue;
        }
        match probe_one(path) {
            Ok(Some(rec)) => out.push(rec),
            Ok(None) => {} // detected as Unknown after byte read — drop.
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "companion.scan.probe_skip");
            }
        }
    }
    Ok(out)
}

fn probe_one(path: &Path) -> Result<Option<DiscoveredCompanion>, ScanError> {
    let bytes_total = std::fs::metadata(path)
        .map_err(|e| ScanError::Read {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?
        .len();
    let bytes_total = i64::try_from(bytes_total).unwrap_or(i64::MAX);

    let prefix = read_prefix(path, DETECT_PREFIX_BYTES)?;
    let format = detect_format(&prefix);
    if matches!(format, CompanionFormat::Unknown) {
        return Ok(None);
    }
    let parse_tier = parse_tier_for(format);
    let content_hash = blake3_hex_of_file(path)?;
    Ok(Some(DiscoveredCompanion {
        path: path.to_path_buf(),
        format,
        parse_tier,
        bytes: bytes_total,
        content_hash,
    }))
}

fn read_prefix(path: &Path, n: usize) -> Result<Vec<u8>, ScanError> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path).map_err(|e| ScanError::Read {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    let mut buf = vec![0u8; n];
    let read = file.read(&mut buf).map_err(|e| ScanError::Read {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    buf.truncate(read);
    Ok(buf)
}

fn blake3_hex_of_file(path: &Path) -> Result<String, ScanError> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path).map_err(|e| ScanError::Read {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| ScanError::Read {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(&path, bytes).expect("write");
        path
    }

    #[test]
    fn discovers_pdf_and_skips_audio() {
        let tmp = TempDir::new().expect("tmpdir");
        let root = tmp.path();
        write(root, "notes.pdf", b"%PDF-1.7\n%pretend");
        write(root, "audio.m4b", b"\0\0\0\0ftypM4B ");
        write(root, "readme.txt", b"hi");
        let results = discover_companions(root).expect("walk");
        assert_eq!(results.len(), 1, "only the PDF should surface");
        assert_eq!(results[0].format, CompanionFormat::Pdf);
        assert_eq!(results[0].parse_tier, ParseTier::Document);
    }

    #[test]
    fn drops_unknown_magic_bytes() {
        let tmp = TempDir::new().expect("tmpdir");
        let root = tmp.path();
        // .pdf extension but garbage bytes — magic-byte dispatch
        // says Unknown, scanner drops it. Tests the
        // "bytes win" contract.
        write(root, "fake.pdf", b"this is not a pdf");
        let results = discover_companions(root).expect("walk");
        assert!(results.is_empty(), "Unknown-format files are dropped");
    }

    #[test]
    fn walks_subdirectories() {
        let tmp = TempDir::new().expect("tmpdir");
        let root = tmp.path();
        write(root, "a/b/c/notes.pdf", b"%PDF-1.4 fake");
        let results = discover_companions(root).expect("walk");
        assert_eq!(results.len(), 1);
        assert!(results[0].path.ends_with("notes.pdf"));
    }

    #[test]
    fn computes_blake3_and_bytes() {
        let tmp = TempDir::new().expect("tmpdir");
        let root = tmp.path();
        let body = b"%PDF-1.0 small body".to_vec();
        let expected_hash = blake3::hash(&body).to_hex().to_string();
        let expected_bytes = i64::try_from(body.len()).expect("fits i64");
        write(root, "x.pdf", &body);
        let results = discover_companions(root).expect("walk");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content_hash, expected_hash);
        assert_eq!(results[0].bytes, expected_bytes);
    }

    #[test]
    fn empty_tree_returns_empty() {
        let tmp = TempDir::new().expect("tmpdir");
        let results = discover_companions(tmp.path()).expect("walk");
        assert!(results.is_empty());
    }
}
