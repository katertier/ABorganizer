//! Seed-fingerprint chromaprint match wiring for the audit
//! binary (ADR-0054, Phase 2C).
//!
//! Given a freshly-extracted audit clip (the 60s front or end
//! window the audit binary already produces) and the
//! [`SeedDb`] loaded via `--seed-fingerprints`:
//!
//! 1. Fingerprint the clip via [`ab_fingerprint::fingerprint_file`]
//!    (chromaprint over symphonia-decoded PCM).
//! 2. Narrow seeds to those whose position matches (`Intro` for the
//!    front clip, `Outro` for the end clip) AND whose publisher
//!    matches the book's `publisher` tag (case-insensitive
//!    substring on either side — `ABtagger` tags often add
//!    imprint suffixes like "Audible Originals" vs "Audible
//!    Studios").
//! 3. Base64-decode each candidate seed's `fingerprint_b64`,
//!    convert to `Vec<u32>` via
//!    [`ab_fingerprint::fingerprint_from_bytes`], and slide-match
//!    against the clip's hash sequence via
//!    [`ab_fingerprint::slide_match`].
//! 4. Return the highest-confidence match (or `None`).
//!
//! This is the cascade's **first cut**: if a publisher's known
//! fingerprint matches, the operator can confirm the jingle
//! without falling through to transcript / silence detection.

#![allow(clippy::missing_errors_doc)]

use std::path::Path;

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use ab_fingerprint::{
    confidence_from_hamming, fingerprint_file, fingerprint_from_bytes, slide_match,
};

use super::seed::{Position, SeedDb};

/// Best-match result against the seed DB.
#[derive(Debug, Clone)]
pub struct SeedMatch {
    /// Index into [`SeedDb::fingerprints`] — useful for fetching
    /// the publisher / transcript excerpt for display.
    pub seed_idx: usize,
    /// Publisher tag from the matched seed.
    pub publisher: Option<String>,
    /// Publisher-match label from the matched seed.
    pub publisher_match: Option<String>,
    /// Position the match anchored at.
    pub position: Position,
    /// Hash-position offset inside the clip where the best
    /// alignment begins (chromaprint hash unit = ~0.124 s).
    pub hash_offset: usize,
    /// Hamming distance at that offset. Smaller is better.
    pub hamming: u32,
    /// Confidence ∈ [0.0, 1.0] derived from `hamming` /
    /// `needle_hashes`. The audit binary uses this for display
    /// ordering; downstream consumers compare against a
    /// per-method threshold.
    pub confidence: f32,
    /// Number of chromaprint hashes in the seed (needle length).
    pub needle_hashes: usize,
}

/// Match `clip_path` against every position-compatible,
/// publisher-compatible seed in `seeds`. Returns the best match
/// or `None` if the clip can't be fingerprinted / no seed
/// matches above zero confidence.
///
/// # Errors
///
/// Fingerprinting failures (file missing, unsupported codec)
/// surface as anyhow.
pub fn best_match(
    clip_path: &Path,
    seeds: &SeedDb,
    publisher_hint: Option<&str>,
    position: Position,
) -> Result<Option<SeedMatch>> {
    let windows = fingerprint_file(clip_path)
        .with_context(|| format!("fingerprint clip {}", clip_path.display()))?;
    let Some(window) = windows.into_iter().next() else {
        return Ok(None);
    };
    Ok(best_match_against_hashes(
        &window.fingerprint,
        seeds,
        publisher_hint,
        position,
    ))
}

