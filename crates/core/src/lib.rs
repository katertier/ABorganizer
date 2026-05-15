//! Shared types, errors, IDs, build-info constants, and the `Tunables`
//! config. Zero I/O — pure data + traits.
//!
//! Every other crate depends on this one. This crate depends on
//! nothing app-specific.

pub mod build_info {
    //! Branding constants generated from `[workspace.metadata.app]` in
    //! the workspace `Cargo.toml`. No source file may hardcode any of
    //! these strings — `cargo xtask check` enforces this.
    include!(concat!(env!("OUT_DIR"), "/build_info.rs"));
}

pub mod auth;
pub mod cache;
pub mod cleanup;
pub mod error;
pub mod field;
pub mod genre_code;
pub mod ids;
pub mod language_code;
pub mod paths;
pub mod reading_status;
pub mod tags;
pub mod time_format;
pub mod trust_zones;
pub mod tunables;

pub use cache::{CacheKey, ParseCacheKeyError, cache_keys_for_stage};
pub use cleanup::{Category, CleanupReport, Policy, compute_age_seconds};
pub use error::{Error, Result};
pub use field::{Field, ParseFieldError};
pub use ids::{BookId, FileId, JobId};
pub use reading_status::{ParseReadingStatusError, ReadingStatus};
pub use tags::{TAG_PREFIX_DNA, TAG_PREFIX_GENRE, TAG_PREFIX_SPOILER, TagKind};
pub use tunables::Tunables;
