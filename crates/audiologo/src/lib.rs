//! Publisher-jingle detection (intro + outro).
//!
//! # Theme overview
//!
//! See ADR-0024 for the full design. This crate covers slice 4A's
//! foundational surface: the [`Method`] enum, the [`Kind`] +
//! [`Status`] + [`BookStatus`] enums, the [`Detection`] data type,
//! and the [`rank()`] helper. Detection algorithms (catalog
//! bootstrap, fingerprint matching, transcript-aided) ship in
//! slices 4B-4D. Review workflow ships in slice 4E.
//!
//! # Storage shape (4A onwards)
//!
//! Per-file rows live in `book_file_audiologos`
//! (`(file_id, kind, jingle_start_ms, jingle_end_ms, padding_ms,
//! method, audiologo_id, confidence, status, …)`). The
//! semantic cut is `[jingle_start_ms, jingle_end_ms]` — a
//! mid-text splice that preserves "Title by Author" voiceovers
//! that follow publisher jingles.
//!
//! The `books.brand_intro_duration_ms` / `_outro_ms` columns
//! (renamed from `audiologo_intro_ms` / `_outro_ms` by slice
//! 4B.0 / migration 017) hold Audnexus's reported brand-jingle
//! duration. They feed chapter-mark recomputation at apply
//! time + the Libation-stripped path (per ADR-0024 Revision 2);
//! they are **not** an input to detection — fingerprint
//! matching against the `audiologos` table is the only
//! detection path.
//!
//! Fingerprints persist in `audiologos` (extended in 4A with
//! the `'ab_tagger_import'` `verified_via` value for 4A's
//! one-shot import).
//!
//! # Empirical re-evaluation
//!
//! The per-tier confidence floors + tier ordering ship as
//! best-effort defaults in 4A's tunables. ADR-0024 explicitly
//! commits to re-evaluating both after slices 4A-4E have
//! landed + the user's full library has been imported and
//! reviewed. The COURSE-CORRECTION journal gets a follow-up
//! cycle entry then.

use std::path::Path;

use serde::{Deserialize, Serialize};

use ab_core::Result;

pub mod stage;

pub use stage::{DetectAudiologoStage, STAGE_ID as DETECT_AUDIOLOGO_STAGE_ID};

/// Which side of the audio we're detecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Beginning of the file.
    Intro,
    /// End of the file.
    Outro,
}

impl Kind {
    /// The exact string written into
    /// `book_file_audiologos.kind` and `audiologos.kind`. Use
    /// in `sqlx::query!` bind params.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Intro => "intro",
            Self::Outro => "outro",
        }
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a candidate detection was produced.
///
/// The five tiers, ordered roughly by descending reliability:
///
/// 1. [`Method::FingerprintFull`] — Whole jingle waveform
///    matches a known `audiologos` row.
/// 2. [`Method::FingerprintBookend`] — Start + end
///    fingerprints match; middle may vary (publishers that
///    vary their voice line per book but keep stable bookends).
/// 3. [`Method::FingerprintAndTranscript`] — Start fingerprint
///    matches AND the transcript contains a publisher mention
///    that localises the cut's end.
/// 4. [`Method::TranscriptOnly`] — Transcript contains a
///    publisher mention; silence (used as an internal
///    localiser only, never as a standalone signal) marks the
///    cut boundary.
/// 5. [`Method::Manual`] — Operator-set cut via the CLI / API.
///
/// `SilenceOnly` from the pre-4A scaffold is dropped (silence
/// is not a reliable jingle indicator per the user's empirical
/// experience; it stays internal to tiers 3 + 4 only).
///
/// `CatalogBrandDuration` from the original ADR-0024 is dropped
/// in Revision 2 (2026-05-13): Audnexus brand-duration is only
/// available for Audible books, and for those we already have
/// the matching fingerprints. The brand-duration value is still
/// persisted (`books.brand_intro_duration_ms` / `_outro_ms`),
/// but only as input to chapter-mark recomputation + the
/// Libation-stripped path — not as a detection tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    /// Full jingle waveform matches an existing audiologos row.
    FingerprintFull,
    /// Start + end fingerprints match (mid-jingle may vary).
    FingerprintBookend,
    /// Start fingerprint match + transcript publisher mention.
    FingerprintAndTranscript,
    /// Transcript publisher mention + silence localises cut.
    TranscriptOnly,
    /// Operator-set cut via `aborg audiologos cut` or the API.
    Manual,
}

