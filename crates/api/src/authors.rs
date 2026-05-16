//! `GET /api/v1/authors/{author_id}` — single-author read endpoint.
//!
//! Returns the canonical author row + `book_count` (count of
//! `books.author_id = ?`). Surfaces the data the
//! `enrich-canonical-author` pipeline stage populates (`bio` +
//! `image_url` + `audible_id` + aliases) so frontends can render
//! an author detail page without hand-crafting joins.
//!
//! ## Use cases
//!
//! * **Author detail page** — primary consumer.
//! * **Verify enrichment landed** — operator can curl this after a
//!   library scan to confirm Audnexus filled in bio + image.
//! * **Identity-resolve debugging** — when two authors collapsed
//!   under one row, this endpoint shows the surviving canonical
//!   data + the alias list.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, response::Response};
use serde::{Deserialize, Serialize};

use crate::ApiError;
use crate::state::ApiState;

/// Default `limit` when callers omit it. Picked to keep the
/// most common "browse the authors catalogue" call cheap while
/// still being one screen of results on a typical UI.
const DEFAULT_LIMIT: i64 = 50;
/// Hard ceiling on `limit`. Larger requests get clamped (no 400)
/// — same posture as the `books_list` endpoint.
const MAX_LIMIT: i64 = 200;

/// Author detail JSON returned by the GET endpoint.
#[derive(Debug, Serialize)]
pub struct AuthorDetail {
    pub author_id: i64,
    pub name: String,
    pub name_sort: Option<String>,
    /// Bio / blurb. Populated by the `enrich-canonical-author`
    /// stage from Audnexus's `/authors/{ASIN}.description`. May
    /// contain HTML markup (links, italics); frontends sanitise
    /// on render.
    pub bio: Option<String>,
    /// Canonical headshot URL — Audnexus's `image` field.
    pub image_url: Option<String>,
    /// Audible ASIN, when known. The cross-system join key for
    /// canonical-author-enrich + future identity work.
    pub audible_id: Option<String>,
    /// Observed-spelling variants from the `author_aliases`
    /// junction table — populated by identity-resolve, audnexus
    /// enrich, and tag-read. Sorted by observation order.
    pub aliases: Vec<String>,
    /// Number of books currently joined to this author (active +
    /// inactive — same `books.author_id` semantics as the rest
    /// of the read path).
    pub book_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// `GET /api/v1/authors/{author_id}`
///
/// Returns `200 OK` with [`AuthorDetail`] JSON. `404 Not Found`
/// when no `authors` row exists at that id.
///
/// # Errors
///
/// Database access failures surface as [`ApiError::Internal`].
#[allow(clippy::missing_panics_doc)] // panic-free
pub async fn authors_get(
    State(state): State<ApiState>,
    Path(author_id): Path<i64>,
) -> Result<Response, ApiError> {
    let row = sqlx::query!(
        r#"SELECT a.author_id AS "author_id!: i64",
                  a.name AS "name!: String",
                  a.name_sort AS "name_sort?: String",
                  a.bio AS "bio?: String",
                  a.image_url AS "image_url?: String",
                  a.audible_id AS "audible_id?: String",
                  a.created_at AS "created_at!: i64",
                  a.updated_at AS "updated_at!: i64",
                  (SELECT COUNT(*) FROM books WHERE author_id = a.author_id)
                      AS "book_count!: i64"
           FROM authors a
           WHERE a.author_id = ?"#,
        author_id,
    )
    .fetch_optional(state.inner.library.pool())
    .await
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("author lookup: {e}"))))?;

    let Some(r) = row else {
        return Err(ApiError::NotFound(format!("author {author_id}")));
    };

    let aliases: Vec<String> = sqlx::query_scalar!(
        r#"SELECT alias AS "alias!: String"
             FROM author_aliases
            WHERE author_id = ?
            ORDER BY alias_id"#,
        author_id,
    )
    .fetch_all(state.inner.library.pool())
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "author aliases lookup: {e}"
        )))
    })?;

    let detail = AuthorDetail {
        author_id: r.author_id,
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

/// Compact row in the paginated authors list.
///
/// Trimmed shape vs. [`AuthorDetail`] — drops `bio` (too long for
/// a list), `aliases` (junction query per row would be O(n)
/// round-trips), and timestamps (operators browsing the list
/// don't typically care).
#[derive(Debug, Serialize)]
pub struct AuthorListItem {
    pub author_id: i64,
    pub name: String,
    pub name_sort: Option<String>,
    pub audible_id: Option<String>,
    pub image_url: Option<String>,
    pub book_count: i64,
}

/// Response body for `GET /api/v1/authors`. `total` is the count
/// matching the filter (NOT clamped by `limit` / `offset`), so
/// clients can build "page 1 of N" UIs without a second call.
#[derive(Debug, Serialize)]
pub struct AuthorsListResponse {
    pub authors: Vec<AuthorListItem>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// Query-string params for the authors list endpoint.
#[derive(Debug, Deserialize, Default)]
pub struct AuthorsListQuery {
    /// Page size. Defaults to [`DEFAULT_LIMIT`] (50); clamped to
    /// [`MAX_LIMIT`] (200) silently — same posture as `books_list`.
    /// Negative / zero values are clamped to 1.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Offset into the result set. Defaults to 0; negatives
    /// clamp to 0.
    #[serde(default)]
    pub offset: Option<i64>,
    /// Sort key. One of:
    ///
    /// * `name` — by display name, case-insensitive (default).
    /// * `name_sort` — by `COALESCE(name_sort, name)`, useful for
    ///   "Sanderson, Brandon" style ordering.
    /// * `book_count` — descending by `book_count`, then by name
    ///   ascending. Surfaces the prolific authors first.
    ///
    /// Unknown values produce `400 Bad Request` so typos surface
    /// at the API boundary instead of silently picking the default.
    #[serde(default)]
    pub sort: Option<String>,
    /// Optional case-insensitive substring filter on `name`. Empty
    /// string is treated as "no filter" (same as omitted).
    #[serde(default)]
    pub q: Option<String>,
}

/// Resolved sort key for the authors list endpoint. Internal —
/// the API surface uses the string form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthorsSort {
    Name,
    NameSort,
    BookCountDesc,
}

impl AuthorsSort {
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

/// Clamp `limit` to the `[1, MAX_LIMIT]` range with [`DEFAULT_LIMIT`]
/// as the fallback for absent / non-positive values.
const fn clamp_limit(raw: Option<i64>) -> i64 {
    match raw {
        None => DEFAULT_LIMIT,
        Some(n) if n <= 0 => 1,
        Some(n) if n > MAX_LIMIT => MAX_LIMIT,
        Some(n) => n,
    }
}

/// Clamp `offset` to non-negative. Absent → 0.
const fn clamp_offset(raw: Option<i64>) -> i64 {
    match raw {
        None => 0,
        Some(n) if n < 0 => 0,
        Some(n) => n,
    }
}

/// `GET /api/v1/authors[?limit=&offset=&sort=&q=]`
///
/// Returns `200 OK` with [`AuthorsListResponse`] JSON. `400 Bad
/// Request` for an unknown `sort` value.
///
/// # Errors
///
/// Database access failures surface as [`ApiError::Internal`].
#[allow(clippy::missing_panics_doc, clippy::too_many_lines)] // panic-free; macro arms inflate line count, see comment in body
pub async fn authors_list(
    State(state): State<ApiState>,
    Query(params): Query<AuthorsListQuery>,
) -> Result<Response, ApiError> {
    let sort = AuthorsSort::parse(params.sort.as_deref())?;
    let limit = clamp_limit(params.limit);
    let offset = clamp_offset(params.offset);

    // Empty-string `q` is treated as "no filter" — the SQL LIKE
    // pattern below uses NULL for the unfiltered case and binds
    // a wrapped pattern otherwise.
    let q_filter = params
        .q
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| format!("%{s}%"));

    // Total count first — independent of LIMIT / OFFSET so the
    // caller can build pagination UI without a second round-trip.
    let total: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!: i64"
             FROM authors
            WHERE ?1 IS NULL OR name LIKE ?1 COLLATE NOCASE"#,
        q_filter,
    )
    .fetch_one(state.inner.library.pool())
    .await
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("authors count: {e}"))))?;

    // The three ORDER BY clauses are static literals so the
    // `sqlx::query!` macros stay compile-checked. Each arm
    // returns its own anonymous row type, so we map to
    // `AuthorListItem` inside the arm before merging — that's
    // why the inner expression is `Result<Vec<AuthorListItem>>`.
    // The shared SELECT/FROM/WHERE prefix is duplicated rather
    // than abstracted because the macro requires a string literal.
    let pool = state.inner.library.pool();
    let authors: Vec<AuthorListItem> = match sort {
        AuthorsSort::Name => sqlx::query!(
            r#"SELECT a.author_id AS "author_id!: i64",
                      a.name AS "name!: String",
                      a.name_sort AS "name_sort?: String",
                      a.audible_id AS "audible_id?: String",
                      a.image_url AS "image_url?: String",
                      (SELECT COUNT(*) FROM books WHERE author_id = a.author_id)
                          AS "book_count!: i64"
                 FROM authors a
                WHERE ?1 IS NULL OR a.name LIKE ?1 COLLATE NOCASE
                ORDER BY a.name COLLATE NOCASE
                LIMIT ?2 OFFSET ?3"#,
            q_filter,
            limit,
            offset,
        )
        .fetch_all(pool)
        .await
        .map(|rows| {
            rows.into_iter()
                .map(|r| AuthorListItem {
                    author_id: r.author_id,
                    name: r.name,
                    name_sort: r.name_sort,
                    audible_id: r.audible_id,
                    image_url: r.image_url,
                    book_count: r.book_count,
                })
                .collect()
        }),
        AuthorsSort::NameSort => sqlx::query!(
            r#"SELECT a.author_id AS "author_id!: i64",
                      a.name AS "name!: String",
                      a.name_sort AS "name_sort?: String",
                      a.audible_id AS "audible_id?: String",
                      a.image_url AS "image_url?: String",
                      (SELECT COUNT(*) FROM books WHERE author_id = a.author_id)
                          AS "book_count!: i64"
                 FROM authors a
                WHERE ?1 IS NULL OR a.name LIKE ?1 COLLATE NOCASE
                ORDER BY COALESCE(a.name_sort, a.name) COLLATE NOCASE
                LIMIT ?2 OFFSET ?3"#,
            q_filter,
            limit,
            offset,
        )
        .fetch_all(pool)
        .await
        .map(|rows| {
            rows.into_iter()
                .map(|r| AuthorListItem {
                    author_id: r.author_id,
                    name: r.name,
                    name_sort: r.name_sort,
                    audible_id: r.audible_id,
                    image_url: r.image_url,
                    book_count: r.book_count,
                })
                .collect()
        }),
        AuthorsSort::BookCountDesc => sqlx::query!(
            r#"SELECT a.author_id AS "author_id!: i64",
                      a.name AS "name!: String",
                      a.name_sort AS "name_sort?: String",
                      a.audible_id AS "audible_id?: String",
                      a.image_url AS "image_url?: String",
                      (SELECT COUNT(*) FROM books WHERE author_id = a.author_id)
                          AS "book_count!: i64"
                 FROM authors a
                WHERE ?1 IS NULL OR a.name LIKE ?1 COLLATE NOCASE
                ORDER BY (SELECT COUNT(*) FROM books WHERE author_id = a.author_id) DESC,
                         a.name COLLATE NOCASE
                LIMIT ?2 OFFSET ?3"#,
            q_filter,
            limit,
            offset,
        )
        .fetch_all(pool)
        .await
        .map(|rows| {
            rows.into_iter()
                .map(|r| AuthorListItem {
                    author_id: r.author_id,
                    name: r.name,
                    name_sort: r.name_sort,
                    audible_id: r.audible_id,
                    image_url: r.image_url,
                    book_count: r.book_count,
                })
                .collect()
        }),
    }
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("authors list: {e}"))))?;

    Ok((
        StatusCode::OK,
        Json(AuthorsListResponse {
            authors,
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
    fn author_detail_serializes_with_expected_keys() {
        let d = AuthorDetail {
            author_id: 7,
            name: "Brandon Sanderson".into(),
            name_sort: Some("Sanderson, Brandon".into()),
            bio: Some("Acclaimed cosmere author...".into()),
            image_url: Some("https://m.media-amazon.com/x.jpg".into()),
            audible_id: Some("B001IGFHW6".into()),
            aliases: vec!["Brandon Sanderson".into(), "B. Sanderson".into()],
            book_count: 42,
            created_at: 1_700_000_000,
            updated_at: 1_770_000_000,
        };
        let json = serde_json::to_value(&d).unwrap();
        assert_eq!(json["author_id"], 7);
        assert_eq!(json["name"], "Brandon Sanderson");
        assert_eq!(json["bio"], "Acclaimed cosmere author...");
        assert_eq!(json["image_url"], "https://m.media-amazon.com/x.jpg");
        assert_eq!(json["audible_id"], "B001IGFHW6");
        assert_eq!(json["book_count"], 42);
        let aliases = json["aliases"].as_array().expect("aliases is array");
        assert_eq!(aliases.len(), 2);
        assert_eq!(aliases[0], "Brandon Sanderson");
        assert_eq!(aliases[1], "B. Sanderson");
    }

    #[test]
    fn author_detail_omits_nothing_when_serializing_nulls() {
        // Make sure NULL bio / image / etc. still serialize as
        // `null` (not absent) so clients can rely on the shape.
        let d = AuthorDetail {
            author_id: 1,
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
    fn clamp_limit_respects_bounds() {
        assert_eq!(clamp_limit(None), DEFAULT_LIMIT);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(-5)), 1);
        assert_eq!(clamp_limit(Some(MAX_LIMIT)), MAX_LIMIT);
        assert_eq!(clamp_limit(Some(MAX_LIMIT + 100)), MAX_LIMIT);
        assert_eq!(clamp_limit(Some(75)), 75);
    }

    #[test]
    fn clamp_offset_respects_bounds() {
        assert_eq!(clamp_offset(None), 0);
        assert_eq!(clamp_offset(Some(-1)), 0);
        assert_eq!(clamp_offset(Some(0)), 0);
        assert_eq!(clamp_offset(Some(250)), 250);
    }

    #[test]
    fn sort_parses_documented_keys() {
        assert_eq!(AuthorsSort::parse(None).unwrap(), AuthorsSort::Name);
        assert_eq!(
            AuthorsSort::parse(Some("")).unwrap(),
            AuthorsSort::Name,
            "empty string treated as default"
        );
        assert_eq!(AuthorsSort::parse(Some("name")).unwrap(), AuthorsSort::Name);
        assert_eq!(
            AuthorsSort::parse(Some("name_sort")).unwrap(),
            AuthorsSort::NameSort
        );
        assert_eq!(
            AuthorsSort::parse(Some("book_count")).unwrap(),
            AuthorsSort::BookCountDesc
        );
    }

    #[test]
    fn sort_rejects_unknown_with_400() {
        match AuthorsSort::parse(Some("created_at")) {
            Err(ApiError::BadRequest(msg)) => {
                assert!(msg.contains("created_at"));
                assert!(msg.contains("book_count"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn authors_list_response_serializes_with_pagination_keys() {
        let resp = AuthorsListResponse {
            authors: vec![AuthorListItem {
                author_id: 7,
                name: "Brandon Sanderson".into(),
                name_sort: Some("Sanderson, Brandon".into()),
                audible_id: Some("B001IGFHW6".into()),
                image_url: Some("https://example.invalid/x.jpg".into()),
                book_count: 42,
            }],
            total: 137,
            limit: 50,
            offset: 0,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["total"], 137);
        assert_eq!(json["limit"], 50);
        assert_eq!(json["offset"], 0);
        let items = json["authors"].as_array().expect("authors is array");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["author_id"], 7);
        assert_eq!(items[0]["name"], "Brandon Sanderson");
        assert_eq!(items[0]["book_count"], 42);
        // Compact shape — these heavier fields are NOT in the list row.
        assert!(items[0].get("bio").is_none(), "list row omits bio");
        assert!(items[0].get("aliases").is_none(), "list row omits aliases");
    }
}
