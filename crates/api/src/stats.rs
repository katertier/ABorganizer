//! Stats endpoints (ADR-0044, slice B.17).
//!
//! Two routes:
//!
//! * `GET /api/v1/stats`                         — counts + listening totals.
//! * `GET /api/v1/stats/breakdown?dimension=…`   — pie-chart bucket list.

use ab_stats::{BreakdownResponse, Dimension, StatsError, StatsResponse, breakdown, stats};
use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;

use crate::error::ApiError;
use crate::state::ApiState;

impl From<StatsError> for ApiError {
    fn from(e: StatsError) -> Self {
        match e {
            StatsError::UnsupportedDimension(s) => Self::BadRequest(format!(
                "unsupported dimension {s:?}; supported: language, length, reading_status, acquisition_year, decade, publisher, format, author, narrator, series, collection, audiologo_status, abridged, rating"
            )),
            StatsError::Db(db) => Self::Internal(ab_core::Error::Database(db.to_string())),
        }
    }
}

/// `GET /api/v1/stats` — counts + listening totals.
pub async fn stats_get(State(state): State<ApiState>) -> Result<Json<StatsResponse>, ApiError> {
    let s = stats(state.inner.library.pool()).await?;
    Ok(Json(s))
}

#[derive(Deserialize)]
pub struct BreakdownQuery {
    pub dimension: String,
}

/// `GET /api/v1/stats/breakdown?dimension=<dim>` — pie-chart data.
pub async fn breakdown_get(
    State(state): State<ApiState>,
    Query(q): Query<BreakdownQuery>,
) -> Result<Json<BreakdownResponse>, ApiError> {
    let dim = Dimension::parse(&q.dimension)?;
    let b = breakdown(state.inner.library.pool(), dim).await?;
    Ok(Json(b))
}
