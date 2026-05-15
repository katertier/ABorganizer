//! Transcript correction via EPUB name dictionary (ADR-0043 § C.5).
//!
//! Pure-function library. Given:
//!
//! - an audio transcript (`transcript_full` per book; never the
//!   EPUB body text — we never inject EPUB prose into the speech
//!   transcript, only replace token spellings),
//! - a name dictionary produced by `ab-companion-extract` (slice
//!   C.4),
//! - a max normalised-distance ratio,
//!
//! produce a corrected transcript where near-miss tokens are
//! replaced by their canonical name-dict surface form.
//! Sentence-initial casing of the original is preserved (per
//! ADR-0043 § C.5 step 4).
//!
//! ## Gating
//!
//! The ADR gates C.5 on three conditions:
//!
//! 1. `books.abridged != true`
//! 2. `books.language == epub.language`
//! 3. Non-empty name dictionary
//!
//! All three are caller responsibilities. This crate is
//! unconditional: bad inputs (empty dict, empty transcript,
//! whatever) just produce the identity transformation.
//!
//! ## Algorithm
//!
//! 1. Tokenise the transcript via [`unicode_segmentation`] word
//!    boundaries, preserving every separator verbatim so output
//!    spacing matches input.
//! 2. For each capitalised word-token, score against every
//!    dict surface (`strsim::normalized_levenshtein`).
//! 3. Replace with the highest-scoring surface whose
//!    distance ratio is `<= max_ratio` and where the surface
//!    isn't already an exact match (no-op replacements waste
//!    cycles + risk casing flips).
//! 4. Preserve sentence-initial title-case if the original
//!    token was sentence-initial.
//!
//! Multi-token dict entries ("Kaladin Stormblessed") are
//! filtered out for this iteration — the matcher works
//! single-token only. Multi-token sliding-window matching
//! is a follow-up slice. The single-token form covers the
//! main "Caladin → Kaladin" case the ADR targets.

use std::collections::HashSet;

use strsim::normalized_levenshtein;
use unicode_segmentation::UnicodeSegmentation;

/// One name dictionary entry as produced by `ab-companion-extract`.
///
/// Re-declared here as a local type to keep this crate
/// dependency-free of the C.4 module — callers convert in
/// whichever direction is convenient.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictEntry {
    /// Canonical surface form (single token; multi-token entries
    /// are ignored by [`correct_transcript`]).
    pub surface: String,
}

impl DictEntry {
    /// Construct from any string-like value.
    pub fn new(surface: impl Into<String>) -> Self {
        Self {
            surface: surface.into(),
        }
    }
}

/// ADR-0043 § C.5 max-ratio default.
///
/// The score `1.0 - ratio` is the normalised Levenshtein
/// similarity floor — pairs scoring below it never replace.
/// `0.3` lets "Caladin" → "Kaladin" (one edit on a 7-letter
/// word → ~0.14 ratio) but blocks "Adolin" → "Kaladin"
/// (~0.71 ratio).
pub const DEFAULT_MAX_RATIO: f64 = 0.3;

