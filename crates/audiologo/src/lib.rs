//! Publisher-jingle detection (intro + outro).
//!
//! # Public model
//!
//! Detection returns a single [`Detection`] with a confidence score.
//! No tier system — multiple detection methods (fingerprint, ASR
//! gate + silence, silence-only) each propose candidates; a ranker
//! picks the highest-confidence one. The current code's tier system
//! is collapsed into "method enum + confidence."
//!
//! # Storage
//!
//! Fingerprints persist to `library.audiologos`. Every row has a
//! real `source_book_id` FK + `source_offset_ms` so review tools can
//! reproduce the exact matched clip without parsing strings.
//!
//! # Verification provenance
//!
//! `verified_via` values, schema-enforced via CHECK constraint:
//!
//! * `manual` — `aborg audiologos add` from a known clean clip
//! * `review_confirmed` — accepted by user during `audiologos review`
//! * `silence` — auto-bootstrapped from a Tier-1-style cut (needs
//!   transcript corroboration to fire on a different book)
//! * `transcription` — auto with transcript publisher hit
//! * `seed` — shipped via the seed-data repo
//! * `import` — loaded from a JSON dump

use std::path::Path;

use serde::{Deserialize, Serialize};

use ab_core::Result;

/// Which side of the audio we're detecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Beginning of the file.
    Intro,
    /// End of the file.
    Outro,
}

/// How a candidate detection was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    /// Matched against a fingerprint already in the DB.
    Fingerprint,
    /// Transcript identified a publisher phrase; silence localised
    /// the boundary.
    AsrAndSilence,
    /// Silence-only detection (no transcript). Lower confidence.
    SilenceOnly,
    /// Brand intro/outro lengths supplied by Audnexus.
    CatalogBrandDuration,
}

/// One candidate trim point.
#[derive(Debug, Clone)]
pub struct Detection {
    /// Where to cut, in milliseconds (relative to file start for
    /// intros; relative to file end for outros).
    pub cut_ms: u64,
    /// Confidence in `[0.0, 1.0]`.
    pub confidence: f32,
    /// Which method produced this candidate.
    pub method: Method,
    /// Optional matched fingerprint ID when method is `Fingerprint`.
    pub matched_audiologo_id: Option<i64>,
}

/// Detect the best trim candidate for `file`.
///
/// Implementations live in the daemon's wiring; this crate exposes
/// the data types + ranker only.
///
/// # Errors
///
/// Returns [`ab_core::Error::Io`] on file-system errors,
/// [`ab_core::Error::Stage`] on decode failures.
#[allow(clippy::missing_const_for_fn)]
pub fn detect(_file: &Path, _kind: Kind) -> Result<Option<Detection>> {
    Ok(None)
}

/// Pick the highest-confidence detection from a candidate list.
/// Returns `None` if `candidates` is empty.
pub fn rank(mut candidates: Vec<Detection>) -> Option<Detection> {
    candidates.sort_by(|a, b| {
        // NaN sorts as equal; we never construct NaN confidences.
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    candidates.into_iter().next()
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn rank_picks_highest_confidence() {
        let a = Detection {
            cut_ms: 1000,
            confidence: 0.6,
            method: Method::Fingerprint,
            matched_audiologo_id: None,
        };
        let b = Detection {
            cut_ms: 2000,
            confidence: 0.9,
            method: Method::AsrAndSilence,
            matched_audiologo_id: None,
        };
        let winner = rank(vec![a, b]).expect("rank returns Some on non-empty input");
        assert_eq!(winner.cut_ms, 2000);
    }
}
