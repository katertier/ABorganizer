//! Static publisher-phrase const list for tier-4 transcript scan
//! (slice 4C; ADR-0024 § Tier-4 vocabulary).
//!
//! Each entry pairs a normalized phrase ("audible studios presents")
//! with the publisher it's associated with. Tier-4 candidates that
//! light up here surface the publisher in the candidate row so the
//! review UI can prefer matches from the book's known publisher.
//!
//! Match is **case-insensitive substring**. The phrases are stored
//! lowercased; callers lowercase the transcript text before
//! searching.
//!
//! Adding a new phrase: append below + add a unit test in this
//! module's tests. The list is intentionally small — empirical
//! re-evaluation per the ADR's "Verification" section will widen
//! it once real-world data lands.

/// One entry in the phrase list.
#[derive(Debug, Clone, Copy)]
pub struct Phrase {
    /// Lowercased substring to search for.
    pub text: &'static str,
    /// Publisher identifier — typically a stable canonical name.
    /// Surfaced in the candidate row so review UI can prefer
    /// matches whose publisher matches the book's known one.
    pub publisher: &'static str,
    /// Language hint (BCP-47). Currently informational; future
    /// localisation may filter the phrase list by
    /// `books.language` to reduce false-positive cross-lingual
    /// matches.
    pub lang: &'static str,
}

/// The phrase const list. Add to this — never reorder — so test
/// references stay stable.
pub const PHRASES: &[Phrase] = &[
    // ── English ─────────────────────────────────────────────
    Phrase {
        text: "audible studios presents",
        publisher: "Audible Studios",
        lang: "en",
    },
    Phrase {
        text: "audible original",
        publisher: "Audible Studios",
        lang: "en",
    },
    Phrase {
        text: "a recorded books production",
        publisher: "Recorded Books",
        lang: "en",
    },
    Phrase {
        text: "this is a tantor audio production",
        publisher: "Tantor Audio",
        lang: "en",
    },
    Phrase {
        text: "blackstone audio presents",
        publisher: "Blackstone Audio",
        lang: "en",
    },
    Phrase {
        text: "penguin random house audio presents",
        publisher: "Penguin Random House Audio",
        lang: "en",
    },
    Phrase {
        text: "macmillan audio",
        publisher: "Macmillan Audio",
        lang: "en",
    },
    Phrase {
        text: "harpercollins publishers",
        publisher: "HarperCollins",
        lang: "en",
    },
    Phrase {
        text: "simon and schuster audio presents",
        publisher: "Simon & Schuster Audio",
        lang: "en",
    },
    Phrase {
        text: "hachette audio",
        publisher: "Hachette Audio",
        lang: "en",
    },
    // ── German ──────────────────────────────────────────────
    Phrase {
        text: "audible studios präsentiert",
        publisher: "Audible Studios",
        lang: "de",
    },
    Phrase {
        text: "hörbuch hamburg präsentiert",
        publisher: "Hörbuch Hamburg",
        lang: "de",
    },
    Phrase {
        text: "lübbe audio",
        publisher: "Lübbe Audio",
        lang: "de",
    },
    Phrase {
        text: "argon hörbuch",
        publisher: "Argon Verlag",
        lang: "de",
    },
    // ── French ──────────────────────────────────────────────
    Phrase {
        text: "audible studios présente",
        publisher: "Audible Studios",
        lang: "fr",
    },
    Phrase {
        text: "audiolib présente",
        publisher: "Audiolib",
        lang: "fr",
    },
    // ── Spanish ─────────────────────────────────────────────
    Phrase {
        text: "audible studios presenta",
        publisher: "Audible Studios",
        lang: "es",
    },
    Phrase {
        text: "una producción de penguin random house",
        publisher: "Penguin Random House Audio",
        lang: "es",
    },
];

/// Match-result for a phrase hit.
#[derive(Debug, Clone, Copy)]
pub struct PhraseHit<'a> {
    /// Reference to the matched phrase.
    pub phrase: &'a Phrase,
    /// Byte offset of the match inside the searched text (the
    /// already-lowercased haystack).
    pub byte_offset: usize,
}

/// Find the earliest phrase hit in `text_lowercased`.
///
/// `text_lowercased` must already be lowercase (caller's
/// responsibility — keeps the const list pure and avoids
/// re-allocating on every scan). Returns `None` if no phrase
/// matches. Ties resolve by earliest position; phrase-list order
/// breaks remaining ties.
#[must_use]
pub fn first_phrase_hit(text_lowercased: &str) -> Option<PhraseHit<'static>> {
    let mut best: Option<PhraseHit<'static>> = None;
    for p in PHRASES {
        if let Some(pos) = text_lowercased.find(p.text) {
            if best.as_ref().is_none_or(|b| pos < b.byte_offset) {
                best = Some(PhraseHit {
                    phrase: p,
                    byte_offset: pos,
                });
            }
        }
    }
    best
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn phrase_list_is_nonempty_and_lowercased() {
        assert!(!PHRASES.is_empty(), "phrase list must have entries");
        for p in PHRASES {
            assert_eq!(
                p.text,
                p.text.to_lowercase(),
                "phrase `{}` not lowercased",
                p.text
            );
            assert!(!p.text.is_empty(), "empty phrase");
            assert!(!p.publisher.is_empty(), "empty publisher");
            assert!(!p.lang.is_empty(), "empty lang");
        }
    }

    #[test]
    fn first_hit_returns_none_on_unrelated_text() {
        // English text with no publisher phrases. lowercased
        // already.
        let t = "the quick brown fox jumps over the lazy dog";
        assert!(first_phrase_hit(t).is_none());
    }

    #[test]
    fn first_hit_finds_audible_studios() {
        let t = "audible studios presents foundation by isaac asimov";
        let hit = first_phrase_hit(t).expect("hit");
        assert_eq!(hit.phrase.publisher, "Audible Studios");
        assert_eq!(hit.byte_offset, 0);
    }

    #[test]
    fn first_hit_picks_earliest_match() {
        let t = "macmillan audio audible studios presents";
        let hit = first_phrase_hit(t).expect("hit");
        // `macmillan audio` starts at 0; `audible studios presents`
        // starts at 16. macmillan wins.
        assert_eq!(hit.phrase.publisher, "Macmillan Audio");
    }

    #[test]
    fn first_hit_works_on_german() {
        let t = "audible studios präsentiert: foundation";
        let hit = first_phrase_hit(t).expect("hit");
        assert_eq!(hit.phrase.lang, "de");
    }

    #[test]
    fn first_hit_works_on_substring_match() {
        // Phrase embedded mid-text. Should still hit at the
        // byte-offset of "audible studios presents". "welcome —
        // " counts as 12 bytes because the em-dash is 3 UTF-8
        // bytes (E2 80 94).
        let t = "welcome — audible studios presents the new title";
        let hit = first_phrase_hit(t).expect("hit");
        assert_eq!(hit.byte_offset, 12);
        assert_eq!(hit.phrase.publisher, "Audible Studios");
    }
}
