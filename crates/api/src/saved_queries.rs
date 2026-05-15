//! Saved-queries endpoints (ADR-0034, slice B.12).
//!
//! Routes:
//!
//! * `GET    /api/v1/saved_queries`             — list (optional `?kind=`).
//! * `POST   /api/v1/saved_queries`             — create.
//! * `GET    /api/v1/saved_queries/{id}`        — read.
//! * `PATCH  /api/v1/saved_queries/{id}`        — update.
//! * `DELETE /api/v1/saved_queries/{id}`        — delete (rejects `system`-owned).
//! * `GET    /api/v1/saved_queries/{id}/items`  — execute + return rows.
//! * `GET    /api/v1/saved_queries/{id}/count`  — execute + return count only.

use ab_saved_queries::{
    CreateRequest, SavedQuery, SavedQueryError, SavedQueryKind, UpdateRequest, count, create,
    delete, execute, get, list,
};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::state::ApiState;

impl From<SavedQueryError> for ApiError {
    fn from(e: SavedQueryError) -> Self {
        match e {
            SavedQueryError::NotFound(id) => Self::NotFound(format!("saved query {id}")),
            SavedQueryError::SystemReadOnly(id) => {
                Self::BadRequest(format!("saved query {id} is system-owned and read-only"))
            }
            SavedQueryError::InvalidKind(k) => {
                Self::BadRequest(format!("invalid saved-query kind {k:?}"))
            }
            SavedQueryError::InvalidQuery(s) => Self::BadRequest(format!("invalid query: {s}")),
            SavedQueryError::Serde(s) => Self::BadRequest(format!("invalid body JSON: {s}")),
            SavedQueryError::Query(q) => Self::Internal(ab_core::Error::Database(q.to_string())),
            SavedQueryError::Db(d) => Self::Internal(ab_core::Error::Database(d.to_string())),
        }
    }
}

#[derive(Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Serialize)]
pub struct ListResponse {
    pub queries: Vec<SavedQuery>,
}

pub async fn saved_queries_list(
    State(state): State<ApiState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<ListResponse>, ApiError> {
    let kind = q.kind.as_deref().map(SavedQueryKind::parse).transpose()?;
    let queries = list(state.inner.library.pool(), kind).await?;
    Ok(Json(ListResponse { queries }))
}

#[derive(Serialize)]
pub struct CreateResponse {
    pub query_id: i64,
}

pub async fn saved_queries_create(
    State(state): State<ApiState>,
    Json(req): Json<CreateRequest>,
) -> Result<(StatusCode, Json<CreateResponse>), ApiError> {
    let id = create(state.inner.library.pool(), &req).await?;
    Ok((StatusCode::CREATED, Json(CreateResponse { query_id: id })))
}

pub async fn saved_queries_get(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
) -> Result<Json<SavedQuery>, ApiError> {
    let row = get(state.inner.library.pool(), id).await?;
    Ok(Json(row))
}

pub async fn saved_queries_update(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateRequest>,
) -> Result<StatusCode, ApiError> {
    ab_saved_queries::update(state.inner.library.pool(), id, &req).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn saved_queries_delete(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    delete(state.inner.library.pool(), id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct ItemsResponse {
    pub items: Vec<ab_query::BookListItem>,
}

pub async fn saved_queries_items(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
) -> Result<Json<ItemsResponse>, ApiError> {
    let items = execute(state.inner.library.pool(), id).await?;
    Ok(Json(ItemsResponse { items }))
}

#[derive(Serialize)]
pub struct CountResponse {
    pub count: u64,
}

pub async fn saved_queries_count(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
) -> Result<Json<CountResponse>, ApiError> {
    let n = count(state.inner.library.pool(), id).await?;
    Ok(Json(CountResponse { count: n }))
}
