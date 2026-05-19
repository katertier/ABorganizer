//! `GET /api/v1/publishers` list + `GET /api/v1/publishers/{publisher_id}`
//! detail. Mirrors the author / narrator pattern (cycle 32) — the
//! third entity-list endpoint following the same shape.
//!
//! Publishers carry less metadata than authors: just `name` +
//! `canonical_name` + `created_at` (no bio / image / `audible_id` /
//! aliases / `name_sort` / `updated_at` — see migration 001). The
//! endpoints surface those columns plus a `book_count` join.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, response::Response};
use serde::{Deserialize, Serialize};

use crate::ApiError;
use crate::entity_books::{EntityBookSummary, EntityBooksQuery, EntityBooksResponse};
use crate::pagination::{clamp_limit, clamp_offset};
use crate::state::ApiState;

/// Publisher detail JSON returned by the GET endpoint.
#[derive(Debug, Serialize)]
pub struct PublisherDetail {
    pub publisher_id: i64,
    pub name: String,
    /// Normalised form, e.g. "Audible Studios". May be `null` —
    /// populated by future canonical-publisher enrich work.
    pub canonical_name: Option<String>,
    /// Number of books currently joined to this publisher (active +
    /// inactive — same `books.publisher_id` semantics as the rest
    /// of the read path).
    pub book_count: i64,
    pub created_at: i64,
}

/// `GET /api/v1/publishers/{publisher_id}`
///
/// Returns `200 OK` with [`PublisherDetail`] JSON. `404 Not Found`
/// when no `publishers` row exists at that id.
///
/// # Errors
///
/// Database access failures surface as [`ApiError::Internal`].
#[allow(clippy::missing_panics_doc)] // panic-free
pub async fn publishers_get(
    State(state): State<ApiState>,
    Path(publisher_id): Path<i64>,
) -> Result<Response, ApiError> {
    let row = sqlx::query!(
        r#"SELECT p.publisher_id AS "publisher_id!: i64",
                  p.name AS "name!: String",
                  p.canonical_name AS "canonical_name?: String",
                  p.created_at AS "created_at!: i64",
                  (SELECT COUNT(*) FROM books WHERE publisher_id = p.publisher_id)
                      AS "book_count!: i64"
           FROM publishers p
           WHERE p.publisher_id = ?"#,
        publisher_id,
    )
    .fetch_optional(state.inner.library.pool())
    .await
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("publisher lookup: {e}"))))?;

    let Some(r) = row else {
        return Err(ApiError::NotFound(format!("publisher {publisher_id}")));
    };

    let detail = PublisherDetail {
        publisher_id: r.publisher_id,
        name: r.name,
        canonical_name: r.canonical_name,
        book_count: r.book_count,
        created_at: r.created_at,
    };
    Ok((StatusCode::OK, Json(detail)).into_response())
}

/// Compact row in the paginated publishers list. Same shape as
/// [`PublisherDetail`] — there's nothing heavy enough to drop
/// (no bio / aliases) so list + detail rows match exactly.
#[derive(Debug, Serialize)]
pub struct PublisherListItem {
    pub publisher_id: i64,
    pub name: String,
    pub canonical_name: Option<String>,
    pub book_count: i64,
}

/// Response body for `GET /api/v1/publishers`. `total` is the count
/// matching the filter (NOT clamped by `limit` / `offset`).
#[derive(Debug, Serialize)]
pub struct PublishersListResponse {
    pub publishers: Vec<PublisherListItem>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// Query-string params for the publishers list endpoint.
#[derive(Debug, Deserialize, Default)]
pub struct PublishersListQuery {
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
    /// Sort key. One of:
    ///
    /// * `name` — case-insensitive (default).
    /// * `book_count` — descending, then by name ascending.
    ///
    /// Unknown values produce `400 Bad Request`.
    #[serde(default)]
    pub sort: Option<String>,
    /// Case-insensitive substring filter on `name`. Empty string =
    /// no filter.
    #[serde(default)]
    pub q: Option<String>,
}

/// Resolved sort key. Publishers lack `name_sort`, so the sort
/// surface is just `name` / `book_count`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublishersSort {
    Name,
    BookCountDesc,
}

impl PublishersSort {
    fn parse(s: Option<&str>) -> Result<Self, ApiError> {
        match s {
            None | Some("" | "name") => Ok(Self::Name),
            Some("book_count") => Ok(Self::BookCountDesc),
            Some(other) => Err(ApiError::BadRequest(format!(
                "unknown sort {other:?}; expected one of name / book_count"
            ))),
        }
    }
}

