// `pub(crate)` items inside a `pub(crate) mod` trip
// clippy::redundant_pub_crate, but switching to plain `pub`
// trips rustc::unreachable_pub. Allow the clippy lint
// module-wide; the crate-internal visibility is the intent.
#![allow(clippy::redundant_pub_crate)]

//! Trust-zone-jail resolution for the transcode write path
//! (ADR-0049 step 3).
//!
//! Every transcode write goes through here. The stage hands us
//! the absolute source path from `book_files.file_path`; we
//! canonicalize, find the matching `library_roots` row by
//! longest-prefix match, build a [`TrustZoneJail<LibraryRoot>`]
//! at that root, and return jailed input + output paths. The
//! Swift FFI is only ever called with paths that survived the
//! jail check.
//!
//! This closes two real attack surfaces:
//!
//! * A malicious symlink inside a library root pointing at
//!   `/etc/something` — canonicalize + jail-prefix-check
//!   rejects it before AVFoundation gets a chance to write.
//! * A `book_files.file_path` row that resolves outside any
//!   active library root (manual DB tampering, stale row from
//!   a removed root) — the longest-prefix-match step returns
//!   `None` and the stage skips with a warn log.
//!
//! Output paths are derived inside the same jail by stripping
//! the library-root prefix from the canonical input, swapping
//! the extension to `.m4b`, and re-validating via [`TrustZoneJail::join`].

use std::path::{Path, PathBuf};

use ab_core::trust_zones::{LibraryRoot, TrustZoneJail, TrustedPath};

/// One source resolved to a jailed input + jailed output pair.
pub(crate) struct Resolved {
    /// Owned jail keyed to the library root containing the source.
    pub(crate) _jail: TrustZoneJail<LibraryRoot>,
    /// Canonical input path, validated to lie inside the jail.
    pub(crate) input: TrustedPath<LibraryRoot>,
    /// Output path inside the same jail. Need not exist yet —
    /// the jail's `join()` permits non-existent components,
    /// which is correct for write-side validation.
    pub(crate) output: TrustedPath<LibraryRoot>,
}

impl std::fmt::Debug for Resolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `_jail` is intentionally omitted from the Debug output —
        // it's an owned handle whose interesting state (the root
        // path) is reachable via `self.input.as_path()` and the
        // jail's own root accessor isn't worth printing.
        f.debug_struct("Resolved")
            .field("input", &self.input.as_path())
            .field("output", &self.output.as_path())
            .finish_non_exhaustive()
    }
}

/// Reasons the resolver may decline to produce a `Resolved`.
///
/// Each variant carries enough context for a `tracing::warn!`
/// at the call site; the caller skips the file and continues.
#[derive(Debug)]
pub(crate) enum SkipReason {
    /// Source path failed canonicalization (unreachable / ENOENT).
    Canonicalize { source_path: PathBuf, error: String },
    /// No `library_roots` row matches the canonical source path.
    NotInAnyLibraryRoot { canonical: PathBuf },
    /// Jail construction or join failed (escape attempt, etc.).
    Jail { reason: String },
    /// `strip_prefix` unexpectedly failed despite a prefix match —
    /// indicates a logic bug in this module; emit so it's
    /// observable rather than silently dropping the file.
    PrefixStripFailed { canonical: PathBuf, root: PathBuf },
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Canonicalize { source_path, error } => write!(
                f,
                "canonicalize failed for {}: {error}",
                source_path.display()
            ),
            Self::NotInAnyLibraryRoot { canonical } => write!(
                f,
                "source path {} is not inside any active library_root",
                canonical.display()
            ),
            Self::Jail { reason } => write!(f, "jail validation failed: {reason}"),
            Self::PrefixStripFailed { canonical, root } => write!(
                f,
                "internal: strip_prefix({}, {}) failed despite starts_with match",
                canonical.display(),
                root.display()
            ),
        }
    }
}

/// Load every active `library_roots.path`, canonicalize each,
/// drop unreachable ones with a warn log. Returned vector is
/// the containment whitelist used for the longest-prefix
/// match.
///
/// Unreachable roots (rename, unmount, permission lift) are
/// not fatal: the daemon keeps running with whatever remains.
pub(crate) async fn library_roots_canonical(
    pool: &sqlx::SqlitePool,
) -> Result<Vec<PathBuf>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT path AS "path!: String"
             FROM library_roots
            WHERE is_active = 1"#,
    )
    .fetch_all(pool)
    .await?;

    let mut roots = Vec::with_capacity(rows.len());
    for r in rows {
        match tokio::fs::canonicalize(&r.path).await {
            Ok(canonical) => roots.push(canonical),
            Err(e) => tracing::warn!(
                path = %r.path,
                error = %e,
                "transcode.output_resolve.library_root_unreachable"
            ),
        }
    }
    Ok(roots)
}

/// Longest-prefix match. Returns the matching root (so the
/// caller can build a jail at it) or None if no active root
/// contains the canonical path.
///
/// Longest-prefix matters when nested roots are configured
/// (e.g. `/Volumes/Audiobooks` AND `/Volumes/Audiobooks/Library`)
/// — picking the deeper root keeps the jail tight.
fn longest_prefix_root<'a>(canonical: &Path, roots: &'a [PathBuf]) -> Option<&'a Path> {
    roots
        .iter()
        .filter(|r| canonical.starts_with(r))
        .max_by_key(|r| r.as_os_str().len())
        .map(PathBuf::as_path)
}

