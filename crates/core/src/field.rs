//! Typed identifier for `book_field_provenance.field`.
//!
//! Every extractor (tag-read, audnexus, audible-search,
//! transcript title/author/publisher heuristics) writes one or
//! more rows into `book_field_provenance` with a `field` column
//! naming what value the row claims. The consensus stage reads
//! the same column to pick winners and promote them into
//! `books.<column>`.
//!
//! Keeping that vocabulary in one place — and going through a
//! typed enum instead of inline `"title"` / `"author"` literals —
//! catches "extractor wrote, consensus never promoted" bugs at
//! compile time: an extractor that targets a field consensus
//! doesn't know about, or vice versa, is a renamed-variant
//! mismatch the compiler surfaces immediately.
//!
//! Add a new field: add a variant here, add the `as_str()` arm,
//! update the consensus crate's `PROMOTABLE_FIELDS` (if it's a
//! scalar promotion) or the relevant junction-table writer
//! (genre, author, narrator).

use serde::{Deserialize, Serialize};

/// A value of `book_field_provenance.field`.
///
/// The closed enum + `Display` + `AsRef<str>` impls produce the
/// exact string written into the DB column — bind these
/// directly into `sqlx::query!` params.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    /// Book title. Promoted to `books.title`.
    Title,
    /// Book subtitle. Promoted to `books.subtitle`.
    Subtitle,
    /// Book description / synopsis. Promoted to `books.description`.
    Description,
    /// BCP-47 primary subtag (e.g. `en`, `de`, `zh-Hans`).
    /// Promoted to `books.language`.
    Language,
    /// ISO-8601 release date string. Promoted to `books.release_date`.
    ReleaseDate,
    /// Duration in seconds. Promoted to `books.duration_ms`
    /// (× 1000) by the consensus stage's `promote_duration` path.
    DurationSeconds,
    /// Audible ASIN. Promoted to `books.asin`.
    Asin,
    /// ISBN-10 or ISBN-13. Promoted to `books.isbn`.
    Isbn,
    /// Author name. Multi-value; resolved into `authors` +
    /// `book_narrator`-style junction by `identity-resolve`.
    Author,
    /// Narrator name. Multi-value; same identity-resolve path
    /// as author.
    Narrator,
    /// Publisher / imprint name.
    Publisher,
    /// Book series name (multi-value; one row per series the
    /// book belongs to). Currently written by tag-read from the
    /// audio file's album tag. No consensus / identity-resolve
    /// path yet — adding one is a future slice; until then the
    /// provenance row is captured but does not promote anywhere.
    Series,
    /// Genre slug (canonicalised by `genre_code::normalize`).
    /// Multi-value; promoted to the `book_genre` junction.
    Genre,
    /// Cover image URL — promoted to `books.cover_url`.
    CoverUrl,
    /// Boolean flag (truthy string). Promoted to `books.abridged`.
    Abridged,
    /// Boolean flag. Promoted to `books.explicit`.
    Explicit,
}