/// Apply the C.5 correction in-place over a transcript string.
///
/// `dict` is the raw output from C.4 (single-token entries
/// participate; multi-token are ignored, see crate-level docs).
/// `max_ratio` is the normalised-distance ceiling for a
/// replacement — pass [`DEFAULT_MAX_RATIO`] for the ADR default.
///
/// Returns a freshly allocated `String`; the original is
/// untouched. The C.5 pipeline stage will write the result to
/// `books.transcript_corrected`.
#[must_use]
pub fn correct_transcript(transcript: &str, dict: &[DictEntry], max_ratio: f64) -> String {
    // Filter to single-token surface forms. The C.4 output is a
    // mix; multi-token entries get ignored here per the
    // single-token-only scope of this iteration.
    let surfaces: Vec<&str> = dict
        .iter()
        .map(|e| e.surface.as_str())
        .filter(|s| !s.contains(' ') && !s.is_empty())
        .collect();
    if surfaces.is_empty() || transcript.is_empty() {
        return transcript.to_owned();
    }
    // Exact-match set short-circuits the per-token scoring loop
    // — exact matches never need correction.
    let exact: HashSet<&str> = surfaces.iter().copied().collect();
    let min_similarity = 1.0 - max_ratio;

    let mut out = String::with_capacity(transcript.len());
    let mut prev_was_sentence_end = true; // start of doc

    for tok in transcript.split_word_bounds() {
        let trimmed = tok.trim();
        if trimmed.is_empty() {
            out.push_str(tok);
            continue;
        }
        if is_sentence_terminator(trimmed) {
            out.push_str(tok);
            prev_was_sentence_end = true;
            continue;
        }
        if !is_word_token(trimmed) {
            out.push_str(tok);
            continue;
        }
        if !starts_with_upper(trimmed) || exact.contains(trimmed) {
            out.push_str(tok);
            prev_was_sentence_end = false;
            continue;
        }
        // Score against every single-token surface; pick the
        // best within ratio.
        let mut best: Option<(&str, f64)> = None;
        for surface in &surfaces {
            let score = normalized_levenshtein(trimmed, surface);
            if score >= min_similarity && best.is_none_or(|(_, bs)| score > bs) {
                best = Some((surface, score));
            }
        }
        if let Some((surface, _)) = best {
            // Match the whitespace prefix/suffix of the original
            // word-bound token so spacing/punctuation around the
            // word stays exact.
            let (prefix, suffix) = split_outer_whitespace(tok);
            out.push_str(prefix);
            out.push_str(&apply_sentence_initial_casing(
                surface,
                prev_was_sentence_end,
            ));
            out.push_str(suffix);
        } else {
            out.push_str(tok);
        }
        prev_was_sentence_end = false;
    }
    out
}

fn is_sentence_terminator(tok: &str) -> bool {
    matches!(tok, "." | "!" | "?" | "\n" | "…")
}

fn is_word_token(tok: &str) -> bool {
    tok.chars().any(char::is_alphabetic)
}

fn starts_with_upper(tok: &str) -> bool {
    tok.chars().next().is_some_and(char::is_uppercase)
}

/// All name-dict surfaces are stored title-case (per C.4
/// capitalisation filter), so the only casing decision left is
/// "did the original sit at sentence start, in which case keep
/// it title-case anyway?" Currently a no-op since both branches
/// emit the surface verbatim — extension point if the ADR ever
/// adds an all-caps "emphatic" handling.
fn apply_sentence_initial_casing(surface: &str, _sentence_initial: bool) -> String {
    surface.to_owned()
}