/// `GET /api/v1/publishers[?limit=&offset=&sort=&q=]`
///
/// Returns `200 OK` with [`PublishersListResponse`] JSON. `400 Bad
/// Request` for an unknown `sort` value.
///
/// # Errors
///
/// Database access failures surface as [`ApiError::Internal`].
#[allow(clippy::missing_panics_doc)] // panic-free
pub async fn publishers_list(
    State(state): State<ApiState>,
    Query(params): Query<PublishersListQuery>,
) -> Result<Response, ApiError> {
    let sort = PublishersSort::parse(params.sort.as_deref())?;
    let limit = clamp_limit(params.limit);
    let offset = clamp_offset(params.offset);

    let q_filter = params
        .q
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| format!("%{s}%"));

    let total: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!: i64"
             FROM publishers
            WHERE ?1 IS NULL OR name LIKE ?1 COLLATE NOCASE"#,
        q_filter,
    )
    .fetch_one(state.inner.library.pool())
    .await
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("publishers count: {e}"))))?;

    let pool = state.inner.library.pool();
    let publishers: Vec<PublisherListItem> = match sort {
        PublishersSort::Name => sqlx::query!(
            r#"SELECT p.publisher_id AS "publisher_id!: i64",
                      p.name AS "name!: String",
                      p.canonical_name AS "canonical_name?: String",
                      (SELECT COUNT(*) FROM books WHERE publisher_id = p.publisher_id)
                          AS "book_count!: i64"
                 FROM publishers p
                WHERE ?1 IS NULL OR p.name LIKE ?1 COLLATE NOCASE
                ORDER BY p.name COLLATE NOCASE
                LIMIT ?2 OFFSET ?3"#,
            q_filter,
            limit,
            offset,
        )
        .fetch_all(pool)
        .await
        .map(|rows| {
            rows.into_iter()
                .map(|r| PublisherListItem {
                    publisher_id: r.publisher_id,
                    name: r.name,
                    canonical_name: r.canonical_name,
                    book_count: r.book_count,
                })
                .collect()
        }),
        PublishersSort::BookCountDesc => sqlx::query!(
            r#"SELECT p.publisher_id AS "publisher_id!: i64",
                      p.name AS "name!: String",
                      p.canonical_name AS "canonical_name?: String",
                      (SELECT COUNT(*) FROM books WHERE publisher_id = p.publisher_id)
                          AS "book_count!: i64"
                 FROM publishers p
                WHERE ?1 IS NULL OR p.name LIKE ?1 COLLATE NOCASE
                ORDER BY (SELECT COUNT(*) FROM books WHERE publisher_id = p.publisher_id) DESC,
                         p.name COLLATE NOCASE
                LIMIT ?2 OFFSET ?3"#,
            q_filter,
            limit,
            offset,
        )
        .fetch_all(pool)
        .await
        .map(|rows| {
            rows.into_iter()
                .map(|r| PublisherListItem {
                    publisher_id: r.publisher_id,
                    name: r.name,
                    canonical_name: r.canonical_name,
                    book_count: r.book_count,
                })
                .collect()
        }),
    }
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("publishers list: {e}"))))?;

    Ok((
        StatusCode::OK,
        Json(PublishersListResponse {
            publishers,
            total,
            limit,
            offset,
        }),
    )
        .into_response())
}

