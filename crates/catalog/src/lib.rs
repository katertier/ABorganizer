//! Catalog clients for external book metadata sources.
//!
//! # Sources
//!
//! | Source    | Auth | Rate limit          | Returns                |
//! |-----------|------|---------------------|------------------------|
//! | Audnexus  | none | ~100/min per region | book/author/chapters   |
//! | Audible   | none | conservative ~120ms | catalog JSON (api.audible.com) |
//!
//! See `docs/PROJECT.md` for endpoint details. Region walks happen
//! when an ASIN lookup misses on the home region; the order is
//! configurable via `Tunables::network::audnexus_region_order`.

#![allow(missing_docs)] // scaffold

pub mod asin_learnings;
pub mod audible;
pub mod audible_search;
pub mod audnexus;
pub mod canonical_author;
pub mod chapter_info;
pub mod chapter_winner;
pub mod chapters;
pub mod consensus;
pub mod cue_chapters;
pub mod embedded_chapters;
pub mod enrich;
pub mod epub_chapters;
pub mod franchise;
pub mod identity;
pub mod mp3_chap;
pub mod release_state;
pub mod series_gaps;
pub mod tiers;

pub use audible::AudibleClient;
pub use audible_search::AudibleSearchStage;
pub use audnexus::AudnexusClient;
pub use canonical_author::CanonicalAuthorEnrichStage;
pub use chapter_winner::ChapterWinnerStage;
pub use chapters::AudnexusChaptersStage;
pub use consensus::ConsensusStage;
pub use cue_chapters::CueSidecarChaptersStage;
pub use embedded_chapters::EmbeddedChaptersStage;
pub use enrich::AudnexusEnrichStage;
pub use epub_chapters::EpubChaptersStage;
pub use identity::IdentityResolveStage;
