//! Transcript-side extractors + the Swift FFI bridge for
//! producing transcripts.
//!
//! Two surfaces here:
//!
//! 1. [`bridge::transcribe_window`] — call into Swift /
//!    `SpeechAnalyzer` (stubbed in slice 3A.2; real engine in
//!    3A.3). Returns timestamped [`bridge::TranscriptSegment`]s.
//! 2. [`Extractor`] + [`Candidate`] — pluggable consumers that
//!    read transcripts and write provenance candidates. The
//!    daemon's transcript stage iterates registered extractors;
//!    each writes 0..N candidates to `book_field_provenance`.
//!
//! Add a new extractor: implement [`Extractor`], register it.

pub mod bridge;
pub mod idle_install;
pub mod language;
pub mod stage;

pub use idle_install::run_idle_install_loop;

pub use bridge::{
    BridgeError, TranscriptSegment, install_speech_model, install_speech_model_typed,
    transcribe_window, transcribe_window_typed,
};
pub use language::{
    LanguageDetection, LanguageHit, detect as detect_language, detect_from_transcript,
};
pub use stage::{
    CACHE_TYPE_HEAD, CACHE_TYPE_TAIL, SOURCE_NL_LANGUAGE_TAGS, SOURCE_NL_LANGUAGE_TRANSCRIPT_HEAD,
    STAGE_NAME as TRANSCRIBE_HEAD_TAIL_STAGE, TranscribeHeadTailStage,
};

use serde::{Deserialize, Serialize};

/// A typed extractor over a transcript head/tail.
pub trait Extractor: Send + Sync + 'static {
    /// Stable identifier used as the provenance `source` value.
    fn name(&self) -> &'static str;

    /// Pull candidates from the given transcript. Returns empty when
    /// the extractor has no opinion (no pattern matched).
    fn extract(&self, transcript: &str) -> Vec<Candidate>;
}

/// A typed candidate value. Targets a single field on the book.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    /// Field this candidate proposes a value for (`title`, `author`,
    /// `narrator`, `language`, `publisher`).
    pub field: String,
    /// Proposed value.
    pub value: String,
    /// Confidence in `[0.0, 1.0]`.
    pub confidence: f32,
}

/// Transcript window helper: clamp `s` to a char-boundary-safe prefix
/// of `max_chars` bytes.
pub fn head(s: &str, max_chars: usize) -> &str {
    let end = max_chars.min(s.len());
    let end = (0..=end)
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0);
    &s[..end]
}

/// Transcript window helper: clamp `s` to a char-boundary-safe suffix
/// of `max_chars` bytes.
pub fn tail(s: &str, max_chars: usize) -> &str {
    let start = s.len().saturating_sub(max_chars);
    let start = (start..=s.len())
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(s.len());
    &s[start..]
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn head_is_char_safe() {
        let s = "hellö world"; // 'ö' is 2 bytes
        assert!(head(s, 5).is_char_boundary(0));
        assert!(s.starts_with(head(s, 4)));
    }

    #[test]
    fn tail_is_char_safe() {
        let s = "hellö world";
        let t = tail(s, 6);
        assert!(s.ends_with(t));
    }
}