/// Split a word-bound token into (leading-whitespace, body,
/// trailing-whitespace). The body is what we replaced; the
/// whitespace bookends come from the original tok. Implementation
/// note: `split_word_bounds` yields tokens with no internal
/// whitespace, so in practice prefix + suffix are both empty for
/// the word-token case. The helper exists so multi-codepoint
/// punctuation handling is local to this function — easier to
/// extend later.
fn split_outer_whitespace(tok: &str) -> (&str, &str) {
    let trimmed_left = tok.trim_start();
    let left_len = tok.len() - trimmed_left.len();
    let trimmed_right = trimmed_left.trim_end();
    let right_len = trimmed_left.len() - trimmed_right.len();
    let (prefix, rest) = tok.split_at(left_len);
    let (_, suffix) = rest.split_at(rest.len() - right_len);
    (prefix, suffix)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn dict(words: &[&str]) -> Vec<DictEntry> {
        words.iter().map(|w| DictEntry::new(*w)).collect()
    }

    #[test]
    fn near_miss_token_is_replaced() {
        let transcript = "He saw Caladin walk away.";
        let out = correct_transcript(transcript, &dict(&["Kaladin"]), DEFAULT_MAX_RATIO);
        assert!(
            out.contains("Kaladin"),
            "expected Caladin -> Kaladin, got {out:?}",
        );
        assert!(!out.contains("Caladin"), "old surface should be gone");
    }

    #[test]
    fn far_miss_token_is_left_alone() {
        // "Adolin" vs "Kaladin" — distance > 0.3 ratio.
        let transcript = "Adolin watched.";
        let out = correct_transcript(transcript, &dict(&["Kaladin"]), DEFAULT_MAX_RATIO);
        assert_eq!(out, "Adolin watched.");
    }

    #[test]
    fn exact_match_is_passed_through_unchanged() {
        let transcript = "Kaladin spoke.";
        let out = correct_transcript(transcript, &dict(&["Kaladin"]), DEFAULT_MAX_RATIO);
        assert_eq!(out, "Kaladin spoke.");
    }

    #[test]
    fn empty_dict_is_identity() {
        let transcript = "Whatever Caladin words.";
        let out = correct_transcript(transcript, &[], DEFAULT_MAX_RATIO);
        assert_eq!(out, transcript);
    }

    #[test]
    fn empty_transcript_returns_empty() {
        let out = correct_transcript("", &dict(&["Kaladin"]), DEFAULT_MAX_RATIO);
        assert_eq!(out, "");
    }

    #[test]
    fn lowercase_token_is_never_replaced_even_if_close() {
        // "caladin" lowercased — not a proper-noun candidate;
        // skip even though normalised distance is the same.
        let transcript = "the word caladin appears.";
        let out = correct_transcript(transcript, &dict(&["Kaladin"]), DEFAULT_MAX_RATIO);
        assert_eq!(out, "the word caladin appears.");
    }

    #[test]
    fn multi_token_dict_entries_are_ignored() {
        // The dict has "Kaladin Stormblessed" — that's multi-token
        // and out of scope for this iteration; single-token
        // "Caladin" should NOT match it.
        let transcript = "Caladin walked.";
        let out = correct_transcript(
            transcript,
            &dict(&["Kaladin Stormblessed"]),
            DEFAULT_MAX_RATIO,
        );
        assert_eq!(out, "Caladin walked.");
    }

    #[test]
    fn replacement_picks_closest_match() {
        // Dict has both "Kaladin" and "Paladin"; "Caladin" is
        // closer to "Kaladin" (1 edit) than "Paladin" (1 edit).
        // Either is acceptable — but with two equally close
        // candidates we deterministically take the first that
        // ties (whichever scores ≥ best). Verify a sensible one
        // is picked.
        let transcript = "Caladin walked.";
        let out = correct_transcript(
            transcript,
            &dict(&["Kaladin", "Paladin"]),
            DEFAULT_MAX_RATIO,
        );
        assert!(out.contains("Kaladin") || out.contains("Paladin"));
        assert!(!out.contains("Caladin"));
    }

    #[test]
    fn sentence_initial_replacement_keeps_title_case() {
        // Replacement surface is already title-case; sentence-
        // initial position doesn't change that. Spot-check we
        // don't lowercase it.
        let transcript = "Caladin walked.";
        let out = correct_transcript(transcript, &dict(&["Kaladin"]), DEFAULT_MAX_RATIO);
        assert!(out.starts_with("Kaladin"));
    }

    #[test]
    fn whitespace_and_punctuation_around_replacement_preserved() {
        let transcript = "She said, \"Caladin?\" and stood.";
        let out = correct_transcript(transcript, &dict(&["Kaladin"]), DEFAULT_MAX_RATIO);
        assert!(out.contains("\"Kaladin?\""), "got {out:?}");
    }

    #[test]
    fn tight_ratio_blocks_more_replacements() {
        // ratio 0.05 → only ~exact matches survive. "Caladin" is
        // ~0.14 ratio from "Kaladin" → blocked at 0.05.
        let transcript = "Caladin walked.";
        let out = correct_transcript(transcript, &dict(&["Kaladin"]), 0.05);
        assert_eq!(out, "Caladin walked.");
    }
}
