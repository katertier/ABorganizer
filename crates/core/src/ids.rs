//! Newtype IDs. Per `docs/POLICIES.md`: every semantically distinct
//! numeric ID gets its own type, so a `BookId` can't accidentally be
//! used where a `JobId` is expected.

use serde::{Deserialize, Serialize};

/// Database primary key for a book.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BookId(pub i64);

impl std::fmt::Display for BookId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "book#{}", self.0)
    }
}

/// Database primary key for an audio file belonging to a book.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FileId(pub i64);

impl std::fmt::Display for FileId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "file#{}", self.0)
    }
}

/// Database primary key for a job (queued pipeline work item).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JobId(pub i64);

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "job#{}", self.0)
    }
}