impl Method {
    /// The exact string written into
    /// `book_file_audiologos.method`. Use in `sqlx::query!`
    /// bind params.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FingerprintFull => "fingerprint_full",
            Self::FingerprintBookend => "fingerprint_bookend",
            Self::FingerprintAndTranscript => "fingerprint_and_transcript",
            Self::TranscriptOnly => "transcript_only",
            Self::Manual => "manual",
        }
    }

    /// Parse the `method` column back into the typed enum.
    /// Returns `None` for unknown strings — callers treat
    /// these as legacy / pre-4A rows (or pre-Revision-2 rows
    /// for the dropped `catalog_brand_duration` value).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "fingerprint_full" => Some(Self::FingerprintFull),
            "fingerprint_bookend" => Some(Self::FingerprintBookend),
            "fingerprint_and_transcript" => Some(Self::FingerprintAndTranscript),
            "transcript_only" => Some(Self::TranscriptOnly),
            "manual" => Some(Self::Manual),
            _ => None,
        }
    }

    /// Does this Method auto-apply (skip the `candidate`
    /// state) when its confidence clears the per-Method floor?
    ///
    /// The two transcript-bearing tiers always stay as
    /// candidates; the user reviews them. The two
    /// fingerprint-bearing methods + Manual auto-apply.
    #[must_use]
    pub const fn auto_applies(self) -> bool {
        matches!(
            self,
            Self::FingerprintFull | Self::FingerprintBookend | Self::Manual,
        )
    }
}

impl std::fmt::Display for Method {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// State machine for a `book_file_audiologos` row. See
/// ADR-0024 § state-machine diagram for the transition rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Detected; not yet applied. Awaits user review for the
    /// two transcript-bearing tiers; auto-applies for the
    /// other four Methods when above the per-Method confidence
    /// floor.
    Candidate,
    /// Trim is live; `books.duration_ms` reflects it; chapters
    /// have been shifted.
    Applied,
    /// User reviewed and said "no" to this specific candidate
    /// or applied row.
    Rejected,
    /// Superseded by a newer row for the same `(file_id, kind)`
    /// pair (re-detection ran and produced a different cut).
    ReDetected,
}

impl Status {
    /// The exact string written into
    /// `book_file_audiologos.status`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Applied => "applied",
            Self::Rejected => "rejected",
            Self::ReDetected => "re_detected",
        }
    }

    /// Parse the `status` column back into the typed enum.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "candidate" => Some(Self::Candidate),
            "applied" => Some(Self::Applied),
            "rejected" => Some(Self::Rejected),
            "re_detected" => Some(Self::ReDetected),
            _ => None,
        }
    }

    /// Is `next` a valid transition from `self`?
    ///
    /// See ADR-0024 § state-machine diagram. Transitions:
    /// - `Candidate → Applied` (auto for auto-applying Methods
    ///   above floor; manual via review)
    /// - `Candidate → Rejected` (user said no)
    /// - `Applied → Rejected` (user un-applies)
    /// - `Applied → ReDetected` (a new candidate replaced it)
    /// - `Candidate → ReDetected` (re-detection replaced an
    ///   unapproved candidate)
    /// - Self-transitions are not transitions.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (
                Self::Candidate,
                Self::Applied | Self::Rejected | Self::ReDetected
            ) | (Self::Applied, Self::Rejected | Self::ReDetected)
        )
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Book-level audiologo status.
///
/// Lives on `books.audiologo_status` as a first-class
/// metadata column (NOT NULL DEFAULT `'unknown'`); distinguishes
/// "detection hasn't run yet" from "we detected nothing" from
/// "the file was libation-stripped."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BookStatus {
    /// Detection hasn't run.
    Unknown,
    /// At least one `Candidate` row exists for this book.
    Detected,
    /// At least one `Applied` row exists for this book.
    Applied,
    /// Catalog said there should be a jingle here, but
    /// fingerprint at that location matched nothing AND no
    /// silence + transcript hit confirmed presence (or a
    /// cross-kind match warning fired). Libation-suspect.
    Stripped,
    /// No catalog hint AND detection produced no matches.
    /// E.g. self-published or unbranded.
    None,
    /// User reviewed and explicitly rejected any trim.
    Rejected,
}

impl BookStatus {
    /// The exact string written into `books.audiologo_status`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Detected => "detected",
            Self::Applied => "applied",
            Self::Stripped => "stripped",
            Self::None => "none",
            Self::Rejected => "rejected",
        }
    }

    /// Parse the `audiologo_status` column back into the
    /// typed enum.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "unknown" => Some(Self::Unknown),
            "detected" => Some(Self::Detected),
            "applied" => Some(Self::Applied),
            "stripped" => Some(Self::Stripped),
            "none" => Some(Self::None),
            "rejected" => Some(Self::Rejected),
            _ => None,
        }
    }
}

