//! Heuristic title / author / narrator extractor over the head
//! transcript.
//!
//! Looks for canonical audiobook-intro phrases:
//!
//! - `... by <Author>` — primary authorship signal. Common
//!   immediately after the title is spoken.
//! - `read by <Narrator>` / `narrated by <Narrator>` /
//!   `performed by <Narrator>` — narrator signal.
//! - `This is <Title>` / `<Title>, written by ...` — title
//!   signal (less reliable; the title often runs into the
//!   author phrase, so the regex is conservative).
//!
//! Each match becomes a [`crate::Candidate`] with
//! `source` set by the wrapping stage. Confidence is fixed per
//! pattern based on empirical precision in `ABtagger` — the
//! consensus stage promotes the highest-confidence value when
//! multiple sources agree.

// The three `Regex::new(...).expect(...)` calls in this file
// compile static patterns at first use; failure would mean an
// invariant violation (developer typo in a const pattern caught
// by the unit tests), not a runtime concern. Project style
// allows `.expect()` for exactly this case — the lint is too
// broad. See `RTK.md` "Errors → .expect() for invariant
// violations."
#![allow(clippy::expect_used)]

use std::sync::OnceLock;

use regex::Regex;

use crate::{Candidate, Extractor};

/// Stable [`Extractor::name`] value — surfaces in
/// `book_field_provenance.source`.
pub const NAME: &str = "transcript_title_author";

/// Confidence assigned to `... by <Author>` matches. Tuned on
/// the `ABtagger` corpus: ~95% precision on books with the phrase
/// in the first 6 minutes; the remaining 5% are catalogues
/// where the announcer reads multiple author names. Consensus
/// can override when a higher-confidence source (Audnexus,
/// Audible) disagrees.
const AUTHOR_CONFIDENCE: f32 = 0.85;

/// Confidence for narrator matches — usually more reliable than
/// author because the phrasing is more constrained
/// ("narrated by X" vs. "by X" which can also follow chapter
/// titles).
const NARRATOR_CONFIDENCE: f32 = 0.90;

/// Confidence for title matches — least reliable; titles get
/// truncated by the regex boundary and run into authorship.
const TITLE_CONFIDENCE: f32 = 0.70;

/// Heuristic regex-based title / author / narrator extractor.
pub struct TitleAuthorExtractor {
    by_author: &'static Regex,
    narrated_by: &'static Regex,
    this_is_title: &'static Regex,
}

impl TitleAuthorExtractor {
    /// Construct. Compiled regexes are `OnceLock`-cached so
    /// re-instantiating the extractor is free.
    #[must_use]
    pub fn new() -> Self {
        Self {
            by_author: by_author_regex(),
            narrated_by: narrated_by_regex(),
            this_is_title: this_is_title_regex(),
        }
    }
}

impl Default for TitleAuthorExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl Extractor for TitleAuthorExtractor {
    fn name(&self) -> &'static str {
        NAME
    }

    fn extract(&self, transcript: &str) -> Vec<Candidate> {
        let mut out = Vec::new();
        for c in self.by_author.captures_iter(transcript).take(2) {
            if let Some(m) = c.get(1) {
                let name = sanitize_name(m.as_str());
                if !name.is_empty() {
                    out.push(Candidate {
                        field: ab_core::Field::Author,
                        value: name,
                        confidence: AUTHOR_CONFIDENCE,
                    });
                }
            }
        }
        for c in self.narrated_by.captures_iter(transcript).take(2) {
            if let Some(m) = c.get(1) {
                let name = sanitize_name(m.as_str());
                if !name.is_empty() {
                    out.push(Candidate {
                        field: ab_core::Field::Narrator,
                        value: name,
                        confidence: NARRATOR_CONFIDENCE,
                    });
                }
            }
        }
        for c in self.this_is_title.captures_iter(transcript).take(1) {
            if let Some(m) = c.get(1) {
                let title = sanitize_name(m.as_str());
                if !title.is_empty() {
                    out.push(Candidate {
                        field: ab_core::Field::Title,
                        value: title,
                        confidence: TITLE_CONFIDENCE,
                    });
                }
            }
        }
        out
    }
}

