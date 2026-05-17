//! `GET /api/v1/narrators/{narrator_id}` — single-narrator read endpoint.
//!
//! Mirrors `authors_get` against the `narrators` table. Returns the
//! canonical narrator row + `book_count` (count of `book_narrator`
//! junction rows) so a narrator detail page can render without
//! hand-crafting joins. Aliases live in the `narrator_aliases`
//! junction (migration 013); the legacy `narrators.aliases`
//! newline-string column was dropped in migration 015.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, response::Response};
use serde::{Deserialize, Serialize};

use crate::ApiError;
use crate::pagination::{clamp_limit, clamp_offset};
use crate::state::ApiState;

/// Narrator detail JSON returned by the GET endpoint.
#[derive(Debug, Serialize)]
pub struct NarratorDetail {
    pub narrator_id: i64,
    pub name: String,
    pub name_sort: Option<String>,
    /// Bio / blurb. No enrichment stage populates this today —
    /// reserved for a future `enrich-canonical-narrator` step
    /// mirroring `enrich-canonical-author`. May contain HTML.
    pub bio: Option<String>,
    /// Canonical headshot URL. Reserved (see `bio`).
    pub image_url: Option<String>,
    /// Audible ASIN, when known. Cross-system join key for any
    /// future narrator enrichment.
    pub audible_id: Option<String>,
    /// Observed-spelling variants from the `narrator_aliases`
    /// junction table. Sorted by observation order.
    pub aliases: Vec<String>,
    /// Number of books credited to this narrator (count of
    /// `book_narrator` rows — same semantics as `book_count`
    /// on the authors endpoint).
    pub book_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// `GET /api/v1/narrators/{narrator_id}`
///
/// Returns `200 OK` with [`NarratorDetail`] JSON. `404 Not Found`
/// when no `narrators` row exists at that id.
///
/// # Errors
///
/// Database access failures surface as [`ApiError::Internal`].
#[allow(clippy::missing_panics_doc)] // panic-free
pub async fn narrators_get(
    State(state): State<ApiState>,
    Path(narrator_id): Path<i64>,
) -> Result<Response, ApiError> {
    let row = sqlx::query!(
        r#"SELECT n.narrator_id AS "narrator_id!: i64",
                  n.name AS "name!: String",
                  n.name_sort AS "name_sort?: String",
                  n.bio AS "bio?: String",
                  n.image_url AS "image_url?: String",
                  n.audible_id AS "audible_id?: String",
                  n.created_at AS "created_at!: i64",
                  n.updated_at AS "updated_at!: i64",
                  (SELECT COUNT(*) FROM book_narrator WHERE narrator_id = n.narrator_id)
                      AS "book_count!: i64"
           FROM narrators n
           WHERE n.narrator_id = ?"#,
        narrator_id,
    )
    .fetch_optional(state.inner.library.pool())
    .await
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("narrator lookup: {e}"))))?;

    let Some(r) = row else {
        return Err(ApiError::NotFound(format!("narrator {narrator_id}")));
    };

    let aliases: Vec<String> = sqlx::query_scalar!(
        r#"SELECT alias AS "alias!: String"
             FROM narrator_aliases
            WHERE narrator_id = ?
            ORDER BY alias_id"#,
        narrator_id,
    )
    .fetch_all(state.inner.library.pool())
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "narrator aliases lookup: {e}"
        )))
    })?;

    let detail = NarratorDetail {
        narrator_id: r.narrator_id,
        name: r.name,
        name_sort: r.name_sort,
        bio: r.bio,
        image_url: r.image_url,
        audible_id: r.audible_id,
        aliases,
        book_count: r.book_count,
        created_at: r.created_at,
        updated_at: r.updated_at,
    };
    Ok((StatusCode::OK, Json(detail)).into_response())
}

