//! Reading state + media-progress endpoints (ADR-0033, slice B.6).
//!
//! Five routes:
//!
//! * `PATCH /books/{id}/status`  — operator-set reading status.
//! * `PATCH /books/{id}/rating`  — 1..=5 stars, `null` clears.
//! * `PATCH /books/{id}/notes`   — free-form text, `""`/whitespace clears.
//! * `POST  /session/{id}/sync`  — player position sync (LWW).
//! * `GET   /books/{id}/progress` — current durable position.

use ab_core::{BookId, ReadingStatus};
use ab_progress::{
    MediaProgress, ProgressError, SyncRequest, get, set_notes, set_rating, set_status, sync,
};
use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::state::ApiState;

impl From<ProgressError> for ApiError {
    fn from(err: ProgressError) -> Self {
        match err {
            ProgressError::NotFound(id) => Self::NotFound(format!("book {id}")),
            ProgressError::Core(e) => Self::Internal(e),
        }
    }
}

#[derive(Deserialize)]
pub struct StatusBody {
    pub reading_status: ReadingStatus,
}

#[derive(Serialize)]
pub struct Ack {
    pub ok: bool,
}

const ACK_OK: Ack = Ack { ok: true };

/// `PATCH /books/{id}/status` — set `books.reading_status`.
pub async fn books_status_patch(
    State(state): State<ApiState>,
    Path(book_id): Path<i64>,
    Json(body): Json<StatusBody>,
) -> Result<Json<Ack>, ApiError> {
    set_status(
        state.inner.library.pool(),
        BookId(book_id),
        body.reading_status,
    )
    .await?;
    Ok(Json(ACK_OK))
}

#[derive(Deserialize)]
pub struct RatingBody {
    /// `Some(1..=5)` to rate, `None` to clear.
    pub rating: Option<u8>,
}

/// `PATCH /books/{id}/rating` — 1..=5 stars, `null` clears.
pub async fn books_rating_patch(
    State(state): State<ApiState>,
    Path(book_id): Path<i64>,
    Json(body): Json<RatingBody>,
) -> Result<Json<Ack>, ApiError> {
    if let Some(r) = body.rating {
        if !(1..=5).contains(&r) {
            return Err(ApiError::BadRequest(format!(
                "rating must be 1..=5, got {r}"
            )));
        }
    }
    set_rating(state.inner.library.pool(), BookId(book_id), body.rating).await?;
    Ok(Json(ACK_OK))
}

#[derive(Deserialize)]
pub struct NotesBody {
    pub notes: Option<String>,
}

/// `PATCH /books/{id}/notes` — operator note; empty / whitespace clears.
pub async fn books_notes_patch(
    State(state): State<ApiState>,
    Path(book_id): Path<i64>,
    Json(body): Json<NotesBody>,
) -> Result<Json<Ack>, ApiError> {
    set_notes(
        state.inner.library.pool(),
        BookId(book_id),
        body.notes.as_deref(),
    )
    .await?;
    Ok(Json(ACK_OK))
}

#[derive(Serialize)]
pub struct SyncResponse {
    /// `true` when the report was written; `false` if a newer
    /// report had already landed (LWW dropped this one).
    pub accepted: bool,
}

/// `POST /session/{id}/sync` — record current player position.
pub async fn session_sync(
    State(state): State<ApiState>,
    Path(book_id): Path<i64>,
    Json(req): Json<SyncRequest>,
) -> Result<Json<SyncResponse>, ApiError> {
    let accepted = sync(state.inner.library.pool(), BookId(book_id), &req).await?;
    Ok(Json(SyncResponse { accepted }))
}

#[derive(Serialize)]
pub struct ProgressResponse {
    pub book_id: i64,
    pub current_time_ms: i64,
    pub is_finished: bool,
    pub last_listened_at: Option<i64>,
    pub last_synced_from: Option<String>,
    pub last_synced_at: Option<i64>,
}

impl From<MediaProgress> for ProgressResponse {
    fn from(p: MediaProgress) -> Self {
        Self {
            book_id: p.book_id.0,
            current_time_ms: p.current_time_ms,
            is_finished: p.is_finished,
            last_listened_at: p.last_listened_at,
            last_synced_from: p.last_synced_from,
            last_synced_at: p.last_synced_at,
        }
    }
}

/// `GET /books/{id}/progress` — current durable position. Returns
/// a zeroed row when the book exists but has no progress yet, so
/// clients never have to handle a 404 for "never played".
pub async fn books_progress_get(
    State(state): State<ApiState>,
    Path(book_id): Path<i64>,
) -> Result<Json<ProgressResponse>, ApiError> {
    let p = get(state.inner.library.pool(), BookId(book_id)).await?;
    Ok(Json(p.map_or(
        ProgressResponse {
            book_id,
            current_time_ms: 0,
            is_finished: false,
            last_listened_at: None,
            last_synced_from: None,
            last_synced_at: None,
        },
        ProgressResponse::from,
    )))
}
