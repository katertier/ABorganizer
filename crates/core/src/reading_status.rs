//! Reading status — closed enum mirroring the `books.reading_status`
//! CHECK constraint (ADR-0033).
//!
//! Variants serialize as `snake_case` to match the DB string values.

use serde::{Deserialize, Serialize};

/// Per-book curation status. Each newly scanned book starts as
/// [`ReadingStatus::WantToRead`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadingStatus {
    /// Book is in the library but the operator hasn't started it.
    WantToRead,
    /// Currently being listened to.
    Reading,
    /// Listened-to-the-end.
    Finished,
    /// Did-not-finish. Operator gave up; not a candidate for
    /// "continue listening" surfaces.
    Dnf,
}

impl ReadingStatus {
    /// The string written to `books.reading_status` (matches the
    /// `CHECK (reading_status IN (...))` constraint).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WantToRead => "want_to_read",
            Self::Reading => "reading",
            Self::Finished => "finished",
            Self::Dnf => "dnf",
        }
    }
}

/// Error returned when a string doesn't match any [`ReadingStatus`] variant.
#[derive(Debug, thiserror::Error)]
#[error("invalid reading status: {0:?}")]
pub struct ParseReadingStatusError(pub String);

impl std::str::FromStr for ReadingStatus {
    type Err = ParseReadingStatusError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "want_to_read" => Self::WantToRead,
            "reading" => Self::Reading,
            "finished" => Self::Finished,
            "dnf" => Self::Dnf,
            other => return Err(ParseReadingStatusError(other.to_owned())),
        })
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_all_variants() {
        for v in [
            ReadingStatus::WantToRead,
            ReadingStatus::Reading,
            ReadingStatus::Finished,
            ReadingStatus::Dnf,
        ] {
            assert_eq!(
                v.as_str().parse::<ReadingStatus>().expect("test fixture"),
                v
            );
        }
    }

    #[test]
    fn rejects_unknown() {
        assert!("bogus".parse::<ReadingStatus>().is_err());
    }

    #[test]
    fn serde_uses_snake_case() {
        let json = serde_json::to_string(&ReadingStatus::WantToRead).expect("test fixture");
        assert_eq!(json, "\"want_to_read\"");
        let back: ReadingStatus = serde_json::from_str("\"finished\"").expect("test fixture");
        assert_eq!(back, ReadingStatus::Finished);
    }
}
