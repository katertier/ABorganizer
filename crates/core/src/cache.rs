//! Typed identifier for `ai_cache.cache_type`.
//!
//! Every cache producer (transcribe stages, LLM extractor
//! stages) writes one of a closed set of values. Keeping that
//! set in one place — and using it through a typed enum
//! instead of inline `"transcript_head"` literals — catches the
//! "stage typoed its cache key" class of bug at compile time.
//! With strings, a producer can write `"transcribe_head"` (note
//! the extra 'e'), the freshness-check still reads
//! `"transcript_head"`, and the stage re-runs every scheduler
//! tick forever. The enum makes that impossible.
//!
//! Add a new cache type: add a variant here, point its
//! `as_str()` arm at the chosen string, and update any
//! freshness checks. The enum is the single source of truth.

use serde::{Deserialize, Serialize};

/// One row of `ai_cache.cache_type`.
///
/// Variants enumerate every cache producer the workspace
/// currently knows about. The `Display` + `AsRef<str>` impls
/// produce the string that lives in the DB column — bind these
/// directly into `sqlx::query!` params.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheKey {
    /// First-N-seconds transcript (default 360 s). Written by
    /// the `transcribe-head-tail` stage; consumed by the
    /// language-detect post-pass, the transcript extractor
    /// stage, and downstream LLM extractors.
    TranscriptHead,
    /// Last-N-seconds transcript (default 30 s). Written by
    /// the `transcribe-head-tail` stage; consumed by the
    /// outro audiologo + last-sentence-boundary work.
    TranscriptTail,
    /// Three short windows at 25 / 50 / 75 % of book duration.
    /// Written by `transcribe-samples`; primary source of
    /// post-transcribe language confirmation.
    TranscriptSamples,
    /// Whole-book transcript, concatenated across files.
    /// Written by `transcribe-full` (Idle priority); consumed
    /// by every LLM extractor that wants context beyond the
    /// first 6 minutes.
    TranscriptFull,
    /// Raw LLM response for the DNA-tag extractor. Promoted to
    /// `#`-prefixed and `!`-prefixed rows in `book_tags`.
    DnaTags,
    /// Raw LLM response for the spoiler-free summary
    /// extractor. Promoted to `books.summary_spoiler_free` +
    /// `books.summary_spoiler_free_lang`.
    SummarySpoilerFree,
    /// Raw LLM response for the story-arc extractor. Promoted
    /// to `books.story_arc_json` (JSON array of step records).
    StoryArc,
    /// Raw LLM response for the character extractor. Promoted
    /// to rows in the `characters` table.
    Characters,
}

impl CacheKey {
    /// The exact string written into `ai_cache.cache_type`.
    /// Use this for `sqlx::query!` bind params and for any
    /// API-level introspection (`aborg doctor` etc.).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TranscriptHead => "transcript_head",
            Self::TranscriptTail => "transcript_tail",
            Self::TranscriptSamples => "transcript_samples",
            Self::TranscriptFull => "transcript_full",
            Self::DnaTags => "dna_tags",
            Self::SummarySpoilerFree => "summary_spoiler_free",
            Self::StoryArc => "story_arc",
            Self::Characters => "characters",
        }
    }

    /// Parse the `cache_type` column back into the typed enum.
    /// Returns `None` for unknown strings — callers can treat
    /// those as legacy / not-our-key. Named `parse` (not
    /// `from_str`) to avoid colliding with the `FromStr` trait
    /// method, which would force an `Err` type we don't want
    /// for an "unknown is fine" lookup.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "transcript_head" => Some(Self::TranscriptHead),
            "transcript_tail" => Some(Self::TranscriptTail),
            "transcript_samples" => Some(Self::TranscriptSamples),
            "transcript_full" => Some(Self::TranscriptFull),
            "dna_tags" => Some(Self::DnaTags),
            "summary_spoiler_free" => Some(Self::SummarySpoilerFree),
            "story_arc" => Some(Self::StoryArc),
            "characters" => Some(Self::Characters),
            _ => None,
        }
    }
}

impl std::fmt::Display for CacheKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for CacheKey {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_every_variant() {
        // Pinning the closed set: when a new variant lands,
        // this test fails until the round-trip is added.
        for key in [
            CacheKey::TranscriptHead,
            CacheKey::TranscriptTail,
            CacheKey::TranscriptSamples,
            CacheKey::TranscriptFull,
            CacheKey::DnaTags,
            CacheKey::SummarySpoilerFree,
            CacheKey::StoryArc,
            CacheKey::Characters,
        ] {
            let s = key.as_str();
            assert_eq!(CacheKey::parse(s), Some(key), "round-trip {s}");
        }
    }

    #[test]
    fn unknown_strings_return_none() {
        assert_eq!(CacheKey::parse(""), None);
        assert_eq!(CacheKey::parse("transcribe_head"), None); // common typo
        assert_eq!(CacheKey::parse("TRANSCRIPT_HEAD"), None); // case-sensitive
    }

    #[test]
    fn display_matches_as_str() {
        let key = CacheKey::DnaTags;
        assert_eq!(format!("{key}"), "dna_tags");
    }
}