/// Trim whitespace, strip a trailing period, and reject results
/// that look like sentence runoff rather than a name (too long,
/// contains "and"-joined lists, etc.).
fn sanitize_name(raw: &str) -> String {
    let s = raw
        .trim()
        .trim_end_matches(['.', ',', ';'])
        .trim()
        .to_owned();
    // Empirically a name is 2-50 chars. Longer captures are
    // almost always the regex eating into the next sentence.
    if s.is_empty() || s.len() > 50 {
        return String::new();
    }
    // Reject obvious runoff signals.
    let lower = s.to_lowercase();
    if lower.contains(" and the ") || lower.contains(" chapter ") || lower.contains(" prologue") {
        return String::new();
    }
    s
}

/// Character class for "inside-a-name" — a single letter, an
/// apostrophe or hyphen, OR a period strictly followed by an
/// alpha char (initial pattern like `J.R.R.`). Excluding the
/// terminal `. ` (period-space) is what prevents capturing
/// runoff into the next sentence — e.g. `Fforde. Chapter` no
/// longer matches as a single name.
const NAME_TOKEN: &str = r"[A-Z](?:[A-Za-z'\-]|\.[A-Za-z])*";

fn by_author_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        let pattern = format!(r"(?m)\bby\s+({NAME_TOKEN}(?:\s+{NAME_TOKEN}){{0,4}})");
        Regex::new(&pattern).expect("static regex")
    })
}

fn narrated_by_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        let pattern = format!(
            r"(?im)\b(?:narrated|read|performed|voiced)\s+by\s+({NAME_TOKEN}(?:\s+{NAME_TOKEN}){{0,4}})"
        );
        Regex::new(&pattern).expect("static regex")
    })
}

fn this_is_title_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        // "This is <Title>, ..." where <Title> starts with a
        // Capital letter — rejects "this is a long ramble,"
        // (lowercase runoff). The case-sensitivity-flipping
        // `(?i:..)` group makes only `This is` case-insensitive;
        // the `[A-Z]` boundary on the title remains strict.
        Regex::new(r"\b(?i:This is)\s+([A-Z][^,.;]{1,79})[,.;]").expect("static regex")
    })
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn names(out: &[Candidate], field: ab_core::Field) -> Vec<String> {
        out.iter()
            .filter(|c| c.field == field)
            .map(|c| c.value.clone())
            .collect()
    }

    #[test]
    fn author_captured() {
        let ex = TitleAuthorExtractor::new();
        let text = "Welcome to The Eyre Affair, by Jasper Fforde. Chapter one.";
        let out = ex.extract(text);
        let authors = names(&out, ab_core::Field::Author);
        assert!(authors.contains(&"Jasper Fforde".to_owned()), "{out:?}");
    }

    #[test]
    fn narrator_captured() {
        let ex = TitleAuthorExtractor::new();
        let text = "Read by Hugh Fraser. A Hercule Poirot mystery by Agatha Christie.";
        let out = ex.extract(text);
        let narrators = names(&out, ab_core::Field::Narrator);
        let authors = names(&out, ab_core::Field::Author);
        assert!(narrators.contains(&"Hugh Fraser".to_owned()), "{out:?}");
        assert!(authors.contains(&"Agatha Christie".to_owned()), "{out:?}");
    }

    #[test]
    fn this_is_title() {
        let ex = TitleAuthorExtractor::new();
        let text = "This is The Hobbit, by J.R.R. Tolkien.";
        let out = ex.extract(text);
        let titles = names(&out, ab_core::Field::Title);
        assert!(titles.contains(&"The Hobbit".to_owned()), "{out:?}");
    }

    #[test]
    fn rejects_sentence_runoff() {
        // "by" followed by lowercase shouldn't match anything;
        // and even the capitalized match shouldn't include
        // chapter / prologue tail.
        let ex = TitleAuthorExtractor::new();
        let text = "By the way, this is a long ramble. Chapter one begins with prologue text.";
        let out = ex.extract(text);
        assert!(out.is_empty(), "expected no candidates, got {out:?}");
    }

    #[test]
    fn caps_per_field_count() {
        // Even with a verbose transcript, we cap the result list.
        let ex = TitleAuthorExtractor::new();
        let text = "By Author One. By Author Two. By Author Three. Read by Narrator Alpha. Read by Narrator Beta. Read by Narrator Gamma.";
        let out = ex.extract(text);
        assert!(names(&out, ab_core::Field::Author).len() <= 2);
        assert!(names(&out, ab_core::Field::Narrator).len() <= 2);
    }
}
