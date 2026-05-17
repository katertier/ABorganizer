//! Reading state + media-progress endpoints (ADR-0033, slice B.6).
//!
//! Five routes:
//!
//! * `PATCH /books/{id}/status`  — operator-set reading status.
//!   Captures an `operation_journal` row per ADR-0039 — pre-state
//!   carries the previous status + the intended new status so a
//!   future `Replayer` can re-execute idempotently after a crash.
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
use serde_json::{Value, json};

use crate::error::ApiError;
use crate::state::ApiState;

/// `op_kind` recorded in `operation_journal` for `PATCH /books/{id}/status`.
///
/// Stable string used by [`crate::journal_replayers::StatusReplayer`]
/// to claim rows from the registry on crash recovery / retry.
pub const OP_KIND_BOOK_STATUS_SET: &str = "book-status-set";

/// `op_kind` recorded in `operation_journal` for `PATCH /books/{id}/rating`.
///
/// `pre_state = { current: <prev>, intent: <new> }` where each
/// is either the integer rating (1..=5) or `null` for the
/// "no rating" state. [`crate::journal_replayers::RatingReplayer`]
/// reads this shape on retry / crash recovery.
pub const OP_KIND_BOOK_RATING_SET: &str = "book-rating-set";

/// `op_kind` recorded in `operation_journal` for `PATCH /books/{id}/notes`.
///
/// `pre_state = { current: <prev>, intent: <new> }` where each
/// is either the (whitespace-normalised) note string or `null`.
/// Notes are normalised at handler entry — both `pre_state.intent`
/// and `post_state.notes` reflect the persisted value, not the raw
/// operator input. The future `NotesReplayer` will use this shape.
pub const OP_KIND_BOOK_NOTES_SET: &str = "book-notes-set";

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

/// `PATCH /books/{id}/status` — set `books.reading_status` and
/// record the mutation in `operation_journal` (ADR-0039).
///
/// Sequence:
/// 1. Read the current `reading_status` (returns 404 if the book
///    is gone — the journal row is *not* created in that case
///    because there's nothing to replay).
/// 2. `journal::record` a `pending` row whose `pre_state` carries
///    both `current` (for drift detection) and `intent` (so a
///    future `Replayer` knows what to set).
/// 3. Call `set_status` to apply the mutation.
/// 4. `journal::mark_done` with `post_state = { reading_status }`
///    on success, or `journal::mark_failed` with the error string
///    on failure. The handler still returns the original error
///    after marking — `mark_failed` is best-effort.
pub async fn books_status_patch(
    State(state): State<ApiState>,
    Path(book_id): Path<i64>,
    Json(body): Json<StatusBody>,
) -> Result<Json<Ack>, ApiError> {
    let pool = state.inner.library.pool();

    let current: Option<String> = sqlx::query_scalar!(
        r#"SELECT reading_status AS "reading_status!: String"
             FROM books WHERE book_id = ?"#,
        book_id,
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "read pre-state for book {book_id}: {e}"
        )))
    })?;
    let Some(current) = current else {
        return Err(ApiError::NotFound(format!("book {book_id}")));
    };

    let intent = body.reading_status.as_str();
    let entry = ab_journal::NewEntry {
        op_kind: OP_KIND_BOOK_STATUS_SET,
        target: ab_journal::Target {
            kind: "book".to_owned(),
            id: book_id,
        },
        pre_state: json!({ "current": current, "intent": intent }),
        reversible: true,
        batch_id: None,
    };
    let op_id = crate::journal_capture::record_pending(pool, &entry).await?;

    match set_status(pool, BookId(book_id), body.reading_status).await {
        Ok(()) => {
            crate::journal_capture::mark_done_or_log(
                pool,
                op_id,
                &json!({ "reading_status": intent }),
                "api.books_status_patch",
            )
            .await;
            Ok(Json(ACK_OK))
        }
        Err(err) => {
            crate::journal_capture::mark_failed_or_log(
                pool,
                op_id,
                &err.to_string(),
                "api.books_status_patch",
            )
            .await;
            Err(ApiError::from(err))
        }
    }
}

#[derive(Deserialize)]
pub struct RatingBody {
    /// `Some(1..=5)` to rate, `None` to clear.
    pub rating: Option<u8>,
}

