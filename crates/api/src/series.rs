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

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, response::Response};
use serde::Serialize;

use crate::ApiError;
use crate::state::ApiState;

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
}