impl Field {
    /// The exact string written to `book_field_provenance.field`.
    /// Bind this into `sqlx::query!` and use it for any read-side
    /// comparison.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Subtitle => "subtitle",
            Self::Description => "description",
            Self::Language => "language",
            Self::ReleaseDate => "release_date",
            Self::DurationSeconds => "duration_seconds",
            Self::Asin => "asin",
            Self::Isbn => "isbn",
            Self::Author => "author",
            Self::Narrator => "narrator",
            Self::Publisher => "publisher",
            Self::Series => "series",
            Self::Genre => "genre",
            Self::CoverUrl => "cover_url",
            Self::Abridged => "abridged",
            Self::Explicit => "explicit",
        }
    }

    /// The `books.<col>` column this field promotes into, if any.
    ///
    /// Returns:
    /// - `Some("title")`, `Some("subtitle")`, … for fields whose
    ///   winner value lands in a scalar column on `books`.
    /// - `Some("duration_ms")` for `DurationSeconds` — the value
    ///   transform (seconds × 1000) is the consensus stage's
    ///   responsibility; this method just names the target column.
    /// - `None` for fields that go through junction tables
    ///   (`Author`, `Narrator`, `Genre`, `Series`) or FK-indirect
    ///   identity tables (`Publisher` → `books.publisher_id`).
    ///   These need their own promotion path (identity-resolve,
    ///   consensus's genre promoter, future series-resolve).
    ///
    /// Consensus's `PROMOTABLE_FIELDS` table iterates the
    /// `Some(_)`-returning variants whose value transform is a
    /// direct text copy. `DurationSeconds` is `Some` here but
    /// handled by its own dedicated path because of the × 1000
    /// integer conversion.
    #[must_use]
    pub const fn books_column(self) -> Option<&'static str> {
        match self {
            Self::Title => Some("title"),
            Self::Subtitle => Some("subtitle"),
            Self::Description => Some("description"),
            Self::Language => Some("language"),
            Self::ReleaseDate => Some("release_date"),
            Self::DurationSeconds => Some("duration_ms"),
            Self::Asin => Some("asin"),
            Self::Isbn => Some("isbn"),
            Self::CoverUrl => Some("cover_url"),
            Self::Abridged => Some("abridged"),
            Self::Explicit => Some("explicit"),
            // Junction / identity-resolve fields:
            Self::Author | Self::Narrator | Self::Publisher | Self::Genre | Self::Series => None,
        }
    }

    /// Parse a stored `field` string back into the typed enum.
    /// Returns `None` on unknown strings — callers can treat
    /// those as legacy / extension fields not part of the closed
    /// set.
    ///
    /// Named `parse` (not `from_str`) to avoid the `FromStr`
    /// trait collision lint, same as `CacheKey::parse`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "title" => Some(Self::Title),
            "subtitle" => Some(Self::Subtitle),
            "description" => Some(Self::Description),
            "language" => Some(Self::Language),
            "release_date" => Some(Self::ReleaseDate),
            "duration_seconds" => Some(Self::DurationSeconds),
            "asin" => Some(Self::Asin),
            "isbn" => Some(Self::Isbn),
            "author" => Some(Self::Author),
            "narrator" => Some(Self::Narrator),
            "publisher" => Some(Self::Publisher),
            "series" => Some(Self::Series),
            "genre" => Some(Self::Genre),
            "cover_url" => Some(Self::CoverUrl),
            "abridged" => Some(Self::Abridged),
            "explicit" => Some(Self::Explicit),
            _ => None,
        }
    }
}

impl std::fmt::Display for Field {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for Field {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_every_variant() {
        for f in [
            Field::Title,
            Field::Subtitle,
            Field::Description,
            Field::Language,
            Field::ReleaseDate,
            Field::DurationSeconds,
            Field::Asin,
            Field::Isbn,
            Field::Author,
            Field::Narrator,
            Field::Publisher,
            Field::Series,
            Field::Genre,
            Field::CoverUrl,
            Field::Abridged,
            Field::Explicit,
        ] {
            assert_eq!(Field::parse(f.as_str()), Some(f), "round-trip {f}");
        }
    }

    #[test]
    fn unknown_strings_return_none() {
        assert_eq!(Field::parse(""), None);
        assert_eq!(Field::parse("TITLE"), None); // case-sensitive
        assert_eq!(Field::parse("authors"), None); // common typo
    }

    #[test]
    fn display_matches_as_str() {
        assert_eq!(format!("{}", Field::Asin), "asin");
    }

    #[test]
    fn books_column_for_scalar_fields() {
        assert_eq!(Field::Title.books_column(), Some("title"));
        assert_eq!(Field::Description.books_column(), Some("description"));
        assert_eq!(Field::ReleaseDate.books_column(), Some("release_date"));
        // DurationSeconds maps to `duration_ms` (the × 1000
        // transform is consensus's job, not this method's):
        assert_eq!(Field::DurationSeconds.books_column(), Some("duration_ms"));
        assert_eq!(Field::CoverUrl.books_column(), Some("cover_url"));
    }

    #[test]
    fn books_column_none_for_junction_fields() {
        // Junction / identity-resolve fields don't promote into a
        // scalar `books` column:
        assert_eq!(Field::Author.books_column(), None);
        assert_eq!(Field::Narrator.books_column(), None);
        assert_eq!(Field::Publisher.books_column(), None);
        assert_eq!(Field::Genre.books_column(), None);
        assert_eq!(Field::Series.books_column(), None);
    }
}
