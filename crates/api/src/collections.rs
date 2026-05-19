//! `GET /api/v1/collections` list + `GET /api/v1/collections/{collection_id}`
//! detail. Reads from the `book_collections` + `book_collection_members`
//! tables landed in migration 043.
//!
//! Collections are box sets, publisher bundles, and operator-curated
//! groupings — distinct from series (which carry authoritative
//! ordering). The endpoints surface the same shape as `/publishers`:
//! paginated list with name + `book_count` + `audible_id`, single-row
//! detail with the full row + `book_count`.
//!
//! Books-in-collection (`/collections/{id}/books`) ships in a
//! follow-up slice; the slim list + detail endpoints land first so
//! GUIs can render the collection picker without a JOIN.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, response::Response};
use serde::{Deserialize, Serialize};

use crate::ApiError;
use crate::pagination::{clamp_limit, clamp_offset};
use crate::state::ApiState;

/// Collection detail JSON returned by the GET endpoint.
#[derive(Debug, Serialize)]
pub struct CollectionDetail {
    pub collection_id: i64,
    pub name: String,
    /// Normalised form; populated by future canonical-collection
    /// enrich work. `null` for scanner-detected / operator-curated
    /// collections that haven't been enriched yet.
    pub canonical_name: Option<String>,
    /// Audible collection ASIN. `null` when the collection isn't
    /// mirrored on Audible (operator-curated or scanner-detected
    /// bundles).
    pub audible_id: Option<String>,
    pub description: Option<String>,
    /// Free-text classification: `box_set`, `compilation`,
    /// `curated`, … See migration 043 header for the rationale
    /// against an enum today.
    pub kind: Option<String>,
    /// Member count — JOIN on `book_collection_members` (no
    /// CASCADE-aware filter needed; member rows go away with
    /// their parent collection or book).
    pub book_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// `GET /api/v1/collections/{collection_id}`
///
/// Returns `200 OK` with [`CollectionDetail`] JSON. `404 Not Found`
/// when no `book_collections` row exists at that id.
///
/// # Errors
///
/// Database access failures surface as [`ApiError::Internal`].
#[allow(clippy::missing_panics_doc)] // panic-free
pub async fn collections_get(
    State(state): State<ApiState>,
    Path(collection_id): Path<i64>,
) -> Result<Response, ApiError> {
    let row = sqlx::query!(
        r#"SELECT c.collection_id  AS "collection_id!: i64",
                  c.name           AS "name!: String",
                  c.canonical_name AS "canonical_name?: String",
                  c.audible_id     AS "audible_id?: String",
                  c.description    AS "description?: String",
                  c.kind           AS "kind?: String",
                  c.created_at     AS "created_at!: i64",
                  c.updated_at     AS "updated_at!: i64",
                  (SELECT COUNT(*) FROM book_collection_members
                    WHERE collection_id = c.collection_id)
                      AS "book_count!: i64"
             FROM book_collections c
            WHERE c.collection_id = ?"#,
        collection_id,
    )
    .fetch_optional(state.inner.library.pool())
    .await
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("collection lookup: {e}"))))?;

    let Some(r) = row else {
        return Err(ApiError::NotFound(format!("collection {collection_id}")));
    };

    let detail = CollectionDetail {
        collection_id: r.collection_id,
        name: r.name,
        canonical_name: r.canonical_name,
        audible_id: r.audible_id,
        description: r.description,
        kind: r.kind,
        book_count: r.book_count,
        created_at: r.created_at,
        updated_at: r.updated_at,
    };
    Ok((StatusCode::OK, Json(detail)).into_response())
}

/// Compact row in the paginated collections list. Drops `description`
/// (potentially long), `created_at`, `updated_at`.
#[derive(Debug, Serialize)]
pub struct CollectionListItem {
    pub collection_id: i64,
    pub name: String,
    pub canonical_name: Option<String>,
    pub audible_id: Option<String>,
    pub kind: Option<String>,
    pub book_count: i64,
}