/// `GET /api/v1/publishers/{publisher_id}/books[?limit=&offset=]`
///
/// Returns `200 OK` with [`EntityBooksResponse`] JSON listing the
/// books whose `publisher_id` FK points at this publisher. `404
/// Not Found` when the publisher doesn't exist. Empty `books`
/// array when the publisher exists but has no books yet.
///
/// Single-FK predicate like `authors_books` — `books.publisher_id`
/// is a nullable single-FK column, not a junction. Multi-publisher
/// editions (rare; co-publishing deals) are not modelled.
///
/// Sort: `release_date DESC NULLS LAST`, then `books.title COLLATE
/// NOCASE`. Soft-deleted books stay in the result (operators can
/// dim them client-side).
///
/// # Errors
///
/// Database access failures surface as [`ApiError::Internal`].
#[allow(clippy::missing_panics_doc)] // panic-free
pub async fn publishers_books(
    State(state): State<ApiState>,
    Path(publisher_id): Path<i64>,
    Query(params): Query<EntityBooksQuery>,
) -> Result<Response, ApiError> {
    let pool = state.inner.library.pool();
    let limit = clamp_limit(params.limit);
    let offset = clamp_offset(params.offset);

    let publisher_exists = sqlx::query_scalar!(
        r#"SELECT 1 AS "n!: i64" FROM publishers WHERE publisher_id = ?"#,
        publisher_id,
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "publisher existence check: {e}"
        )))
    })?
    .is_some();

    if !publisher_exists {
        return Err(ApiError::NotFound(format!("publisher {publisher_id}")));
    }

    let total: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!: i64"
             FROM books
            WHERE publisher_id = ?"#,
        publisher_id,
    )
    .fetch_one(pool)
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "publisher books count: {e}"
        )))
    })?;

    let rows = sqlx::query!(
        r#"SELECT book_id          AS "book_id!: i64",
                  title            AS "title!: String",
                  release_date     AS "release_date?: String",
                  duration_ms      AS "duration_ms?: i64",
                  reading_status   AS "reading_status!: String"
             FROM books
            WHERE publisher_id = ?
            ORDER BY release_date IS NULL,
                     release_date DESC,
                     title COLLATE NOCASE
            LIMIT ? OFFSET ?"#,
        publisher_id,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("publisher books: {e}"))))?;

    let books: Vec<EntityBookSummary> = rows
        .into_iter()
        .map(|r| EntityBookSummary {
            book_id: r.book_id,
            title: r.title,
            release_date: r.release_date,
            duration_ms: r.duration_ms,
            reading_status: r.reading_status,
        })
        .collect();

    Ok((
        StatusCode::OK,
        Json(EntityBooksResponse {
            books,
            total,
            limit,
            offset,
        }),
    )
        .into_response())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn publisher_detail_serializes_with_expected_keys() {
        let d = PublisherDetail {
            publisher_id: 3,
            name: "Audible Studios".into(),
            canonical_name: Some("audible-studios".into()),
            book_count: 17,
            created_at: 1_700_000_000,
        };
        let json = serde_json::to_value(&d).unwrap();
        assert_eq!(json["publisher_id"], 3);
        assert_eq!(json["name"], "Audible Studios");
        assert_eq!(json["canonical_name"], "audible-studios");
        assert_eq!(json["book_count"], 17);
        assert_eq!(json["created_at"], 1_700_000_000);
    }

    #[test]
    fn publisher_detail_preserves_null_canonical_name() {
        let d = PublisherDetail {
            publisher_id: 1,
            name: "Unknown".into(),
            canonical_name: None,
            book_count: 0,
            created_at: 0,
        };
        let json = serde_json::to_value(&d).unwrap();
        assert!(json.get("canonical_name").is_some());
        assert!(json["canonical_name"].is_null());
    }

    #[test]
    fn sort_parses_documented_keys() {
        assert_eq!(PublishersSort::parse(None).unwrap(), PublishersSort::Name);
        assert_eq!(
            PublishersSort::parse(Some("")).unwrap(),
            PublishersSort::Name
        );
        assert_eq!(
            PublishersSort::parse(Some("name")).unwrap(),
            PublishersSort::Name
        );
        assert_eq!(
            PublishersSort::parse(Some("book_count")).unwrap(),
            PublishersSort::BookCountDesc
        );
    }

    #[test]
    fn sort_rejects_unknown_with_400() {
        match PublishersSort::parse(Some("name_sort")) {
            Err(ApiError::BadRequest(msg)) => {
                assert!(msg.contains("name_sort"));
                assert!(msg.contains("book_count"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn publishers_list_response_serializes_with_pagination_keys() {
        let resp = PublishersListResponse {
            publishers: vec![PublisherListItem {
                publisher_id: 3,
                name: "Audible Studios".into(),
                canonical_name: Some("audible-studios".into()),
                book_count: 17,
            }],
            total: 42,
            limit: 50,
            offset: 0,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["total"], 42);
        assert_eq!(json["limit"], 50);
        assert_eq!(json["offset"], 0);
        let items = json["publishers"].as_array().expect("publishers is array");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["publisher_id"], 3);
        assert_eq!(items[0]["name"], "Audible Studios");
        assert_eq!(items[0]["book_count"], 17);
    }
}
