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

pub mod audible;
pub mod audible_search;
pub mod audnexus;
pub mod chapters;
pub mod consensus;
pub mod enrich;
pub mod identity;

pub use audible::AudibleClient;
pub use audible_search::AudibleSearchStage;
pub use audnexus::AudnexusClient;
pub use chapters::AudnexusChaptersStage;
pub use consensus::ConsensusStage;
pub use enrich::AudnexusEnrichStage;
pub use identity::IdentityResolveStage;