/// Resolve a single transcode source into a jailed
/// `(input, output)` pair.
///
/// Steps:
///
/// 1. Canonicalize the source — rejects ENOENT / EACCES.
/// 2. Load active `library_roots` (canonicalized).
/// 3. Longest-prefix-match the canonical source against the
///    roots; if none match, skip.
/// 4. Build a `TrustZoneJail<LibraryRoot>` at the matching root.
/// 5. Compute the relative path inside the jail.
/// 6. Validate the input via `jail.join(relative_input)` — this
///    is the structural belt-and-braces step (canonicalize +
///    prefix-match should have caught everything, but a
///    symlink-since-canonicalize race or `path_jail`'s own
///    `secure-open` invariants get a second chance here).
/// 7. Derive output: same relative path with `.m4b` extension.
/// 8. Validate output via the same join.
///
/// `library_roots` is taken as a slice so callers loading the
/// roots once per stage invocation don't repeat the DB query
/// per source file.
pub(crate) async fn resolve_for_source(
    source_path: &Path,
    library_roots: &[PathBuf],
) -> Result<Resolved, SkipReason> {
    let canonical =
        tokio::fs::canonicalize(source_path)
            .await
            .map_err(|e| SkipReason::Canonicalize {
                source_path: source_path.to_path_buf(),
                error: e.to_string(),
            })?;

    let root = longest_prefix_root(&canonical, library_roots).ok_or_else(|| {
        SkipReason::NotInAnyLibraryRoot {
            canonical: canonical.clone(),
        }
    })?;

    let jail = TrustZoneJail::<LibraryRoot>::new(root).map_err(|e| SkipReason::Jail {
        reason: e.to_string(),
    })?;

    let relative_input =
        canonical
            .strip_prefix(root)
            .map_err(|_| SkipReason::PrefixStripFailed {
                canonical: canonical.clone(),
                root: root.to_path_buf(),
            })?;

    let input = jail.join(relative_input).map_err(|e| SkipReason::Jail {
        reason: e.to_string(),
    })?;

    let relative_output = relative_input.with_extension("m4b");
    let output = jail.join(&relative_output).map_err(|e| SkipReason::Jail {
        reason: e.to_string(),
    })?;

    Ok(Resolved {
        _jail: jail,
        input,
        output,
    })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn longest_prefix_picks_deepest_match() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let outer = tmp.path().join("outer");
        let inner = outer.join("inner");
        tokio::fs::create_dir_all(&inner).await.expect("mkdir");
        let outer_c = tokio::fs::canonicalize(&outer).await.expect("canon outer");
        let inner_c = tokio::fs::canonicalize(&inner).await.expect("canon inner");
        let target = inner_c.join("book.m4a");
        tokio::fs::write(&target, b"x").await.expect("write");
        let target_c = tokio::fs::canonicalize(&target)
            .await
            .expect("canon target");

        let roots = vec![outer_c.clone(), inner_c.clone()];
        let picked = longest_prefix_root(&target_c, &roots).expect("match");
        assert_eq!(picked, inner_c.as_path());
    }

    #[tokio::test]
    async fn resolve_happy_path_yields_input_and_m4b_output() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let root_c = tokio::fs::canonicalize(&root).await.expect("canon root");
        let src = root.join("Author/Book/file.mp3");
        tokio::fs::create_dir_all(src.parent().unwrap())
            .await
            .expect("mkdir parents");
        tokio::fs::write(&src, b"x").await.expect("write src");

        let resolved = resolve_for_source(&src, std::slice::from_ref(&root_c))
            .await
            .expect("resolve");
        assert!(resolved.input.as_path().starts_with(&root_c));
        assert_eq!(resolved.output.as_path().extension().unwrap(), "m4b");
        assert!(resolved.output.as_path().starts_with(&root_c));
        assert_eq!(resolved.output.as_path().file_name().unwrap(), "file.m4b");
    }

    #[tokio::test]
    async fn resolve_rejects_path_outside_any_root() {
        let tmp_root = tempfile::tempdir().expect("tempdir root");
        let root_c = tokio::fs::canonicalize(tmp_root.path())
            .await
            .expect("canon root");

        let tmp_outside = tempfile::tempdir().expect("tempdir outside");
        let outside = tmp_outside.path().join("file.mp3");
        tokio::fs::write(&outside, b"x")
            .await
            .expect("write outside");

        let err = resolve_for_source(&outside, &[root_c])
            .await
            .expect_err("should reject");
        match err {
            SkipReason::NotInAnyLibraryRoot { .. } => {}
            other => panic!("unexpected: {other}"),
        }
    }

    #[tokio::test]
    async fn resolve_rejects_unreachable_source() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root_c = tokio::fs::canonicalize(tmp.path()).await.expect("canon");
        let missing = tmp.path().join("does-not-exist.mp3");

        let err = resolve_for_source(&missing, &[root_c])
            .await
            .expect_err("should reject");
        match err {
            SkipReason::Canonicalize { .. } => {}
            other => panic!("unexpected: {other}"),
        }
    }

    #[tokio::test]
    async fn resolve_rejects_symlink_escape() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("library");
        tokio::fs::create_dir(&root).await.expect("mkdir library");
        let root_c = tokio::fs::canonicalize(&root).await.expect("canon root");

        let outside_dir = tmp.path().join("outside");
        tokio::fs::create_dir(&outside_dir)
            .await
            .expect("mkdir outside");
        let outside_file = outside_dir.join("real.mp3");
        tokio::fs::write(&outside_file, b"x").await.expect("write");

        // Symlink under the library root that points outside.
        let link = root.join("link.mp3");
        std::os::unix::fs::symlink(&outside_file, &link).expect("symlink");

        // Canonicalize on the symlinked path resolves to the
        // real file outside the root, which the prefix check
        // then rejects.
        let err = resolve_for_source(&link, &[root_c])
            .await
            .expect_err("symlink escape should be rejected");
        match err {
            SkipReason::NotInAnyLibraryRoot { .. } => {}
            other => panic!("unexpected: {other}"),
        }
    }
}