/// Compact row in the paginated narrators list.
///
/// Trimmed shape vs. [`NarratorDetail`] — drops `bio` (too long
/// for a list), `aliases` (junction query per row would be O(n)
/// round-trips), and timestamps (operators browsing the list
/// don't typically care).
#[derive(Debug, Serialize)]
pub struct NarratorListItem {
    pub narrator_id: i64,
    pub name: String,
    pub name_sort: Option<String>,
    pub audible_id: Option<String>,
    pub image_url: Option<String>,
    pub book_count: i64,
}

/// Response body for `GET /api/v1/narrators`. `total` is the count
/// matching the filter (NOT clamped by `limit` / `offset`), so
/// clients can build "page 1 of N" UIs without a second call.
#[derive(Debug, Serialize)]
pub struct NarratorsListResponse {
    pub narrators: Vec<NarratorListItem>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// Query-string params for the narrators list endpoint.
#[derive(Debug, Deserialize, Default)]
pub struct NarratorsListQuery {
    /// Page size. Defaults to [`crate::pagination::DEFAULT_LIMIT`]
    /// (50); clamped to [`crate::pagination::MAX_LIMIT`] (200)
    /// silently. Negative / zero values clamp to 1.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Offset into the result set. Defaults to 0; negatives
    /// clamp to 0.
    #[serde(default)]
    pub offset: Option<i64>,
    /// Sort key. One of:
    ///
    /// * `name` — by display name, case-insensitive (default).
    /// * `name_sort` — by `COALESCE(name_sort, name)`.
    /// * `book_count` — descending by `book_count` (count of
    ///   `book_narrator` rows), then by name ascending for
    ///   stable tie-break. Surfaces the most prolific narrators.
    ///
    /// Unknown values produce `400 Bad Request` so typos surface
    /// at the API boundary instead of silently picking the default.
    #[serde(default)]
    pub sort: Option<String>,
    /// Optional case-insensitive substring filter on `name`.
    /// Empty string is treated as "no filter".
    #[serde(default)]
    pub q: Option<String>,
}

/// Resolved sort key. Internal — surface uses the string form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NarratorsSort {
    Name,
    NameSort,
    BookCountDesc,
}

impl NarratorsSort {
    fn parse(s: Option<&str>) -> Result<Self, ApiError> {
        match s {
            None | Some("" | "name") => Ok(Self::Name),
            Some("name_sort") => Ok(Self::NameSort),
            Some("book_count") => Ok(Self::BookCountDesc),
            Some(other) => Err(ApiError::BadRequest(format!(
                "unknown sort {other:?}; expected one of name / name_sort / book_count"
            ))),
        }
    }
}