/// `PATCH /books/{id}/rating` — 1..=5 stars, `null` clears.
///
/// Captures the mutation in `operation_journal` (ADR-0039) — same
/// shape as [`books_status_patch`]. `pre_state.current` /
/// `pre_state.intent` are JSON numbers (1..=5) or `null` for the
/// no-rating state.
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
    let pool = state.inner.library.pool();

    let current: Option<Option<i64>> = sqlx::query_scalar!(
        r#"SELECT rating AS "rating: i64" FROM books WHERE book_id = ?"#,
        book_id,
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "read pre-state for book {book_id} rating: {e}"
        )))
    })?;
    let Some(current) = current else {
        return Err(ApiError::NotFound(format!("book {book_id}")));
    };

    let intent_json = body
        .rating
        .map_or(Value::Null, |r| Value::from(i64::from(r)));
    let current_json = current.map_or(Value::Null, Value::from);
    let entry = ab_journal::NewEntry {
        op_kind: OP_KIND_BOOK_RATING_SET,
        target: ab_journal::Target {
            kind: "book".to_owned(),
            id: book_id,
        },
        pre_state: json!({ "current": current_json, "intent": intent_json }),
        reversible: true,
        batch_id: None,
    };
    let op_id = crate::journal_capture::record_pending(pool, &entry).await?;

    match set_rating(pool, BookId(book_id), body.rating).await {
        Ok(()) => {
            crate::journal_capture::mark_done_or_log(
                pool,
                op_id,
                &json!({ "rating": intent_json }),
                "api.books_rating_patch",
            )
            .await;
            Ok(Json(ACK_OK))
        }
        Err(err) => {
            crate::journal_capture::mark_failed_or_log(
                pool,
                op_id,
                &err.to_string(),
                "api.books_rating_patch",
            )
            .await;
            Err(ApiError::from(err))
        }
    }
}

#[derive(Deserialize)]
pub struct NotesBody {
    pub notes: Option<String>,
}

/// `PATCH /books/{id}/notes` — operator note; empty / whitespace clears.
///
/// Captures the mutation in `operation_journal` (ADR-0039) — same
/// shape as the status/rating handlers. `set_notes` normalises
/// whitespace-only input to NULL; the journal mirrors the
/// normalised value so the future `NotesReplayer` can compare
/// `current` against `intent` directly without re-running the
/// normaliser.
pub async fn books_notes_patch(
    State(state): State<ApiState>,
    Path(book_id): Path<i64>,
    Json(body): Json<NotesBody>,
) -> Result<Json<Ack>, ApiError> {
    let pool = state.inner.library.pool();

    let current: Option<Option<String>> = sqlx::query_scalar!(
        r#"SELECT notes AS "notes: String" FROM books WHERE book_id = ?"#,
        book_id,
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "read pre-state for book {book_id} notes: {e}"
        )))
    })?;
    let Some(current) = current else {
        return Err(ApiError::NotFound(format!("book {book_id}")));
    };

    let intent_normalised: Option<String> = body
        .notes
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let intent_json = intent_normalised
        .as_deref()
        .map_or(Value::Null, Value::from);
    let current_json = current.as_deref().map_or(Value::Null, Value::from);
    let entry = ab_journal::NewEntry {
        op_kind: OP_KIND_BOOK_NOTES_SET,
        target: ab_journal::Target {
            kind: "book".to_owned(),
            id: book_id,
        },
        pre_state: json!({ "current": current_json, "intent": intent_json }),
        reversible: true,
        batch_id: None,
    };
    let op_id = crate::journal_capture::record_pending(pool, &entry).await?;

    match set_notes(pool, BookId(book_id), body.notes.as_deref()).await {
        Ok(()) => {
            crate::journal_capture::mark_done_or_log(
                pool,
                op_id,
                &json!({ "notes": intent_json }),
                "api.books_notes_patch",
            )
            .await;
            Ok(Json(ACK_OK))
        }
        Err(err) => {
            crate::journal_capture::mark_failed_or_log(
                pool,
                op_id,
                &err.to_string(),
                "api.books_notes_patch",
            )
            .await;
            Err(ApiError::from(err))
        }
    }
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