/// Response body for `GET /api/v1/collections`.
#[derive(Debug, Serialize)]
pub struct CollectionsListResponse {
    pub collections: Vec<CollectionListItem>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// Query-string params for the collections list endpoint.
#[derive(Debug, Deserialize, Default)]
pub struct CollectionsListQuery {
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
    /// Optional `kind` filter (`box_set`, `compilation`, …). Pass
    /// the literal value; empty string = no filter.
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CollectionsSort {
    Name,
    BookCountDesc,
}

impl CollectionsSort {
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

/// `GET /api/v1/collections[?limit=&offset=&sort=&q=&kind=]`
///
/// Returns `200 OK` with [`CollectionsListResponse`] JSON. `400 Bad
/// Request` for an unknown `sort` value.
///
/// # Errors
///
/// Database access failures surface as [`ApiError::Internal`].
#[allow(clippy::missing_panics_doc, clippy::too_many_lines)] // panic-free; macro arms inflate line count
pub async fn collections_list(
    State(state): State<ApiState>,
    Query(params): Query<CollectionsListQuery>,
) -> Result<Response, ApiError> {
    let sort = CollectionsSort::parse(params.sort.as_deref())?;
    let limit = clamp_limit(params.limit);
    let offset = clamp_offset(params.offset);

    let q_filter = params
        .q
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| format!("%{s}%"));
    let kind_filter = params
        .kind
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let total: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!: i64"
             FROM book_collections
            WHERE (?1 IS NULL OR name LIKE ?1 COLLATE NOCASE)
              AND (?2 IS NULL OR kind = ?2)"#,
        q_filter,
        kind_filter,
    )
    .fetch_one(state.inner.library.pool())
    .await
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("collections count: {e}"))))?;

    let pool = state.inner.library.pool();
    let collections: Vec<CollectionListItem> = match sort {
        CollectionsSort::Name => sqlx::query!(
            r#"SELECT c.collection_id  AS "collection_id!: i64",
                      c.name           AS "name!: String",
                      c.canonical_name AS "canonical_name?: String",
                      c.audible_id     AS "audible_id?: String",
                      c.kind           AS "kind?: String",
                      (SELECT COUNT(*) FROM book_collection_members
                        WHERE collection_id = c.collection_id)
                          AS "book_count!: i64"
                 FROM book_collections c
                WHERE (?1 IS NULL OR c.name LIKE ?1 COLLATE NOCASE)
                  AND (?2 IS NULL OR c.kind = ?2)
                ORDER BY c.name COLLATE NOCASE
                LIMIT ?3 OFFSET ?4"#,
            q_filter,
            kind_filter,
            limit,
            offset,
        )
        .fetch_all(pool)
        .await
        .map(|rows| {
            rows.into_iter()
                .map(|r| CollectionListItem {
                    collection_id: r.collection_id,
                    name: r.name,
                    canonical_name: r.canonical_name,
                    audible_id: r.audible_id,
                    kind: r.kind,
                    book_count: r.book_count,
                })
                .collect()
        }),
        CollectionsSort::BookCountDesc => sqlx::query!(
            r#"SELECT c.collection_id  AS "collection_id!: i64",
                      c.name           AS "name!: String",
                      c.canonical_name AS "canonical_name?: String",
                      c.audible_id     AS "audible_id?: String",
                      c.kind           AS "kind?: String",
                      (SELECT COUNT(*) FROM book_collection_members
                        WHERE collection_id = c.collection_id)
                          AS "book_count!: i64"
                 FROM book_collections c
                WHERE (?1 IS NULL OR c.name LIKE ?1 COLLATE NOCASE)
                  AND (?2 IS NULL OR c.kind = ?2)
                ORDER BY (SELECT COUNT(*) FROM book_collection_members
                           WHERE collection_id = c.collection_id) DESC,
                         c.name COLLATE NOCASE
                LIMIT ?3 OFFSET ?4"#,
            q_filter,
            kind_filter,
            limit,
            offset,
        )
        .fetch_all(pool)
        .await
        .map(|rows| {
            rows.into_iter()
                .map(|r| CollectionListItem {
                    collection_id: r.collection_id,
                    name: r.name,
                    canonical_name: r.canonical_name,
                    audible_id: r.audible_id,
                    kind: r.kind,
                    book_count: r.book_count,
                })
                .collect()
        }),
    }
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("collections list: {e}"))))?;

    Ok((
        StatusCode::OK,
        Json(CollectionsListResponse {
            collections,
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
    fn collection_detail_serializes_with_expected_keys() {
        let d = CollectionDetail {
            collection_id: 9,
            name: "The Stormlight Box Set".into(),
            canonical_name: Some("stormlight-box-set".into()),
            audible_id: Some("B0BOXSET01".into()),
            description: Some("Books 1-4 of the cosmere sequence.".into()),
            kind: Some("box_set".into()),
            book_count: 4,
            created_at: 1_700_000_000,
            updated_at: 1_770_000_000,
        };
        let json = serde_json::to_value(&d).unwrap();
        assert_eq!(json["collection_id"], 9);
        assert_eq!(json["name"], "The Stormlight Box Set");
        assert_eq!(json["canonical_name"], "stormlight-box-set");
        assert_eq!(json["audible_id"], "B0BOXSET01");
        assert_eq!(json["kind"], "box_set");
        assert_eq!(json["book_count"], 4);
    }

    #[test]
    fn collection_detail_preserves_null_optional_fields() {
        let d = CollectionDetail {
            collection_id: 1,
            name: "Unnamed".into(),
            canonical_name: None,
            audible_id: None,
            description: None,
            kind: None,
            book_count: 0,
            created_at: 0,
            updated_at: 0,
        };
        let json = serde_json::to_value(&d).unwrap();
        for key in ["canonical_name", "audible_id", "description", "kind"] {
            assert!(json.get(key).is_some());
            assert!(json[key].is_null());
        }
    }

    #[test]
    fn sort_parses_documented_keys() {
        assert_eq!(CollectionsSort::parse(None).unwrap(), CollectionsSort::Name);
        assert_eq!(
            CollectionsSort::parse(Some("")).unwrap(),
            CollectionsSort::Name
        );
        assert_eq!(
            CollectionsSort::parse(Some("name")).unwrap(),
            CollectionsSort::Name
        );
        assert_eq!(
            CollectionsSort::parse(Some("book_count")).unwrap(),
            CollectionsSort::BookCountDesc
        );
    }

    #[test]
    fn sort_rejects_unknown_with_400() {
        match CollectionsSort::parse(Some("kind")) {
            Err(ApiError::BadRequest(msg)) => {
                assert!(msg.contains("kind"));
                assert!(msg.contains("book_count"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn collections_list_response_serializes_with_pagination_keys() {
        let r = CollectionsListResponse {
            collections: vec![CollectionListItem {
                collection_id: 9,
                name: "The Stormlight Box Set".into(),
                canonical_name: Some("stormlight-box-set".into()),
                audible_id: Some("B0BOXSET01".into()),
                kind: Some("box_set".into()),
                book_count: 4,
            }],
            total: 42,
            limit: 50,
            offset: 0,
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["total"], 42);
        assert_eq!(json["limit"], 50);
        assert_eq!(json["offset"], 0);
        let items = json["collections"]
            .as_array()
            .expect("collections is array");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["collection_id"], 9);
        assert_eq!(items[0]["kind"], "box_set");
        assert_eq!(items[0]["book_count"], 4);
    }
}
