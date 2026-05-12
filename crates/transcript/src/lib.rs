//! Pipeline stages that orchestrate transcription + transcript-
//! reading extractors.
//!
//! The Apple Speech / `NaturalLanguage` FFI lives in the
//! [`ab_speech`] crate (separate so a CLI tool or test harness
//! can use the bridge without dragging the DB / pipeline
//! machinery along). This crate composes that surface with
//! `ab_db` writes + `ab_pipeline::Stage` impls.
//!
//! Surfaces:
//!
//! - [`TranscribeHeadTailStage`] / [`TranscribeSamplesStage`] /
//!   [`TranscribeFullStage`] — the three transcribe stages,
//!   each cached by `ai_cache.cache_type`.
//! - [`DetectDescriptionLangStage`] — runs
//!   `NLLanguageRecognizer` over the catalog description text
//!   and writes `books.description_lang`.
//! - [`RunExtractorsStage`] — iterates registered
//!   [`Extractor`]s over the cached head transcript, writes
//!   candidates to `book_field_provenance`.
//! - [`run_idle_install_loop`] — daemon-side tokio loop that
//!   drains `pending_speech_installs` at idle priority.
//!
//! Add a new transcript-text extractor: implement [`Extractor`]
//! + register it in [`extractors::built_in_extractors`].

pub mod description_lang_stage;
pub mod extract_stage;
pub mod extractors;
pub mod full_stage;
pub mod idle_install;
pub mod multi_file;
pub mod samples_stage;
pub mod stage;

pub use description_lang_stage::{
    DetectDescriptionLangStage, STAGE_NAME as DETECT_DESCRIPTION_LANG_STAGE,
};
pub use extract_stage::{RunExtractorsStage, STAGE_NAME as RUN_EXTRACTORS_STAGE};
pub use full_stage::{STAGE_NAME as TRANSCRIBE_FULL_STAGE, TranscribeFullStage};
pub use idle_install::run_idle_install_loop;
pub use samples_stage::{
    SOURCE_NL_LANGUAGE_SAMPLES, STAGE_NAME as TRANSCRIBE_SAMPLES_STAGE, TranscribeSamplesStage,
};
pub use stage::{
    SOURCE_NL_LANGUAGE_TAGS, STAGE_NAME as TRANSCRIBE_HEAD_TAIL_STAGE, TranscribeHeadTailStage,
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
