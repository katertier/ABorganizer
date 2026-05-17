//! ASIN-learnings management endpoints.
//!
//! `ab_catalog::asin_learnings::capture()` writes a row every
//! time the operator sets `asin` on a book (PR #177). The
//! audible-search stage consults the same table on lookup as a
//! pre-network hint (PR #178). These endpoints close the operator
//! loop:
//!
//! * `GET /asin_learnings` — paginated read of the table so the
//!   operator (or a future UI) can see what the system has
//!   memorised.
//! * `DELETE /asin_learnings/{id}` — drop one bad learning. The
//!   row is gone; the next ingest of a matching `(title, author)`
//!   key will go back to hitting Audible.
//!
//! No update endpoint by design: a "wrong" learning gets deleted
//! and re-captured the next time the operator sets the right
//! ASIN on a book.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::pagination::{clamp_limit, clamp_offset};
use crate::state::ApiState;

/// One row in the auto-learn table, as surfaced to API clients.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AsinLearningRow {
    /// Surrogate primary key. Use this in the
    /// `DELETE /asin_learnings/{learning_id}` URL.
    pub learning_id: i64,
    /// Normalised title key (the lookup index uses this).
    pub title_norm: String,
    /// Normalised author key.
    pub author_norm: String,
    /// The remembered ASIN.
    pub asin: String,
    /// Capture provenance — `"user_edit"` for the PATCH path,
    /// future capture sites pick their own tag.
    pub source: String,
    /// ISO 8601 UTC of the capture moment.
    pub learned_at: String,
}

/// Optional query-string params for `GET /asin_learnings`.
#[derive(Debug, Deserialize, Default)]
pub struct AsinLearningsListQuery {
    /// 1..=200, defaults to 50; clamped silently per
    /// [`crate::pagination`].
    pub limit: Option<i64>,
    /// 0-based offset, clamped to >= 0.
    pub offset: Option<i64>,
}

/// Response shape for `GET /asin_learnings`.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AsinLearningsListResponse {
    pub rows: Vec<AsinLearningRow>,
    /// Total row count in the table (unclamped). Clients render
    /// "N of M" without a second call.
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// Response shape for `DELETE /asin_learnings/{learning_id}`.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AsinLearningDeleteResponse {
    pub learning_id: i64,
    pub deleted: bool,
}

/// `GET /api/v1/asin_learnings` — list learnings ordered by
/// `learned_at DESC` (most recent first).
///
/// # Errors
///
/// Returns [`ApiError::Internal`] for any DB failure.
pub async fn asin_learnings_list(
    State(state): State<ApiState>,
    Query(query): Query<AsinLearningsListQuery>,
) -> Result<Json<AsinLearningsListResponse>, ApiError> {
    let limit = clamp_limit(query.limit);
    let offset = clamp_offset(query.offset);
    let total: i64 = sqlx::query_scalar!("SELECT COUNT(*) FROM asin_learnings")
        .fetch_one(state.inner.library.pool())
        .await
        .map_err(|e| {
            ApiError::Internal(ab_core::Error::Database(format!(
                "asin_learnings count: {e}"
            )))
        })?;
    let rows = sqlx::query!(
        r#"SELECT learning_id  AS "learning_id!: i64",
                  title_norm   AS "title_norm!: String",
                  author_norm  AS "author_norm!: String",
                  asin         AS "asin!: String",
                  source       AS "source!: String",
                  learned_at   AS "learned_at!: String"
             FROM asin_learnings
            ORDER BY learned_at DESC, learning_id DESC
            LIMIT ? OFFSET ?"#,
        limit,
        offset,
    )
    .fetch_all(state.inner.library.pool())
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "asin_learnings list: {e}"
        )))
    })?;
    let rows = rows
        .into_iter()
        .map(|r| AsinLearningRow {
            learning_id: r.learning_id,
            title_norm: r.title_norm,
            author_norm: r.author_norm,
            asin: r.asin,
            source: r.source,
            learned_at: r.learned_at,
        })
        .collect();
    Ok(Json(AsinLearningsListResponse {
        rows,
        total,
        limit,
        offset,
    }))
}

/// `DELETE /api/v1/asin_learnings/{learning_id}` — drop one row.
///
/// Returns `200 { learning_id, deleted: true }` on a hit,
/// `200 { learning_id, deleted: false }` for an unknown id
/// (idempotent), and never errors on a normal not-found.
///
/// # Errors
///
/// Returns [`ApiError::Internal`] for any DB failure.
pub async fn asin_learnings_delete(
    State(state): State<ApiState>,
    Path(learning_id): Path<i64>,
) -> Result<(StatusCode, Json<AsinLearningDeleteResponse>), ApiError> {
    let result = sqlx::query!(
        "DELETE FROM asin_learnings WHERE learning_id = ?",
        learning_id,
    )
    .execute(state.inner.library.pool())
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "asin_learnings delete: {e}"
        )))
    })?;
    let deleted = result.rows_affected() > 0;
    Ok((
        StatusCode::OK,
        Json(AsinLearningDeleteResponse {
            learning_id,
            deleted,
        }),
    ))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn list_response_serializes_with_pagination_keys() {
        let r = AsinLearningsListResponse {
            rows: vec![],
            total: 0,
            limit: 50,
            offset: 0,
        };
        let json = serde_json::to_string(&r).expect("serialize");
        assert!(json.contains("\"rows\""));
        assert!(json.contains("\"total\""));
        assert!(json.contains("\"limit\""));
        assert!(json.contains("\"offset\""));
    }

    #[test]
    fn delete_response_signals_outcome() {
        let r = AsinLearningDeleteResponse {
            learning_id: 7,
            deleted: true,
        };
        let json = serde_json::to_string(&r).expect("serialize");
        assert!(json.contains("\"learning_id\":7"));
        assert!(json.contains("\"deleted\":true"));
    }
}