/// Pure-function variant of [`best_match`] — takes the
/// already-fingerprinted clip hashes. Useful for tests and any
/// caller that has the hashes in hand (e.g. cached from a prior
/// pass).
#[must_use]
pub fn best_match_against_hashes(
    clip_hashes: &[u32],
    seeds: &SeedDb,
    publisher_hint: Option<&str>,
    position: Position,
) -> Option<SeedMatch> {
    let mut best: Option<SeedMatch> = None;
    for (idx, seed) in seeds.fingerprints.iter().enumerate() {
        if seed.position != position {
            continue;
        }
        if !publisher_compatible(publisher_hint, seed.publisher.as_deref()) {
            continue;
        }
        let Some(needle) = decode_fingerprint(&seed.fingerprint_b64) else {
            continue;
        };
        if needle.is_empty() {
            continue;
        }
        let Some(pos) = slide_match(clip_hashes, &needle) else {
            continue;
        };
        let conf = confidence_from_hamming(pos.hamming, needle.len());
        let candidate = SeedMatch {
            seed_idx: idx,
            publisher: seed.publisher.clone(),
            publisher_match: seed.publisher_match.clone(),
            position,
            hash_offset: pos.hash_offset,
            hamming: pos.hamming,
            confidence: conf,
            needle_hashes: needle.len(),
        };
        best = match best {
            None => Some(candidate),
            Some(cur) if candidate.confidence > cur.confidence => Some(candidate),
            cur => cur,
        };
    }
    best
}

/// Decode `s` as standard base64, then unpack as little-endian
/// `u32` hashes. Returns `None` if either step fails.
fn decode_fingerprint(s: &str) -> Option<Vec<u32>> {
    let bytes = STANDARD.decode(s).ok()?;
    Some(fingerprint_from_bytes(&bytes))
}

