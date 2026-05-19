//! Shared types for the `/{entity}/{id}/books` endpoints.
//!
//! The third such endpoint (`/publishers/{publisher_id}/books`)
//! triggered this extraction — per the project's "third occurrence"
//! rule, three identical row + response shapes is the signal to
//! share. The earlier two (`authors` + `narrators`) were typed
//! per-entity (`AuthorBookEntry` + `NarratorBookEntry`); both have
//! been retired in favour of [`EntityBookSummary`].
//!
//! ## What stays per-entity
//!
//! The SQL is NOT shared — each entity walks a different join:
//!
//! * `authors_books`: single-FK on `books.author_id`.
//! * `narrators_books`: junction `book_narrator`.
//! * `publishers_books`: single-FK on `books.publisher_id`.
//!
//! Sort order is also identical today (`release_date DESC NULLS LAST,
//! title COLLATE NOCASE`) but lives in each handler so future
//! entity-specific tweaks (e.g. publisher-by-imprint sub-sort) don't
//! need a shared-helper signature change.
//!
//! ## JSON shape — backwards-compatible
//!
//! The Rust types renamed from `AuthorBookEntry` / `NarratorBookEntry`
//! to `EntityBookSummary`, but `serde` produces field names only —
//! the on-wire JSON shape is unchanged.

use serde::{Deserialize, Serialize};

/// One row in [`EntityBooksResponse`]. Slim by design — enough for
/// an entity-detail page to render the book strip without
/// re-fetching `/books/{id}` per row.
#[derive(Debug, Serialize)]
pub struct EntityBookSummary {
    pub book_id: i64,
    pub title: String,
    pub release_date: Option<String>,
    pub duration_ms: Option<i64>,
    pub reading_status: String,
}

/// Response body for `GET /{entity}/{id}/books`. Pagination keys
/// match the other entity-list endpoints so clients can build "page
/// X of Y" UIs without a second call.
#[derive(Debug, Serialize)]
pub struct EntityBooksResponse {
    pub books: Vec<EntityBookSummary>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// Query-string params for `GET /{entity}/{id}/books`. Pagination
/// only — entity-specific sort tweaks (if any future endpoint wants
/// them) can extend the per-handler `Query` extractor instead.
#[derive(Debug, Deserialize, Default)]
pub struct EntityBooksQuery {
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn entity_book_summary_serializes_with_expected_keys() {
        let e = EntityBookSummary {
            book_id: 42,
            title: "The Way of Kings".into(),
            release_date: Some("2010-08-31".into()),
            duration_ms: Some(45_000_000),
            reading_status: "finished".into(),
        };
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["book_id"], 42);
        assert_eq!(json["title"], "The Way of Kings");
        assert_eq!(json["release_date"], "2010-08-31");
        assert_eq!(json["duration_ms"], 45_000_000);
        assert_eq!(json["reading_status"], "finished");
    }

    #[test]
    fn entity_book_summary_preserves_null_optional_fields() {
        let e = EntityBookSummary {
            book_id: 1,
            title: "Untitled".into(),
            release_date: None,
            duration_ms: None,
            reading_status: "want_to_read".into(),
        };
        let json = serde_json::to_value(&e).unwrap();
        assert!(json.get("release_date").is_some());
        assert!(json["release_date"].is_null());
        assert!(json["duration_ms"].is_null());
    }

    #[test]
    fn entity_books_response_serializes_with_pagination_keys() {
        let r = EntityBooksResponse {
            books: vec![],
            total: 0,
            limit: 50,
            offset: 0,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"books\""));
        assert!(json.contains("\"total\""));
        assert!(json.contains("\"limit\""));
        assert!(json.contains("\"offset\""));
    }
}
