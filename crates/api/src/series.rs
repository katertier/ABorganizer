//! `GET /api/v1/series/{series_id}` — single-series read endpoint.
//!
//! Mirrors `authors_get` / `narrators_get` against the `series`
//! table + `series_aliases` junction (migration 013). Surfaces
//! the canonical series row + `book_count` (count of
//! `book_series` rows — both primary + secondary; the rest of
//! the read path follows the same convention).
//!
//! `ended_state` is decoded to a human-readable string
//! (`"unknown"` / `"ongoing"` / `"ended"`) so clients don't need
//! the integer mapping.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, response::Response};
use serde::{Deserialize, Serialize};

use crate::ApiError;
use crate::state::ApiState;

/// Default `limit` for the list endpoint. Mirrors authors-list +
/// narrators-list.
const DEFAULT_LIMIT: i64 = 50;
/// Hard ceiling on `limit`. Larger requests clamp silently
/// (no 400) — same posture as the other entity-list endpoints.
const MAX_LIMIT: i64 = 200;

/// Series detail JSON returned by the GET endpoint.
#[derive(Debug, Serialize)]
pub struct SeriesDetail {
    pub series_id: i64,
    pub name: String,
    pub name_sort: Option<String>,
    /// Common title prefix across books in the series, used by
    /// `title_sort` to strip the prefix for shelf ordering.
    /// E.g. `"Mistborn: "` for "Mistborn: The Final Empire".
    pub franchise_prefix: Option<String>,
    /// Audible ASIN for the series, when known. Populated by
    /// `audnexus-enrich` from the Audible series object.
    pub audible_id: Option<String>,
    /// One of `"unknown"`, `"ongoing"`, `"ended"`. Decoded from
    /// the `series.ended_state` integer column (0/1/2).
    pub ended_state: &'static str,
    /// Observed-spelling variants from the `series_aliases`
    /// junction table. Sorted by observation order.
    pub aliases: Vec<String>,
    /// Number of `book_series` rows for this series (primary +
    /// secondary entries).
    pub book_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

const fn ended_state_str(code: i64) -> &'static str {
    match code {
        1 => "ongoing",
        2 => "ended",
        _ => "unknown",
    }
}

/// `GET /api/v1/series/{series_id}`
///
/// Returns `200 OK` with [`SeriesDetail`] JSON. `404 Not Found`
/// when no `series` row exists at that id.
///
/// # Errors
///
/// Database access failures surface as [`ApiError::Internal`].
#[allow(clippy::missing_panics_doc)] // panic-free
pub async fn series_get(
    State(state): State<ApiState>,
    Path(series_id): Path<i64>,
) -> Result<Response, ApiError> {
    let row = sqlx::query!(
        r#"SELECT s.series_id AS "series_id!: i64",
                  s.name AS "name!: String",
                  s.name_sort AS "name_sort?: String",
                  s.franchise_prefix AS "franchise_prefix?: String",
                  s.audible_id AS "audible_id?: String",
                  s.ended_state AS "ended_state?: i64",
                  s.created_at AS "created_at!: i64",
                  s.updated_at AS "updated_at!: i64",
                  (SELECT COUNT(*) FROM book_series WHERE series_id = s.series_id)
                      AS "book_count!: i64"
           FROM series s
           WHERE s.series_id = ?"#,
        series_id,
    )
    .fetch_optional(state.inner.library.pool())
    .await
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("series lookup: {e}"))))?;

    let Some(r) = row else {
        return Err(ApiError::NotFound(format!("series {series_id}")));
    };

    let aliases: Vec<String> = sqlx::query_scalar!(
        r#"SELECT alias AS "alias!: String"
             FROM series_aliases
            WHERE series_id = ?
            ORDER BY alias_id"#,
        series_id,
    )
    .fetch_all(state.inner.library.pool())
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "series aliases lookup: {e}"
        )))
    })?;

    let detail = SeriesDetail {
        series_id: r.series_id,
        name: r.name,
        name_sort: r.name_sort,
        franchise_prefix: r.franchise_prefix,
        audible_id: r.audible_id,
        ended_state: ended_state_str(r.ended_state.unwrap_or(0)),
        aliases,
        book_count: r.book_count,
        created_at: r.created_at,
        updated_at: r.updated_at,
    };
    Ok((StatusCode::OK, Json(detail)).into_response())
}

