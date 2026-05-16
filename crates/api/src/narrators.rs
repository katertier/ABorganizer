//! `GET /api/v1/narrators/{narrator_id}` — single-narrator read endpoint.
//!
//! Mirrors `authors_get` against the `narrators` table. Returns the
//! canonical narrator row + `book_count` (count of `book_narrator`
//! junction rows) so a narrator detail page can render without
//! hand-crafting joins. Aliases live in the `narrator_aliases`
//! junction (migration 013); the legacy `narrators.aliases`
//! newline-string column was dropped in migration 015.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, response::Response};
use serde::Serialize;

use crate::ApiError;
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
}
