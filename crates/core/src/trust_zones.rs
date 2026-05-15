//! Trust-zone-validated path handling (ADR-0049).
//!
//! Every filesystem write that takes a path component from
//! untrusted input — operator config, LLM extractor output,
//! catalog response, scanner discovery — funnels through this
//! module. The [`path_jail`] crate (tenuo-ai, MIT OR Apache-2.0,
//! same author as `safe_unzip`) supplies the canonicalize + jail
//! verification primitive; this module overlays compile-time
//! trust-zone markers so two paths from different roots can't be
//! accidentally swapped.
//!
//! # Trust zones
//!
//! Each zone has its own root directory + intended use:
//!
//! | Zone               | Root                              | Used by |
//! | ------------------ | --------------------------------- | ------- |
//! | [`LibraryRoot`]    | operator's `library_roots` entry  | scan, transcode out, library reorg |
//! | [`CoverCache`]     | `paths::cache_dir().join("covers")` | `ab-covers` thumbnail writes |
//! | [`ZipExtractRoot`] | source-zip parent dir             | `ab-archive` extract target |
//! | [`TranscodeOutput`]| final m4b output root             | `ab-transcode` |
//! | [`EphemeralDb`]    | `paths::cache_dir()`              | ephemeral.db + WAL siblings |
//!
//! Adding a zone: define a unit struct, `impl TrustZone for X { const NAME: ... }`.
//!
//! # Usage
//!
//! ```no_run
//! use ab_core::trust_zones::{TrustZoneJail, LibraryRoot};
//! use std::path::Path;
//!
//! let jail = TrustZoneJail::<LibraryRoot>::new(Path::new("/Volumes/Audiobooks/Library"))?;
//! // Validates the relative path stays inside the jail; canonicalizes
//! // symlinks; rejects `../`, absolute paths, null bytes, escaping symlinks.
//! let book_dir = jail.join("AuthorName/BookTitle")?;
//! // `book_dir` has type `TrustedPath<LibraryRoot>`; mixing with a
//! // `TrustedPath<CoverCache>` is a compile error.
//! # Ok::<_, ab_core::trust_zones::TrustZoneError>(())
//! ```
//!
//! # What this module owns vs. what `path_jail` owns
//!
//! - **`path_jail`** owns: path canonicalization, traversal /
//!   symlink-escape / absolute-injection / null-byte rejection,
//!   the `secure-open` `O_NOFOLLOW` opt-in for file open/create,
//!   handling of paths that don't yet exist.
//! - **This module** owns: the marker types, the wrapper API that
//!   keeps the marker visible across joins, and the convention
//!   that every ABorganizer write path goes through a zone jail.

use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use path_jail::{Jail, JailedFile, JailedPath};

/// Marker trait identifying a trust zone. Implementors must be
/// zero-sized; the trait carries only a static `NAME` for tracing.
pub trait TrustZone: 'static {
    /// Lower-case identifier used in tracing / error messages.
    const NAME: &'static str;
}

/// Operator's `library_roots` entry. Audio files + companion files
/// live here. The widest-blast-radius zone — most filesystem
/// writes ultimately land in here.
#[derive(Debug, Clone, Copy)]
pub struct LibraryRoot;
impl TrustZone for LibraryRoot {
    const NAME: &'static str = "library_root";
}

/// `~/Library/Caches/<DisplayName>/covers` — multi-size thumbnail
/// cache (ADR-0030).
///
/// Writes are derived from Audnexus / Audible CDN responses +
/// embedded picture atoms, so the path components (asin, size
/// suffix) are partly external input.
#[derive(Debug, Clone, Copy)]
pub struct CoverCache;
impl TrustZone for CoverCache {
    const NAME: &'static str = "cover_cache";
}

/// `<source-zip-parent>/<source-stem>.extracted/` — the sibling
/// directory `ab-archive` extracts ZIPs into (ADR-0047).
///
/// The extractor in `safe_unzip` already enforces zip-slip /
/// depth / size caps; this zone wraps the *output* root for any
/// post-extract operations that need a containment guarantee.
#[derive(Debug, Clone, Copy)]
pub struct ZipExtractRoot;
impl TrustZone for ZipExtractRoot {
    const NAME: &'static str = "zip_extract_root";
}