/// Compact row in the paginated series list.
///
/// Trimmed shape vs. [`SeriesDetail`] — drops `aliases` (junction
/// query per row would be O(n) round-trips) and timestamps.
/// `franchise_prefix` + `ended_state` stay because they're cheap
/// and useful for list UIs (group ongoing series, show shelf
/// prefix per row).
#[derive(Debug, Serialize)]
pub struct SeriesListItem {
    pub series_id: i64,
    pub name: String,
    pub name_sort: Option<String>,
    pub franchise_prefix: Option<String>,
    pub audible_id: Option<String>,
    pub ended_state: &'static str,
    pub book_count: i64,
}

/// Response body for `GET /api/v1/series`.
#[derive(Debug, Serialize)]
pub struct SeriesListResponse {
    pub series: Vec<SeriesListItem>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// Query-string params for the series list endpoint.
#[derive(Debug, Deserialize, Default)]
pub struct SeriesListQuery {
    /// Page size. Defaults to [`DEFAULT_LIMIT`] (50); clamped to
    /// [`MAX_LIMIT`] (200) silently. Negative / zero → 1.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Offset into the result set. Defaults to 0; negatives → 0.
    #[serde(default)]
    pub offset: Option<i64>,
    /// Sort key. One of:
    ///
    /// * `name` — by display name, case-insensitive (default).
    /// * `name_sort` — by `COALESCE(name_sort, name)`.
    /// * `book_count` — descending; surfaces the largest
    ///   series first.
    /// * `ended_state` — `ongoing` first, then `unknown`, then
    ///   `ended`, tied by name. Useful for "what am I following"
    ///   surfaces.
    ///
    /// Unknown values produce `400 Bad Request`.
    #[serde(default)]
    pub sort: Option<String>,
    /// Optional case-insensitive substring filter on `name`.
    /// Empty string is treated as "no filter".
    #[serde(default)]
    pub q: Option<String>,
}

/// Resolved sort key. Internal — surface uses the string form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SeriesSort {
    Name,
    NameSort,
    BookCountDesc,
    EndedState,
}

impl SeriesSort {
    fn parse(s: Option<&str>) -> Result<Self, ApiError> {
        match s {
            None | Some("" | "name") => Ok(Self::Name),
            Some("name_sort") => Ok(Self::NameSort),
            Some("book_count") => Ok(Self::BookCountDesc),
            Some("ended_state") => Ok(Self::EndedState),
            Some(other) => Err(ApiError::BadRequest(format!(
                "unknown sort {other:?}; expected one of name / name_sort / book_count / ended_state"
            ))),
        }
    }
}

const fn clamp_limit(raw: Option<i64>) -> i64 {
    match raw {
        None => DEFAULT_LIMIT,
        Some(n) if n <= 0 => 1,
        Some(n) if n > MAX_LIMIT => MAX_LIMIT,
        Some(n) => n,
    }
}

const fn clamp_offset(raw: Option<i64>) -> i64 {
    match raw {
        None => 0,
        Some(n) if n < 0 => 0,
        Some(n) => n,
    }
}