/// `GET /api/v1/narrators[?limit=&offset=&sort=&q=]`
///
/// Returns `200 OK` with [`NarratorsListResponse`] JSON. `400 Bad
/// Request` for an unknown `sort` value.
///
/// # Errors
///
/// Database access failures surface as [`ApiError::Internal`].
#[allow(clippy::missing_panics_doc, clippy::too_many_lines)] // panic-free; macro arms inflate line count
pub async fn narrators_list(
    State(state): State<ApiState>,
    Query(params): Query<NarratorsListQuery>,
) -> Result<Response, ApiError> {
    let sort = NarratorsSort::parse(params.sort.as_deref())?;
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
             FROM narrators
            WHERE ?1 IS NULL OR name LIKE ?1 COLLATE NOCASE"#,
        q_filter,
    )
    .fetch_one(state.inner.library.pool())
    .await
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("narrators count: {e}"))))?;

    let pool = state.inner.library.pool();
    let narrators: Vec<NarratorListItem> = match sort {
        NarratorsSort::Name => sqlx::query!(
            r#"SELECT n.narrator_id AS "narrator_id!: i64",
                      n.name AS "name!: String",
                      n.name_sort AS "name_sort?: String",
                      n.audible_id AS "audible_id?: String",
                      n.image_url AS "image_url?: String",
                      (SELECT COUNT(*) FROM book_narrator WHERE narrator_id = n.narrator_id)
                          AS "book_count!: i64"
                 FROM narrators n
                WHERE ?1 IS NULL OR n.name LIKE ?1 COLLATE NOCASE
                ORDER BY n.name COLLATE NOCASE
                LIMIT ?2 OFFSET ?3"#,
            q_filter,
            limit,
            offset,
        )
        .fetch_all(pool)
        .await
        .map(|rows| {
            rows.into_iter()
                .map(|r| NarratorListItem {
                    narrator_id: r.narrator_id,
                    name: r.name,
                    name_sort: r.name_sort,
                    audible_id: r.audible_id,
                    image_url: r.image_url,
                    book_count: r.book_count,
                })
                .collect()
        }),
        NarratorsSort::NameSort => sqlx::query!(
            r#"SELECT n.narrator_id AS "narrator_id!: i64",
                      n.name AS "name!: String",
                      n.name_sort AS "name_sort?: String",
                      n.audible_id AS "audible_id?: String",
                      n.image_url AS "image_url?: String",
                      (SELECT COUNT(*) FROM book_narrator WHERE narrator_id = n.narrator_id)
                          AS "book_count!: i64"
                 FROM narrators n
                WHERE ?1 IS NULL OR n.name LIKE ?1 COLLATE NOCASE
                ORDER BY COALESCE(n.name_sort, n.name) COLLATE NOCASE
                LIMIT ?2 OFFSET ?3"#,
            q_filter,
            limit,
            offset,
        )
        .fetch_all(pool)
        .await
        .map(|rows| {
            rows.into_iter()
                .map(|r| NarratorListItem {
                    narrator_id: r.narrator_id,
                    name: r.name,
                    name_sort: r.name_sort,
                    audible_id: r.audible_id,
                    image_url: r.image_url,
                    book_count: r.book_count,
                })
                .collect()
        }),
        NarratorsSort::BookCountDesc => sqlx::query!(
            r#"SELECT n.narrator_id AS "narrator_id!: i64",
                      n.name AS "name!: String",
                      n.name_sort AS "name_sort?: String",
                      n.audible_id AS "audible_id?: String",
                      n.image_url AS "image_url?: String",
                      (SELECT COUNT(*) FROM book_narrator WHERE narrator_id = n.narrator_id)
                          AS "book_count!: i64"
                 FROM narrators n
                WHERE ?1 IS NULL OR n.name LIKE ?1 COLLATE NOCASE
                ORDER BY (SELECT COUNT(*) FROM book_narrator WHERE narrator_id = n.narrator_id) DESC,
                         n.name COLLATE NOCASE
                LIMIT ?2 OFFSET ?3"#,
            q_filter,
            limit,
            offset,
        )
        .fetch_all(pool)
        .await
        .map(|rows| {
            rows.into_iter()
                .map(|r| NarratorListItem {
                    narrator_id: r.narrator_id,
                    name: r.name,
                    name_sort: r.name_sort,
                    audible_id: r.audible_id,
                    image_url: r.image_url,
                    book_count: r.book_count,
                })
                .collect()
        }),
    }
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("narrators list: {e}"))))?;

    Ok((
        StatusCode::OK,
        Json(NarratorsListResponse {
            narrators,
            total,
            limit,
            offset,
        }),
    )
        .into_response())
}

/// One row in [`NarratorBooksResponse`].
///
/// Slim by design — same trade-off as [`NarratorListItem`]: enough
/// for a narrator-detail page to render the book strip without
/// re-fetching `/books/{id}` for each row.
#[derive(Debug, Serialize)]
pub struct NarratorBookEntry {
    pub book_id: i64,
    pub title: String,
    pub release_date: Option<String>,
    pub duration_ms: Option<i64>,
    pub reading_status: String,
}