/// Transcode output staging directory — the m4b that `ab-transcode`
/// writes before atomic rename into `LibraryRoot`. Currently
/// `paths::cache_dir().join("transcode")`; configurable via
/// `TranscodeTunables`.
#[derive(Debug, Clone, Copy)]
pub struct TranscodeOutput;
impl TrustZone for TranscodeOutput {
    const NAME: &'static str = "transcode_output";
}

/// `~/Library/Caches/<DisplayName>` — ephemeral.db + WAL +
/// pipeline-progress sibling files.
#[derive(Debug, Clone, Copy)]
pub struct EphemeralDb;
impl TrustZone for EphemeralDb {
    const NAME: &'static str = "ephemeral_db";
}

/// Marker-typed filesystem jail. The zone marker prevents
/// accidental cross-zone path mixing at compile time.
pub struct TrustZoneJail<M: TrustZone> {
    inner: Jail,
    _marker: PhantomData<fn() -> M>,
}

impl<M: TrustZone> TrustZoneJail<M> {
    /// Create a jail rooted at `root`. The root must exist;
    /// non-existent jails are rejected at construction time so the
    /// canonicalization invariant holds for every subsequent join.
    ///
    /// # Errors
    ///
    /// [`TrustZoneError::Jail`] if the root doesn't exist, isn't a
    /// directory, or fails canonicalization.
    pub fn new(root: &Path) -> Result<Self, TrustZoneError> {
        let inner = Jail::new(root).map_err(|e| TrustZoneError::Jail {
            zone: M::NAME,
            root: root.to_path_buf(),
            source: e,
        })?;
        Ok(Self {
            inner,
            _marker: PhantomData,
        })
    }

    /// Canonical root path. Useful for displaying the jail to the
    /// operator in `aborg doctor` output.
    #[must_use]
    pub fn root(&self) -> &Path {
        self.inner.root()
    }

    /// Validate `relative` against the jail and return a
    /// [`TrustedPath`] tagged with this zone.
    ///
    /// The relative path may include subdirectories; `path_jail`
    /// canonicalizes the result and verifies it lands inside the
    /// jail (rejecting `..` traversal, symlink escapes, absolute
    /// injection, null bytes). Non-existent components are
    /// permitted — write-side paths validate cleanly.
    ///
    /// # Errors
    ///
    /// [`TrustZoneError::Jail`] on any traversal / escape / null
    /// byte violation.
    pub fn join(&self, relative: impl AsRef<Path>) -> Result<TrustedPath<M>, TrustZoneError> {
        let inner = self
            .inner
            .join_typed(&relative)
            .map_err(|e| TrustZoneError::Jail {
                zone: M::NAME,
                root: self.inner.root().to_path_buf(),
                source: e,
            })?;
        Ok(TrustedPath {
            inner,
            _marker: PhantomData,
        })
    }

    /// Create + open a new file inside the jail with `O_NOFOLLOW`
    /// applied via the `secure-open` feature. Closes the TOCTOU
    /// window between path canonicalization and file open: if the
    /// final path component became a symlink between validation
    /// and open, the syscall refuses.
    ///
    /// # Errors
    ///
    /// [`TrustZoneError::Jail`] on validation failure or open error.
    pub fn create(&self, relative: impl AsRef<Path>) -> Result<JailedFile, TrustZoneError> {
        self.inner
            .create(&relative)
            .map_err(|e| TrustZoneError::Jail {
                zone: M::NAME,
                root: self.inner.root().to_path_buf(),
                source: e,
            })
    }

    /// Open an existing file inside the jail with `O_NOFOLLOW`.
    ///
    /// # Errors
    ///
    /// [`TrustZoneError::Jail`] on validation failure or open error.
    pub fn open(&self, relative: impl AsRef<Path>) -> Result<JailedFile, TrustZoneError> {
        self.inner
            .open(&relative)
            .map_err(|e| TrustZoneError::Jail {
                zone: M::NAME,
                root: self.inner.root().to_path_buf(),
                source: e,
            })
    }
}

/// Path verified to be inside a [`TrustZoneJail`] of zone `M`.
///
/// Deliberately does NOT implement `Deref<Target = Path>` (which
/// `path_jail::JailedPath` provides). Going through
/// [`Self::as_path`] forces every conversion to be explicit and
/// keeps the trust-zone marker visible at the call site.
pub struct TrustedPath<M: TrustZone> {
    inner: JailedPath,
    _marker: PhantomData<fn() -> M>,
}

