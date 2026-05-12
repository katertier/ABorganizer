//! Tag prefix convention — single source of truth for the
//! `@` / `#` / `!` characters that classify rows in
//! `book_tags.tag`.
//!
//! Background: the storage layer is identical for all three
//! prefixes — `book_tags(book_id, tag, source)`. The prefix
//! lives inside the `tag` string. Filter / export semantics
//! (e.g. "hide spoilers in the player UI by default") read the
//! first character and branch.
//!
//! Why constants and not just inline `format!("#{}", …)`:
//! every extractor that emits a DNA tag spells the prefix
//! inline. With four LLM extractors + the catalog/genre
//! pipeline + the spoiler-aware UI all writing tags, a typo
//! (`%` instead of `#`, missing prefix entirely) is silent —
//! the row writes, no SQL constraint catches it, and the UI's
//! filter just doesn't fire for that row. A single set of
//! `const` characters here turns the typo into a compile-time
//! error.

/// Prefix character for genre tags (`@fantasy`, `@thriller`).
/// Each `@`-prefixed tag mirrors a row in the `genres` table
/// for canonical display name + hierarchy.
pub const TAG_PREFIX_GENRE: char = '@';

/// Prefix character for DNA tags safe to display to readers
/// who haven't started the book (`#cozy`,
/// `#unreliable-narrator`, `#commute-friendly`).
pub const TAG_PREFIX_DNA: char = '#';

/// Prefix character for spoiler-bearing DNA tags
/// (`!hero-dies`, `!magic-system-revealed`). Hidden by default
/// in player / API output; always stored for similarity
/// queries.
pub const TAG_PREFIX_SPOILER: char = '!';

/// Tag category classified by the prefix in `book_tags.tag`.
///
/// Returned by [`TagKind::from_tag`] — useful when reading
/// rows back and branching on the category (e.g. the player's
/// "hide spoilers" filter).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TagKind {
    /// `@`-prefixed.
    Genre,
    /// `#`-prefixed.
    Dna,
    /// `!`-prefixed.
    Spoiler,
}

impl TagKind {
    /// Prefix character for this kind.
    #[must_use]
    pub const fn prefix(self) -> char {
        match self {
            Self::Genre => TAG_PREFIX_GENRE,
            Self::Dna => TAG_PREFIX_DNA,
            Self::Spoiler => TAG_PREFIX_SPOILER,
        }
    }

    /// Classify a stored `book_tags.tag` string by its prefix.
    /// Returns `None` when the first character isn't one of
    /// the three known prefixes — those are treated as legacy
    /// untyped tags.
    #[must_use]
    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag.chars().next()? {
            TAG_PREFIX_GENRE => Some(Self::Genre),
            TAG_PREFIX_DNA => Some(Self::Dna),
            TAG_PREFIX_SPOILER => Some(Self::Spoiler),
            _ => None,
        }
    }

    /// Format a body (already slug-normalised) with this
    /// kind's prefix. Use this at every write site — never
    /// `format!("#{body}")` inline.
    #[must_use]
    pub fn format_tag(self, body: &str) -> String {
        format!("{}{body}", self.prefix())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_constants_match_kind_method() {
        assert_eq!(TagKind::Genre.prefix(), TAG_PREFIX_GENRE);
        assert_eq!(TagKind::Dna.prefix(), TAG_PREFIX_DNA);
        assert_eq!(TagKind::Spoiler.prefix(), TAG_PREFIX_SPOILER);
    }

    #[test]
    fn from_tag_classifies_each_prefix() {
        assert_eq!(TagKind::from_tag("@fantasy"), Some(TagKind::Genre));
        assert_eq!(TagKind::from_tag("#cozy"), Some(TagKind::Dna));
        assert_eq!(TagKind::from_tag("!hero-dies"), Some(TagKind::Spoiler));
        assert_eq!(TagKind::from_tag("unprefixed"), None);
        assert_eq!(TagKind::from_tag(""), None);
    }

    #[test]
    fn format_tag_attaches_prefix() {
        assert_eq!(TagKind::Dna.format_tag("cozy"), "#cozy");
        assert_eq!(TagKind::Spoiler.format_tag("hero-dies"), "!hero-dies");
        assert_eq!(TagKind::Genre.format_tag("fantasy"), "@fantasy");
    }
}