impl std::fmt::Display for BookStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One candidate trim point produced by a detection method.
///
/// `cut_ms_start` + `cut_ms_end` define the splice range
/// (offset from file start). For outros, the offsets are
/// still measured from file start (NOT from file end), so the
/// chapter-shift maths is uniform.
///
/// `padding_ms` is `Some(N)` when the detector decided the
/// boundary lands mid-utterance and a non-default padding is
/// warranted; `None` means "use the tunable default."
#[derive(Debug, Clone)]
pub struct Detection {
    /// Where the jingle begins in the file (ms from start).
    pub cut_ms_start: u64,
    /// Where the jingle ends (ms from start).
    pub cut_ms_end: u64,
    /// Confidence in `[0.0, 1.0]`.
    pub confidence: f32,
    /// Which method produced this candidate.
    pub method: Method,
    /// Matched `audiologos.audiologo_id` when the method is
    /// one of the fingerprint-bearing tiers. `None` for
    /// `TranscriptOnly` and bootstrap-without-match cases.
    pub matched_audiologo_id: Option<i64>,
    /// Detector-decided padding override; `None` = use the
    /// tunable default.
    pub padding_ms: Option<u32>,
}

/// Detect the best trim candidate for `file`.
///
/// Implementations live in `crates/audiologo/src/stage.rs`
/// (slice 4B onwards). This stub returns `Ok(None)` until then.
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
        // NaN sorts as equal; we never construct NaN
        // confidences. The match precedence ordering is
        // implicit in confidence values that the detection
        // tiers produce.
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
            cut_ms_start: 0,
            cut_ms_end: 1_000,
            confidence: 0.6,
            method: Method::FingerprintFull,
            matched_audiologo_id: None,
            padding_ms: None,
        };
        let b = Detection {
            cut_ms_start: 0,
            cut_ms_end: 2_000,
            confidence: 0.9,
            method: Method::FingerprintAndTranscript,
            matched_audiologo_id: None,
            padding_ms: None,
        };
        let winner = rank(vec![a, b]).expect("rank returns Some on non-empty input");
        assert_eq!(winner.cut_ms_end, 2_000);
    }

    #[test]
    fn rank_returns_none_on_empty_input() {
        assert!(rank(vec![]).is_none());
    }

    #[test]
    fn method_round_trips_every_variant() {
        for m in [
            Method::FingerprintFull,
            Method::FingerprintBookend,
            Method::FingerprintAndTranscript,
            Method::TranscriptOnly,
            Method::Manual,
        ] {
            assert_eq!(Method::parse(m.as_str()), Some(m), "round-trip {m}");
        }
    }

    #[test]
    fn method_parse_unknown_returns_none() {
        // 4A drops `silence_only` from the enum; Revision 2
        // (slice 4B prep) drops `catalog_brand_duration`. Both
        // legacy strings parse as None so callers can flag them.
        assert!(Method::parse("silence_only").is_none());
        assert!(Method::parse("catalog_brand_duration").is_none());
        assert!(Method::parse("").is_none());
        assert!(Method::parse("FINGERPRINT_FULL").is_none());
    }

    #[test]
    fn auto_applies_covers_only_the_right_methods() {
        // Per ADR-0024 Revision 2: fp_full + fp_bookend + manual
        // auto-apply; the two transcript-bearing tiers stay as
        // candidates for user review.
        assert!(Method::FingerprintFull.auto_applies());
        assert!(Method::FingerprintBookend.auto_applies());
        assert!(Method::Manual.auto_applies());

        assert!(!Method::FingerprintAndTranscript.auto_applies());
        assert!(!Method::TranscriptOnly.auto_applies());
    }

    #[test]
    fn status_round_trips_every_variant() {
        for s in [
            Status::Candidate,
            Status::Applied,
            Status::Rejected,
            Status::ReDetected,
        ] {
            assert_eq!(Status::parse(s.as_str()), Some(s));
        }
    }

    #[test]
    fn status_transitions_are_directed() {
        // Valid transitions per ADR-0024 § state-machine diagram.
        assert!(Status::Candidate.can_transition_to(Status::Applied));
        assert!(Status::Candidate.can_transition_to(Status::Rejected));
        assert!(Status::Candidate.can_transition_to(Status::ReDetected));
        assert!(Status::Applied.can_transition_to(Status::Rejected));
        assert!(Status::Applied.can_transition_to(Status::ReDetected));

        // Invalid transitions.
        assert!(!Status::Applied.can_transition_to(Status::Candidate));
        assert!(!Status::Rejected.can_transition_to(Status::Candidate));
        assert!(!Status::Rejected.can_transition_to(Status::Applied));
        assert!(!Status::ReDetected.can_transition_to(Status::Applied));

        // Self-transitions don't count.
        assert!(!Status::Candidate.can_transition_to(Status::Candidate));
        assert!(!Status::Applied.can_transition_to(Status::Applied));
    }

    #[test]
    fn book_status_round_trips_every_variant() {
        for s in [
            BookStatus::Unknown,
            BookStatus::Detected,
            BookStatus::Applied,
            BookStatus::Stripped,
            BookStatus::None,
            BookStatus::Rejected,
        ] {
            assert_eq!(BookStatus::parse(s.as_str()), Some(s));
        }
    }

    #[test]
    fn kind_round_trips() {
        assert_eq!(Kind::Intro.as_str(), "intro");
        assert_eq!(Kind::Outro.as_str(), "outro");
    }
}
