//! Seed-data bootstrap + updater.
//!
//! # First-run
//!
//! Daemon embeds a minimal seed set (publishers, canonical genres,
//! initial audiologo fingerprints) so it works offline. The
//! [`embedded()`] function returns this bundled set as parsed
//! structures.
//!
//! # Updates
//!
//! `aborg seed update` fetches the latest signed manifest from the
//! `aborganizer-seed` `GitHub` repo (URL constant below). Manifest is
//! signed via Ed25519; the verification key is embedded in this
//! crate. If signature verification fails the update is rejected
//! and the user is prompted.

use ab_core::Result;

/// URL of the seed repo's manifest. Override via env var `AB_SEED_URL` for testing.
pub const SEED_MANIFEST_URL: &str =
    "https://raw.githubusercontent.com/katertier/aborganizer-seed/main/manifest.json";

/// Embedded baseline seed data, parsed lazily on first call.
///
/// Returns a JSON value representing the same shape as the seed
/// repo's manifest, so callers don't branch on the source.
pub fn embedded() -> Result<serde_json::Value> {
    // Minimal v0 seed: an empty manifest. Real entries land in
    // follow-up commits once the seed repo is populated.
    Ok(serde_json::json!({
        "schema_version": 1,
        "seed_version": 0,
        "publishers": [],
        "audiologos": { "intro": [], "outro": [] },
        "genres": [],
        "heuristics": {},
    }))
}