impl<M: TrustZone> TrustedPath<M> {
    /// Borrow as a plain `&Path`. Use this when interoperating
    /// with `std::fs` / `tokio::fs` / sqlx parameter binding.
    /// Prefer the [`TrustZoneJail`] open/create methods when
    /// `O_NOFOLLOW` matters.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        self.inner.as_path()
    }

    /// Consume + return the inner `PathBuf`.
    #[must_use]
    pub fn into_path_buf(self) -> PathBuf {
        self.inner.into_inner()
    }
}

impl<M: TrustZone> std::fmt::Debug for TrustedPath<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrustedPath")
            .field("zone", &M::NAME)
            .field("path", &self.inner.as_path())
            .finish()
    }
}

impl<M: TrustZone> AsRef<Path> for TrustedPath<M> {
    fn as_ref(&self) -> &Path {
        self.inner.as_path()
    }
}

/// Errors surfaced by this module.
#[derive(Debug, thiserror::Error)]
pub enum TrustZoneError {
    /// Path-jail validation failure — traversal, symlink escape,
    /// null byte, root mis-configuration, or open error from the
    /// `secure-open` feature.
    #[error("trust-zone {zone:?} jail violation at {root}: {source}")]
    Jail {
        /// Zone name from [`TrustZone::NAME`].
        zone: &'static str,
        /// Canonical jail root, included in the error for tracing.
        root: PathBuf,
        /// Underlying `path_jail` error.
        source: path_jail::JailError,
    },
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn jail_new_rejects_missing_root() {
        let result = TrustZoneJail::<LibraryRoot>::new(Path::new(
            "/tmp/definitely-not-a-real-jail-root-for-tests",
        ));
        assert!(matches!(result, Err(TrustZoneError::Jail { .. })));
    }

    #[test]
    fn jail_join_normalises_inside() {
        let tmp = TempDir::new().expect("tempdir");
        let jail = TrustZoneJail::<LibraryRoot>::new(tmp.path()).expect("jail");
        let validated = jail.join("author/book/audio.m4b").expect("join");
        let validated_path = validated.as_path();
        // path_jail canonicalises symlinks but leaves non-existent
        // subdirs as components. Either form must remain inside
        // the canonical jail root.
        let canonical_root = jail.root();
        assert!(
            validated_path.starts_with(canonical_root),
            "{validated_path:?} should start with {canonical_root:?}"
        );
    }

    #[test]
    fn jail_join_rejects_traversal() {
        let tmp = TempDir::new().expect("tempdir");
        let jail = TrustZoneJail::<LibraryRoot>::new(tmp.path()).expect("jail");
        let result = jail.join("../escape.txt");
        assert!(
            matches!(result, Err(TrustZoneError::Jail { .. })),
            "expected jail violation, got {result:?}"
        );
    }

    #[test]
    fn jail_join_rejects_absolute() {
        let tmp = TempDir::new().expect("tempdir");
        let jail = TrustZoneJail::<LibraryRoot>::new(tmp.path()).expect("jail");
        let result = jail.join("/etc/passwd");
        assert!(matches!(result, Err(TrustZoneError::Jail { .. })));
    }

    #[test]
    fn jail_create_writes_inside() {
        // O_NOFOLLOW path: create a file inside the jail via the
        // secure-open feature. The file lands at the validated
        // location and is readable through the JailedFile handle.
        use std::io::Write as _;
        let tmp = TempDir::new().expect("tempdir");
        let jail = TrustZoneJail::<LibraryRoot>::new(tmp.path()).expect("jail");
        let mut handle = jail.create("hello.txt").expect("create");
        handle.write_all(b"hi").expect("write");
        drop(handle);
        let body = std::fs::read(tmp.path().join("hello.txt")).expect("read");
        assert_eq!(body, b"hi");
    }

    // Compile-time test: different zones cannot be mixed. Verified
    // via `compile_fail` doctest at the module level on a follow-up
    // slice once the trybuild infrastructure is in place — for now
    // the marker type tags make the intent visible at the call site.

    #[test]
    fn trusted_path_carries_zone_in_debug() {
        let tmp = TempDir::new().expect("tempdir");
        let jail = TrustZoneJail::<CoverCache>::new(tmp.path()).expect("jail");
        let path = jail.join("thumb.jpg").expect("join");
        let debug_str = format!("{path:?}");
        assert!(
            debug_str.contains("cover_cache"),
            "expected zone tag in debug output, got {debug_str}"
        );
    }
}
