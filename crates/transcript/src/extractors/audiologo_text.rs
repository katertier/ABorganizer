//! Tier-4 audiologo detection: pure-transcript keyword match.
//!
//! Where it fits in the 4-tier cascade
//! (see PROJECT.md "Audiologo cascade"):
//!
//! 1. Full-jingle fingerprint match (ab-audiologo crate).
//! 2. Beginning / end fingerprint variants.
//! 3. Fingerprint + transcript corroboration.
//! 4. **Pure transcript** ← this extractor.
//!
//! Tier-4 catches publisher jingles whose audio fingerprint
//! we haven't catalogued yet but whose spoken text is
//! distinctive ("Audible.com presents…", "This is Tantor
//! Media…"). Confidence is intentionally modest: a single
//! keyword match in the jingle window is good evidence but
//! not as strong as a fingerprint hit.
//!
//! Detection scope: the keyword set is intentionally small —
//! adding entries is cheap, but every entry costs one regex
//! pass. The `ABtagger` publisher list is the source of truth;
//! mirror new entries from there.

use std::sync::OnceLock;

use crate::{Candidate, Extractor};

/// Stable [`Extractor::name`] value.
pub const NAME: &str = "transcript_publisher";

/// Confidence assigned to a tier-4 match. Empirically lower
/// than fingerprint-based tiers (which run ~98% precision) but
/// well above tag-derived hints. Tuned in PROJECT.md
/// "Audiologo confidence tiers."
const MATCH_CONFIDENCE: f32 = 0.75;

/// One publisher's signature.
struct Signature {
    /// Canonical publisher name written to provenance. Used
    /// downstream as `book_field_provenance.value`.
    canonical: &'static str,
    /// Lowercased phrases that match. Matching is
    /// case-insensitive substring; phrases are expected to be
    /// 3+ words to avoid false positives on common nouns.
    phrases: &'static [&'static str],
}

fn signatures() -> &'static [Signature] {
    static SIGS: OnceLock<Vec<Signature>> = OnceLock::new();
    SIGS.get_or_init(|| {
        vec![
            Signature {
                canonical: "Audible",
                phrases: &[
                    "audible.com presents",
                    "this is audible",
                    "an audible original",
                    "audible studios",
                ],
            },
            Signature {
                canonical: "Tantor Media",
                phrases: &["tantor media", "this is tantor", "tantor audio"],
            },
            Signature {
                canonical: "Blackstone Audio",
                phrases: &["blackstone audio", "blackstone publishing"],
            },
            Signature {
                canonical: "Recorded Books",
                phrases: &["recorded books"],
            },
            Signature {
                canonical: "Podium Audio",
                phrases: &["podium audio", "podium audiobooks"],
            },
            Signature {
                canonical: "Macmillan Audio",
                phrases: &["macmillan audio"],
            },
            Signature {
                canonical: "Brilliance Audio",
                phrases: &["brilliance audio", "brilliance publishing"],
            },
            Signature {
                canonical: "Penguin Random House Audio",
                phrases: &["penguin random house audio", "random house audio"],
            },
            Signature {
                canonical: "Harper Audio",
                phrases: &["harperaudio", "harper audio"],
            },
            Signature {
                canonical: "Simon & Schuster Audio",
                phrases: &["simon and schuster audio", "simon & schuster audio"],
            },
            Signature {
                canonical: "Hachette Audio",
                phrases: &["hachette audio", "hachette book group"],
            },
            Signature {
                canonical: "W. F. Howes",
                phrases: &["w. f. howes", "w f howes"],
            },
        ]
    })
}

/// Tier-4 publisher extractor.
pub struct AudiologoTextExtractor;

impl AudiologoTextExtractor {
    /// Construct.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for AudiologoTextExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl Extractor for AudiologoTextExtractor {
    fn name(&self) -> &'static str {
        NAME
    }

    fn extract(&self, transcript: &str) -> Vec<Candidate> {
        let lower = transcript.to_lowercase();
        let mut seen: Vec<&'static str> = Vec::new();
        let mut out = Vec::new();
        for sig in signatures() {
            if seen.contains(&sig.canonical) {
                continue;
            }
            if sig.phrases.iter().any(|p| lower.contains(p)) {
                seen.push(sig.canonical);
                out.push(Candidate {
                    field: "publisher".into(),
                    value: sig.canonical.to_owned(),
                    confidence: MATCH_CONFIDENCE,
                });
            }
        }
        out
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn audible_phrase_matches() {
        let ex = AudiologoTextExtractor::new();
        let text = "Audible.com presents The Eyre Affair, by Jasper Fforde.";
        let out = ex.extract(text);
        assert!(out.iter().any(|c| c.value == "Audible"), "{out:?}");
    }

    #[test]
    fn tantor_phrase_matches_case_insensitive() {
        let ex = AudiologoTextExtractor::new();
        let text = "This is TANTOR MEDIA proudly presenting...";
        let out = ex.extract(text);
        assert!(out.iter().any(|c| c.value == "Tantor Media"), "{out:?}");
    }

    #[test]
    fn unknown_publisher_no_match() {
        let ex = AudiologoTextExtractor::new();
        let text = "Welcome to chapter one. Once upon a time...";
        let out = ex.extract(text);
        assert!(out.is_empty(), "expected no matches, got {out:?}");
    }

    #[test]
    fn deduplicates_canonical_name() {
        // Same canonical matched via two phrases in one
        // transcript → one candidate.
        let ex = AudiologoTextExtractor::new();
        let text = "Audible.com presents... This is Audible.";
        let out = ex.extract(text);
        let audible_count = out.iter().filter(|c| c.value == "Audible").count();
        assert_eq!(audible_count, 1, "{out:?}");
    }
}