/// `GET /api/v1/series[?limit=&offset=&sort=&q=]`
///
/// Returns `200 OK` with [`SeriesListResponse`] JSON. `400 Bad
/// Request` for an unknown `sort` value.
///
/// # Errors
///
/// Database access failures surface as [`ApiError::Internal`].
#[allow(clippy::missing_panics_doc, clippy::too_many_lines)] // panic-free; macro arms inflate line count
pub async fn series_list(
    State(state): State<ApiState>,
    Query(params): Query<SeriesListQuery>,
) -> Result<Response, ApiError> {
    let sort = SeriesSort::parse(params.sort.as_deref())?;
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
             FROM series
            WHERE ?1 IS NULL OR name LIKE ?1 COLLATE NOCASE"#,
        q_filter,
    )
    .fetch_one(state.inner.library.pool())
    .await
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("series count: {e}"))))?;

    let pool = state.inner.library.pool();
    let series: Vec<SeriesListItem> = match sort {
        SeriesSort::Name => sqlx::query!(
            r#"SELECT s.series_id AS "series_id!: i64",
                      s.name AS "name!: String",
                      s.name_sort AS "name_sort?: String",
                      s.franchise_prefix AS "franchise_prefix?: String",
                      s.audible_id AS "audible_id?: String",
                      s.ended_state AS "ended_state?: i64",
                      (SELECT COUNT(*) FROM book_series WHERE series_id = s.series_id)
                          AS "book_count!: i64"
                 FROM series s
                WHERE ?1 IS NULL OR s.name LIKE ?1 COLLATE NOCASE
                ORDER BY s.name COLLATE NOCASE
                LIMIT ?2 OFFSET ?3"#,
            q_filter,
            limit,
            offset,
        )
        .fetch_all(pool)
        .await
        .map(|rows| {
            rows.into_iter()
                .map(|r| SeriesListItem {
                    series_id: r.series_id,
                    name: r.name,
                    name_sort: r.name_sort,
                    franchise_prefix: r.franchise_prefix,
                    audible_id: r.audible_id,
                    ended_state: ended_state_str(r.ended_state.unwrap_or(0)),
                    book_count: r.book_count,
                })
                .collect()
        }),
        SeriesSort::NameSort => sqlx::query!(
            r#"SELECT s.series_id AS "series_id!: i64",
                      s.name AS "name!: String",
                      s.name_sort AS "name_sort?: String",
                      s.franchise_prefix AS "franchise_prefix?: String",
                      s.audible_id AS "audible_id?: String",
                      s.ended_state AS "ended_state?: i64",
                      (SELECT COUNT(*) FROM book_series WHERE series_id = s.series_id)
                          AS "book_count!: i64"
                 FROM series s
                WHERE ?1 IS NULL OR s.name LIKE ?1 COLLATE NOCASE
                ORDER BY COALESCE(s.name_sort, s.name) COLLATE NOCASE
                LIMIT ?2 OFFSET ?3"#,
            q_filter,
            limit,
            offset,
        )
        .fetch_all(pool)
        .await
        .map(|rows| {
            rows.into_iter()
                .map(|r| SeriesListItem {
                    series_id: r.series_id,
                    name: r.name,
                    name_sort: r.name_sort,
                    franchise_prefix: r.franchise_prefix,
                    audible_id: r.audible_id,
                    ended_state: ended_state_str(r.ended_state.unwrap_or(0)),
                    book_count: r.book_count,
                })
                .collect()
        }),
        SeriesSort::BookCountDesc => sqlx::query!(
            r#"SELECT s.series_id AS "series_id!: i64",
                      s.name AS "name!: String",
                      s.name_sort AS "name_sort?: String",
                      s.franchise_prefix AS "franchise_prefix?: String",
                      s.audible_id AS "audible_id?: String",
                      s.ended_state AS "ended_state?: i64",
                      (SELECT COUNT(*) FROM book_series WHERE series_id = s.series_id)
                          AS "book_count!: i64"
                 FROM series s
                WHERE ?1 IS NULL OR s.name LIKE ?1 COLLATE NOCASE
                ORDER BY (SELECT COUNT(*) FROM book_series WHERE series_id = s.series_id) DESC,
                         s.name COLLATE NOCASE
                LIMIT ?2 OFFSET ?3"#,
            q_filter,
            limit,
            offset,
        )
        .fetch_all(pool)
        .await
        .map(|rows| {
            rows.into_iter()
                .map(|r| SeriesListItem {
                    series_id: r.series_id,
                    name: r.name,
                    name_sort: r.name_sort,
                    franchise_prefix: r.franchise_prefix,
                    audible_id: r.audible_id,
                    ended_state: ended_state_str(r.ended_state.unwrap_or(0)),
                    book_count: r.book_count,
                })
                .collect()
        }),
        SeriesSort::EndedState => sqlx::query!(
            // Ordering: ongoing (1) first, then unknown (0), then
            // ended (2). The CASE WHEN expression maps the raw
            // integer to the sort key (1 → 0, 0 → 1, 2 → 2) so
            // standard ASC ordering produces the right buckets.
            r#"SELECT s.series_id AS "series_id!: i64",
                      s.name AS "name!: String",
                      s.name_sort AS "name_sort?: String",
                      s.franchise_prefix AS "franchise_prefix?: String",
                      s.audible_id AS "audible_id?: String",
                      s.ended_state AS "ended_state?: i64",
                      (SELECT COUNT(*) FROM book_series WHERE series_id = s.series_id)
                          AS "book_count!: i64"
                 FROM series s
                WHERE ?1 IS NULL OR s.name LIKE ?1 COLLATE NOCASE
                ORDER BY CASE COALESCE(s.ended_state, 0)
                              WHEN 1 THEN 0
                              WHEN 0 THEN 1
                              WHEN 2 THEN 2
                              ELSE 3
                         END,
                         s.name COLLATE NOCASE
                LIMIT ?2 OFFSET ?3"#,
            q_filter,
            limit,
            offset,
        )
        .fetch_all(pool)
        .await
        .map(|rows| {
            rows.into_iter()
                .map(|r| SeriesListItem {
                    series_id: r.series_id,
                    name: r.name,
                    name_sort: r.name_sort,
                    franchise_prefix: r.franchise_prefix,
                    audible_id: r.audible_id,
                    ended_state: ended_state_str(r.ended_state.unwrap_or(0)),
                    book_count: r.book_count,
                })
                .collect()
        }),
    }
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("series list: {e}"))))?;

    Ok((
        StatusCode::OK,
        Json(SeriesListResponse {
            series,
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
    fn ended_state_codes_map_correctly() {
        assert_eq!(ended_state_str(0), "unknown");
        assert_eq!(ended_state_str(1), "ongoing");
        assert_eq!(ended_state_str(2), "ended");
        assert_eq!(ended_state_str(99), "unknown"); // unknown codes -> unknown
    }

    #[test]
    fn series_detail_serializes_with_expected_keys() {
        let d = SeriesDetail {
            series_id: 3,
            name: "Mistborn".into(),
            name_sort: Some("Mistborn".into()),
            franchise_prefix: Some("Mistborn: ".into()),
            audible_id: Some("B07FCMHPWY".into()),
            ended_state: "ongoing",
            aliases: vec!["Mistborn".into(), "Mistborn Saga".into()],
            book_count: 7,
            created_at: 1_700_000_000,
            updated_at: 1_770_000_000,
        };
        let json = serde_json::to_value(&d).unwrap();
        assert_eq!(json["series_id"], 3);
        assert_eq!(json["name"], "Mistborn");
        assert_eq!(json["franchise_prefix"], "Mistborn: ");
        assert_eq!(json["audible_id"], "B07FCMHPWY");
        assert_eq!(json["ended_state"], "ongoing");
        assert_eq!(json["book_count"], 7);
        let aliases = json["aliases"].as_array().expect("aliases is array");
        assert_eq!(aliases.len(), 2);
        assert_eq!(aliases[1], "Mistborn Saga");
    }

    #[test]
    fn series_detail_preserves_nulls() {
        let d = SeriesDetail {
            series_id: 1,
            name: "Untitled Series".into(),
            name_sort: None,
            franchise_prefix: None,
            audible_id: None,
            ended_state: "unknown",
            aliases: Vec::new(),
            book_count: 0,
            created_at: 0,
            updated_at: 0,
        };
        let json = serde_json::to_value(&d).unwrap();
        assert!(json["franchise_prefix"].is_null());
        assert!(json["audible_id"].is_null());
        assert_eq!(json["ended_state"], "unknown");
        assert!(json["aliases"].as_array().expect("array").is_empty());
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
        assert_eq!(SeriesSort::parse(None).unwrap(), SeriesSort::Name);
        assert_eq!(
            SeriesSort::parse(Some("")).unwrap(),
            SeriesSort::Name,
            "empty string treated as default"
        );
        assert_eq!(SeriesSort::parse(Some("name")).unwrap(), SeriesSort::Name);
        assert_eq!(
            SeriesSort::parse(Some("name_sort")).unwrap(),
            SeriesSort::NameSort
        );
        assert_eq!(
            SeriesSort::parse(Some("book_count")).unwrap(),
            SeriesSort::BookCountDesc
        );
        assert_eq!(
            SeriesSort::parse(Some("ended_state")).unwrap(),
            SeriesSort::EndedState
        );
    }

    #[test]
    fn sort_rejects_unknown_with_400() {
        match SeriesSort::parse(Some("created_at")) {
            Err(ApiError::BadRequest(msg)) => {
                assert!(msg.contains("created_at"));
                assert!(msg.contains("ended_state"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn series_list_response_serializes_with_pagination_keys() {
        let resp = SeriesListResponse {
            series: vec![SeriesListItem {
                series_id: 3,
                name: "Mistborn".into(),
                name_sort: Some("Mistborn".into()),
                franchise_prefix: Some("Mistborn: ".into()),
                audible_id: Some("B07FCMHPWY".into()),
                ended_state: "ongoing",
                book_count: 7,
            }],
            total: 84,
            limit: 50,
            offset: 0,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["total"], 84);
        assert_eq!(json["limit"], 50);
        assert_eq!(json["offset"], 0);
        let items = json["series"].as_array().expect("series is array");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["series_id"], 3);
        assert_eq!(items[0]["name"], "Mistborn");
        assert_eq!(items[0]["franchise_prefix"], "Mistborn: ");
        assert_eq!(items[0]["ended_state"], "ongoing");
        assert_eq!(items[0]["book_count"], 7);
        // Compact shape — aliases are NOT in the list row.
        assert!(items[0].get("aliases").is_none(), "list row omits aliases");
    }
}