/// True when the book's publisher tag plausibly matches the
/// seed's. Both lowercased; either may be a substring of the
/// other. Both must be present — we don't speculatively match
/// "(no publisher)" books against any seed.
fn publisher_compatible(book: Option<&str>, seed: Option<&str>) -> bool {
    let (Some(b), Some(s)) = (book, seed) else {
        return false;
    };
    let bl = b.trim().to_lowercase();
    let sl = s.trim().to_lowercase();
    if bl.is_empty() || sl.is_empty() {
        return false;
    }
    bl == sl || bl.contains(&sl) || sl.contains(&bl)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::audit::seed::{SeedFingerprint, SeedSource};
    use ab_fingerprint::fingerprint_to_bytes;

    fn fixture_seed(
        publisher: Option<&str>,
        position: Position,
        hashes: &[u32],
    ) -> SeedFingerprint {
        let bytes = fingerprint_to_bytes(hashes);
        let b64 = STANDARD.encode(&bytes);
        SeedFingerprint {
            publisher: publisher.map(str::to_owned),
            publisher_match: None,
            position,
            fingerprint_b64: b64,
            duration_ms: 1000,
            transcript_excerpt: None,
            source: SeedSource::OperatorConfirmed,
            confirmed: false,
        }
    }

    fn db(seeds: Vec<SeedFingerprint>) -> SeedDb {
        SeedDb {
            fingerprints: seeds,
        }
    }

    #[test]
    fn publisher_compatible_case_insensitive_substring() {
        assert!(publisher_compatible(
            Some("Audible Originals"),
            Some("audible")
        ));
        assert!(publisher_compatible(
            Some("audible"),
            Some("Audible Originals")
        ));
        assert!(publisher_compatible(
            Some("Random House Audio"),
            Some("Random House Audio")
        ));
        assert!(!publisher_compatible(
            Some("Brilliance Audio"),
            Some("Audible")
        ));
        assert!(!publisher_compatible(None, Some("Audible")));
        assert!(!publisher_compatible(Some("Audible"), None));
        assert!(!publisher_compatible(Some("   "), Some("Audible")));
    }

    #[test]
    fn decode_fingerprint_round_trip() {
        let original: Vec<u32> = vec![0x1234_5678, 0xDEAD_BEEF, 0xCAFE_BABE];
        let bytes = fingerprint_to_bytes(&original);
        let b64 = STANDARD.encode(&bytes);
        let decoded = decode_fingerprint(&b64).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn decode_fingerprint_invalid_b64_returns_none() {
        assert!(decode_fingerprint("not-valid-base64-!!").is_none());
    }

    #[test]
    fn best_match_picks_publisher_compatible_only() {
        // Build a clip = 200 random-looking hashes containing the
        // Audible needle at offset 50.
        let needle: Vec<u32> = (10..30).map(|i| 0x1000_0000_u32 + i).collect();
        let mut clip: Vec<u32> = (0..50).map(|i| 0xAAAA_BB00_u32 + i).collect();
        clip.extend(&needle);
        clip.extend((70..200).map(|i| 0xCCCC_DD00_u32 + i));

        let audible_seed = fixture_seed(Some("Audible Originals"), Position::Intro, &needle);
        let brilliance_seed = fixture_seed(
            Some("Brilliance Audio"),
            Position::Intro,
            &needle, // same needle, but should be rejected by publisher gate
        );
        let outro_seed = fixture_seed(Some("Audible"), Position::Outro, &needle);

        let seeds = db(vec![audible_seed, brilliance_seed, outro_seed]);
        let m = best_match_against_hashes(&clip, &seeds, Some("Audible"), Position::Intro)
            .expect("audible intro match");
        assert_eq!(m.seed_idx, 0, "must pick the Audible Originals seed");
        assert_eq!(m.position, Position::Intro);
        assert!(
            m.confidence > 0.99,
            "perfect match confidence: {}",
            m.confidence
        );
        assert_eq!(m.hash_offset, 50);
        assert_eq!(m.hamming, 0);
    }

    #[test]
    fn best_match_returns_none_when_no_publisher_match() {
        let needle: Vec<u32> = (0..20).map(|i| 0x1000_0000_u32 + i).collect();
        let clip: Vec<u32> = needle.iter().chain(needle.iter()).copied().collect();
        let seeds = db(vec![fixture_seed(
            Some("Random House Audio"),
            Position::Intro,
            &needle,
        )]);
        let m = best_match_against_hashes(&clip, &seeds, Some("Audible"), Position::Intro);
        assert!(m.is_none(), "no publisher overlap → no match");
    }

    #[test]
    fn best_match_returns_none_for_wrong_position() {
        let needle: Vec<u32> = (0..20).map(|i| 0x2000_0000_u32 + i).collect();
        let clip = needle.clone();
        let seeds = db(vec![fixture_seed(
            Some("Audible"),
            Position::Outro,
            &needle,
        )]);
        // Looking for intro, only outro available → no match.
        let m = best_match_against_hashes(&clip, &seeds, Some("Audible"), Position::Intro);
        assert!(m.is_none());
    }

    #[test]
    fn best_match_returns_none_when_publisher_hint_absent() {
        let needle: Vec<u32> = (0..20).map(|i| 0x3000_0000_u32 + i).collect();
        let clip = needle.clone();
        let seeds = db(vec![fixture_seed(
            Some("Audible"),
            Position::Intro,
            &needle,
        )]);
        // Book has no publisher tag → never match (we don't
        // speculatively cross-match).
        let m = best_match_against_hashes(&clip, &seeds, None, Position::Intro);
        assert!(m.is_none());
    }

    #[test]
    fn best_match_picks_higher_confidence_when_multiple() {
        let needle_a: Vec<u32> = (0..30).map(|i| 0x4000_0000_u32 + i).collect();
        // Inject `needle_a` exactly at offset 5 in the clip ->
        // hamming = 0 → confidence near 1.0.
        let mut clip: Vec<u32> = (0..5).map(|i| 0xDEAD_0000_u32 + i).collect();
        clip.extend(&needle_a);
        clip.extend((40..200).map(|i| 0xBEEF_0000_u32 + i));

        // Make `needle_b` a slightly-different sequence (bit
        // flipped in each hash) → still aligns somewhere but with
        // worse hamming.
        let needle_b: Vec<u32> = needle_a.iter().map(|h| h ^ 0xFFFF_FFFF).collect();

        let seeds = db(vec![
            fixture_seed(Some("Audible"), Position::Intro, &needle_b), // worse match
            fixture_seed(Some("Audible"), Position::Intro, &needle_a), // exact match
        ]);
        let m = best_match_against_hashes(&clip, &seeds, Some("Audible"), Position::Intro)
            .expect("match");
        assert_eq!(m.seed_idx, 1, "exact-match seed must win");
        assert!(m.confidence > 0.99);
    }
}