/// Response body for `GET /api/v1/narrators/{narrator_id}/books`.
#[derive(Debug, Serialize)]
pub struct NarratorBooksResponse {
    pub books: Vec<NarratorBookEntry>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// Query-string params for [`narrators_books`].
#[derive(Debug, Deserialize, Default)]
pub struct NarratorBooksQuery {
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

/// `GET /api/v1/narrators/{narrator_id}/books[?limit=&offset=]`
///
/// Returns `200 OK` with [`NarratorBooksResponse`] JSON listing
/// the books linked via the `book_narrator` junction (multi-narrator
/// model — a full-cast recording lands a row per (book, narrator)
/// pair, so a 3-cast book is visible in 3 narrator detail pages).
/// `404 Not Found` when the narrator doesn't exist. Empty `books`
/// array when the narrator exists but has no recordings joined.
///
/// Unlike `/authors/{author_id}/books` (single-FK predicate), this
/// endpoint JOINs through `book_narrator`. Each book appears at
/// most once per narrator, but a multi-narrator book ALSO appears
/// in every other co-narrator's bucket — same convention as
/// `Dimension::Narrator` in `ab-stats`.
///
/// Ordering: `release_date DESC` (NULLs last), then `books.title`
/// ASC. Soft-deleted books (`books.deleted_at IS NOT NULL`) stay
/// in the result — operators can dim them client-side.
///
/// # Errors
///
/// Database access failures surface as [`ApiError::Internal`].
#[allow(clippy::missing_panics_doc)] // panic-free
pub async fn narrators_books(
    State(state): State<ApiState>,
    Path(narrator_id): Path<i64>,
    Query(params): Query<NarratorBooksQuery>,
) -> Result<Response, ApiError> {
    let pool = state.inner.library.pool();
    let limit = clamp_limit(params.limit);
    let offset = clamp_offset(params.offset);

    let narrator_exists = sqlx::query_scalar!(
        r#"SELECT 1 AS "n!: i64" FROM narrators WHERE narrator_id = ?"#,
        narrator_id,
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "narrator existence check: {e}"
        )))
    })?
    .is_some();

    if !narrator_exists {
        return Err(ApiError::NotFound(format!("narrator {narrator_id}")));
    }

    let total: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!: i64"
             FROM book_narrator bn
             JOIN books b ON b.book_id = bn.book_id
            WHERE bn.narrator_id = ?"#,
        narrator_id,
    )
    .fetch_one(pool)
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "narrator books count: {e}"
        )))
    })?;

    let rows = sqlx::query!(
        r#"SELECT b.book_id          AS "book_id!: i64",
                  b.title            AS "title!: String",
                  b.release_date     AS "release_date?: String",
                  b.duration_ms      AS "duration_ms?: i64",
                  b.reading_status   AS "reading_status!: String"
             FROM book_narrator bn
             JOIN books b ON b.book_id = bn.book_id
            WHERE bn.narrator_id = ?
            ORDER BY b.release_date IS NULL,
                     b.release_date DESC,
                     b.title COLLATE NOCASE
            LIMIT ? OFFSET ?"#,
        narrator_id,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("narrator books: {e}"))))?;

    let books: Vec<NarratorBookEntry> = rows
        .into_iter()
        .map(|r| NarratorBookEntry {
            book_id: r.book_id,
            title: r.title,
            release_date: r.release_date,
            duration_ms: r.duration_ms,
            reading_status: r.reading_status,
        })
        .collect();

    Ok((
        StatusCode::OK,
        Json(NarratorBooksResponse {
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
    fn narrator_detail_serializes_with_expected_keys() {
        let d = NarratorDetail {
            narrator_id: 11,
            name: "Michael Kramer".into(),
            name_sort: Some("Kramer, Michael".into()),
            bio: Some("Veteran audiobook narrator...".into()),
            image_url: Some("https://example.invalid/k.jpg".into()),
            audible_id: Some("B002XYZ123".into()),
            aliases: vec!["Michael Kramer".into(), "M. Kramer".into()],
            book_count: 87,
            created_at: 1_700_000_000,
            updated_at: 1_770_000_000,
        };
        let json = serde_json::to_value(&d).unwrap();
        assert_eq!(json["narrator_id"], 11);
        assert_eq!(json["name"], "Michael Kramer");
        assert_eq!(json["bio"], "Veteran audiobook narrator...");
        assert_eq!(json["image_url"], "https://example.invalid/k.jpg");
        assert_eq!(json["audible_id"], "B002XYZ123");
        assert_eq!(json["book_count"], 87);
        let aliases = json["aliases"].as_array().expect("aliases is array");
        assert_eq!(aliases.len(), 2);
        assert_eq!(aliases[0], "Michael Kramer");
        assert_eq!(aliases[1], "M. Kramer");
    }

    #[test]
    fn narrator_detail_preserves_nulls() {
        let d = NarratorDetail {
            narrator_id: 1,
            name: "Anonymous".into(),
            name_sort: None,
            bio: None,
            image_url: None,
            audible_id: None,
            aliases: Vec::new(),
            book_count: 0,
            created_at: 0,
            updated_at: 0,
        };
        let json = serde_json::to_value(&d).unwrap();
        assert!(json.get("bio").is_some(), "bio key present even when null");
        assert!(json["bio"].is_null());
        assert!(json["image_url"].is_null());
        assert!(json["audible_id"].is_null());
        let aliases = json["aliases"].as_array().expect("aliases is array");
        assert!(aliases.is_empty());
    }

    #[test]
    fn sort_parses_documented_keys() {
        assert_eq!(NarratorsSort::parse(None).unwrap(), NarratorsSort::Name);
        assert_eq!(
            NarratorsSort::parse(Some("")).unwrap(),
            NarratorsSort::Name,
            "empty string treated as default"
        );
        assert_eq!(
            NarratorsSort::parse(Some("name")).unwrap(),
            NarratorsSort::Name
        );
        assert_eq!(
            NarratorsSort::parse(Some("name_sort")).unwrap(),
            NarratorsSort::NameSort
        );
        assert_eq!(
            NarratorsSort::parse(Some("book_count")).unwrap(),
            NarratorsSort::BookCountDesc
        );
    }

    #[test]
    fn sort_rejects_unknown_with_400() {
        match NarratorsSort::parse(Some("created_at")) {
            Err(ApiError::BadRequest(msg)) => {
                assert!(msg.contains("created_at"));
                assert!(msg.contains("book_count"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn narrators_list_response_serializes_with_pagination_keys() {
        let resp = NarratorsListResponse {
            narrators: vec![NarratorListItem {
                narrator_id: 11,
                name: "Michael Kramer".into(),
                name_sort: Some("Kramer, Michael".into()),
                audible_id: Some("B002XYZ123".into()),
                image_url: Some("https://example.invalid/k.jpg".into()),
                book_count: 87,
            }],
            total: 213,
            limit: 50,
            offset: 0,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["total"], 213);
        assert_eq!(json["limit"], 50);
        assert_eq!(json["offset"], 0);
        let items = json["narrators"].as_array().expect("narrators is array");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["narrator_id"], 11);
        assert_eq!(items[0]["name"], "Michael Kramer");
        assert_eq!(items[0]["book_count"], 87);
        // Compact shape — these heavier fields are NOT in the list row.
        assert!(items[0].get("bio").is_none(), "list row omits bio");
        assert!(items[0].get("aliases").is_none(), "list row omits aliases");
    }

    #[test]
    fn narrator_book_entry_serializes_with_expected_keys() {
        let e = NarratorBookEntry {
            book_id: 7,
            title: "Mistborn: The Final Empire".into(),
            release_date: Some("2006-07-17".into()),
            duration_ms: Some(89_400_000),
            reading_status: "reading".into(),
        };
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["book_id"], 7);
        assert_eq!(json["title"], "Mistborn: The Final Empire");
        assert_eq!(json["release_date"], "2006-07-17");
        assert_eq!(json["duration_ms"], 89_400_000);
        assert_eq!(json["reading_status"], "reading");
    }

    #[test]
    fn narrator_book_entry_preserves_null_optional_fields() {
        let e = NarratorBookEntry {
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
    fn narrator_books_response_serializes_with_pagination_keys() {
        let r = NarratorBooksResponse {
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
