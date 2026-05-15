//! Cover-art handling crate (ADR-0030).
//!
//! Single home for cover bytes across the workspace. Owns:
//!
//! - HTTP fetch ([`CoverClient`]) with byte-cap defence-in-depth
//!   (Content-Length pre-check + streaming guard).
//! - Pre-decode pixel-bomb guard ([`decode_checked`]) — header-only
//!   dimension read before allocating decoder memory.
//! - Multi-size resize helper ([`resize_to_square_jpeg`]) for the
//!   thumbnail cache.
//!
//! Earlier the HTTP fetch surface lived inside `ab-tag-write` as
//! `tag_write::cover`. The refactor (slice B.15) moved it here so
//! every consumer of cover bytes — tag-write, shelf, future
//! menubar app for Finder folder icons — reaches one place.
//! `ab-tag-write` re-exports the type aliases for source-level
//! back-compat.
//!
//! What's intentionally **not** here yet:
//!
//! - **On-disk cache** with LRU eviction, manifest, and
//!   `covers/<book_id>/<size>.jpg` layout. Folder-icon work is in
//!   ADR-0030 § Folder icons; both ship after the foundation slice
//!   lands.
//! - **ABS-compat sidecar layout** writes (`cover.jpg` next to the
//!   audio file). Same ADR — separate slice.
//! - **Folder-icon FFI** (`NSWorkspace.setIcon:forFile:` +
//!   `com.apple.icon.folder#S` xattr). Lives in the menubar app
//!   target, not this crate, per ADR-0030.

mod decode;
mod fetch;
mod resize;

pub use decode::{DecodeCheckedError, decode_checked, probe_dimensions};
pub use fetch::{CoverClient, CoverFetchError};
pub use resize::{ResizeError, resize_to_square_jpeg};
