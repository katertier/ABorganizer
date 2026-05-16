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

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, response::Response};
use serde::Serialize;

use crate::ApiError;
use crate::state::ApiState;

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
}
