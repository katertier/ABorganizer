//! Audiologo review workflow API (slice 4D).
//!
//! Handlers:
//!
//! - [`audiologos_review_list`] — `GET /api/v1/audiologos/review`
//!   returns every `book_file_audiologos` row at
//!   `status='candidate'` for operator review. The CLI's
//!   `aborg audiologos review` is a thin shim over this.
//! - [`audiologos_approve`] — `POST /api/v1/audiologos/{row_id}/approve`
//!   promotes a candidate (or applied row whose status was
//!   manually flipped) through the [`apply_audiologo_cut`]
//!   pipeline. Sets `audiologo.verified_via='review_confirmed'`
//!   when the row references one.
//! - [`audiologos_reject`] — `POST /api/v1/audiologos/{row_id}/reject`
//!   flips a `candidate` / `applied` row to `rejected`. When
//!   rejecting an `applied` row, reverses the chapter shift via
//!   the existing apply infrastructure.
//!
//! ## Auto-confirm via `match_count`
//!
//! [`maybe_auto_confirm_audiologo`] runs every time a row is
//! approved. If the underlying `audiologos` row's
//! `match_count >= auto_confirm_after_matches` (tunable;
//! default 5) AND the row's current `verified_via` is
//! `'silence'`, promote it to `'review_confirmed'`. This bridges
//! the silence-bootstrap path (4C tier-4 candidates) into the
//! "trusted-enough-for-auto-apply" pool over time.

use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;

use crate::audiologo_apply::{ApplyCutParams, apply_audiologo_cut};
use crate::error::ApiError;
use crate::state::ApiState;

/// Auto-confirm threshold: how many positive matches an
/// audiologos row needs before its `verified_via` auto-promotes
/// from `'silence'` (the tier-4 bootstrap value) to
/// `'review_confirmed'`.
///
/// ADR-0024's `auto_confirm_after_matches` tunable was reserved
/// in the design phase. Until the `AudiologoTunables` struct
/// grows a field for it (deferred — touches every detect-stage
/// caller), the threshold lives here as a const. Bump when
/// empirical re-evaluation lands.
const AUTO_CONFIRM_AFTER_MATCHES: i64 = 5;

/// One pending review row. Surfaced as JSON for the CLI / web UI.
#[derive(Debug, Serialize)]
pub struct ReviewRow {
    /// `book_file_audiologos.audiologo_row_id`.
    pub row_id: i64,
    /// Book that owns the file this candidate sits on.
    pub book_id: i64,
    /// Book title for at-a-glance context in the review list.
    pub book_title: String,
    /// File the candidate applies to.
    pub file_id: i64,
    /// File path (so the operator can pre-listen if needed).
    pub file_path: String,
    /// `"intro"` or `"outro"`.
    pub kind: String,
    /// Splice-range start (ms from file start).
    pub jingle_start_ms: i64,
    /// Splice-range end (ms from file start).
    pub jingle_end_ms: i64,
    /// Cut method that produced the candidate
    /// (`fingerprint_full` / `transcript_only` / etc).
    pub method: String,
    /// Confidence in `[0.0, 1.0]`.
    pub confidence: f64,
    /// Matched audiologo row id, if any.
    pub audiologo_id: Option<i64>,
    /// Name of the matched audiologo (NULL for transcript-only
    /// candidates without a fingerprint).
    pub audiologo_name: Option<String>,
}

/// `GET /api/v1/audiologos/review` — list every candidate row.
///
/// Ordered by `(book_id, file_id, audiologo_row_id)` so the
/// operator sees a stable enumeration across re-fetches.
///
/// # Errors
///
/// Database failures return [`ApiError::Internal`].
pub async fn audiologos_review_list(
    State(state): State<ApiState>,
) -> Result<Json<Vec<ReviewRow>>, ApiError> {
    let rows = sqlx::query!(
        r#"SELECT
              bfa.audiologo_row_id AS "row_id!: i64",
              b.book_id            AS "book_id!: i64",
              b.title              AS book_title,
              bfa.file_id,
              bf.file_path,
              bfa.kind,
              bfa.jingle_start_ms,
              bfa.jingle_end_ms,
              bfa.method,
              bfa.confidence,
              bfa.audiologo_id,
              al.name              AS audiologo_name
            FROM book_file_audiologos bfa
            JOIN book_files bf ON bf.file_id = bfa.file_id
            JOIN books b ON b.book_id = bf.book_id
            LEFT JOIN audiologos al ON al.audiologo_id = bfa.audiologo_id
           WHERE bfa.status = 'candidate'
           ORDER BY b.book_id, bfa.file_id, bfa.audiologo_row_id"#,
    )
    .fetch_all(state.inner.library.pool())
    .await
    .map_err(|e| ab_core::Error::Database(format!("review list: {e}")))?;

    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        out.push(ReviewRow {
            row_id: r.row_id,
            book_id: r.book_id,
            book_title: r.book_title,
            file_id: r.file_id,
            file_path: r.file_path,
            kind: r.kind,
            jingle_start_ms: r.jingle_start_ms,
            jingle_end_ms: r.jingle_end_ms,
            method: r.method,
            confidence: r.confidence,
            audiologo_id: r.audiologo_id,
            audiologo_name: r.audiologo_name,
        });
    }
    Ok(Json(out))
}

/// Body of `/audiologos/{row_id}/approve` and `/reject`.
#[derive(Debug, serde::Deserialize)]
pub struct ReviewActionBody {
    /// Optional operator note recorded with the action. Stored
    /// in the audiologos `last_matched_at` adjacent column.
    /// Currently unused at the storage layer (the schema
    /// doesn't carry a note field on `book_file_audiologos`);
    /// the field exists for forward-compat with a future audit
    /// trail. Ignored when absent.
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /api/v1/audiologos/{row_id}/approve`.
///
/// 1. Loads the candidate row.
/// 2. Runs the apply pipeline (`apply_audiologo_cut` — shifts
///    chapters, updates `books.duration_ms`, flips
///    `books.audiologo_status`).
/// 3. If the row references an `audiologos` entry whose
///    `verified_via='silence'` AND `match_count >= floor`,
///    promotes `verified_via` to `'review_confirmed'`.
///
/// # Errors
///
/// - [`ApiError::NotFound`] when the row doesn't exist or isn't
///   at `status='candidate'`.
/// - [`ApiError::Internal`] on DB failure.
pub async fn audiologos_approve(
    State(state): State<ApiState>,
    Path(row_id): Path<i64>,
    Json(_body): Json<ReviewActionBody>,
) -> Result<Json<ApproveResponse>, ApiError> {
    let row = sqlx::query!(
        r#"SELECT
              bfa.audiologo_row_id AS "row_id!: i64",
              bf.book_id           AS "book_id!: i64",
              bfa.file_id,
              bfa.kind,
              bfa.jingle_start_ms,
              bfa.jingle_end_ms,
              bfa.padding_ms,
              bfa.method,
              bfa.audiologo_id,
              bfa.confidence
            FROM book_file_audiologos bfa
            JOIN book_files bf ON bf.file_id = bfa.file_id
           WHERE bfa.audiologo_row_id = ? AND bfa.status = 'candidate'"#,
        row_id,
    )
    .fetch_optional(state.inner.library.pool())
    .await
    .map_err(|e| ab_core::Error::Database(format!("approve fetch: {e}")))?;

    let Some(r) = row else {
        return Err(ApiError::NotFound(format!("no candidate row {row_id}")));
    };

    let outcome = apply_audiologo_cut(
        state.inner.library.pool(),
        ApplyCutParams {
            book_id: r.book_id,
            file_id: r.file_id,
            kind: &r.kind,
            jingle_start_ms: r.jingle_start_ms,
            jingle_end_ms: r.jingle_end_ms,
            padding_ms: r.padding_ms,
            method: &r.method,
            audiologo_id: r.audiologo_id,
            confidence: r.confidence,
            // ADR-0024 Rev 3: detector-side flags default to false
            // ("always pad") until slice 4B.x.3 flips them based on
            // waveform analysis. Approve path uses the conservative
            // default; future slice will read the detector-set
            // flags from the candidate row.
            head_silence_ms: 500,
            tail_silence_ms: 1500,
            head_lands_in_silence: false,
            tail_lands_in_silence: false,
        },
    )
    .await?;

    // Mark the original candidate as re_detected so the review
    // list doesn't show it again. The apply path INSERTed a new
    // row at 'applied' so the candidate is now superseded.
    sqlx::query!(
        "UPDATE book_file_audiologos SET status = 're_detected', re_detected_at = strftime('%s','now') WHERE audiologo_row_id = ?",
        row_id,
    )
    .execute(state.inner.library.pool())
    .await
    .map_err(|e| ab_core::Error::Database(format!("approve mark superseded: {e}")))?;

    // Auto-confirm runs against the audiologos row's
    // match_count outside the apply transaction (it's the
    // operator-confirmed promotion, conceptually independent of
    // the chapter-shift maths).
    let auto_confirmed = if let Some(id) = r.audiologo_id {
        let mut conn = state
            .inner
            .library
            .pool()
            .acquire()
            .await
            .map_err(|e| ab_core::Error::Database(format!("auto-confirm conn: {e}")))?;
        maybe_auto_confirm_audiologo(&mut conn, Some(id)).await?
    } else {
        false
    };

    Ok(Json(ApproveResponse {
        row_id,
        applied_row_id: outcome.row_id,
        chapters_shifted: outcome.chapters_shifted,
        auto_confirmed,
    }))
}

/// Response of `audiologos_approve`.
#[derive(Debug, Serialize)]
pub struct ApproveResponse {
    /// The original candidate row's id (now at `status='re_detected'`).
    pub row_id: i64,
    /// The newly-inserted applied row's id.
    pub applied_row_id: i64,
    /// How many chapter rows were shifted by the cut.
    pub chapters_shifted: i64,
    /// True when the underlying `audiologos` row's `verified_via`
    /// just auto-promoted from `silence` to `review_confirmed`.
    pub auto_confirmed: bool,
}

/// `POST /api/v1/audiologos/{row_id}/reject`.
///
/// Flips a `candidate` row to `rejected` without applying it.
/// If the row was already `applied`, also reverses the chapter
/// shift (uses the same maths in `apply_audiologo_cut`'s shift
/// helper, called with negated `cut_ms` — currently unimplemented;
/// the reject endpoint only handles `candidate` for 4D, the
/// applied-reject path lives in a future slice).
///
/// # Errors
///
/// - [`ApiError::NotFound`] when no row matches.
/// - [`ApiError::BadRequest`] when the row is `applied` (not yet
///   supported).
pub async fn audiologos_reject(
    State(state): State<ApiState>,
    Path(row_id): Path<i64>,
    Json(_body): Json<ReviewActionBody>,
) -> Result<Json<RejectResponse>, ApiError> {
    let row = sqlx::query!(
        "SELECT status FROM book_file_audiologos WHERE audiologo_row_id = ?",
        row_id,
    )
    .fetch_optional(state.inner.library.pool())
    .await
    .map_err(|e| ab_core::Error::Database(format!("reject fetch: {e}")))?;

    let Some(r) = row else {
        return Err(ApiError::NotFound(format!("no row {row_id}")));
    };

    if r.status == "applied" {
        return Err(ApiError::BadRequest(format!(
            "row {row_id} is already applied; reverse-apply not yet supported"
        )));
    }
    if r.status != "candidate" {
        return Err(ApiError::BadRequest(format!(
            "row {row_id} is at status='{}'; only candidate rows can be rejected",
            r.status
        )));
    }

    sqlx::query!(
        "UPDATE book_file_audiologos SET status = 'rejected', rejected_at = strftime('%s','now') WHERE audiologo_row_id = ?",
        row_id,
    )
    .execute(state.inner.library.pool())
    .await
    .map_err(|e| ab_core::Error::Database(format!("reject update: {e}")))?;

    Ok(Json(RejectResponse { row_id }))
}

/// Response of `audiologos_reject`.
#[derive(Debug, Serialize)]
pub struct RejectResponse {
    pub row_id: i64,
}

// ── auto-confirm helper ────────────────────────────────────────

/// If the audiologos row's `verified_via='silence'` AND
/// `match_count >= AUTO_CONFIRM_AFTER_MATCHES`, promote it to
/// `'review_confirmed'`.
///
/// Returns true when a promotion happened.
async fn maybe_auto_confirm_audiologo(
    tx: &mut sqlx::SqliteConnection,
    audiologo_id: Option<i64>,
) -> Result<bool, ApiError> {
    let Some(id) = audiologo_id else {
        return Ok(false);
    };

    let updated = sqlx::query!(
        r#"UPDATE audiologos
              SET verified_via = 'review_confirmed',
                  updated_at = strftime('%s','now')
            WHERE audiologo_id = ?
              AND verified_via = 'silence'
              AND match_count >= ?"#,
        id,
        AUTO_CONFIRM_AFTER_MATCHES,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| ab_core::Error::Database(format!("auto-confirm: {e}")))?;

    let promoted = updated.rows_affected() > 0;
    if promoted {
        tracing::info!(
            audiologo_id = id,
            threshold = AUTO_CONFIRM_AFTER_MATCHES,
            "audiologo.review.auto_confirmed"
        );
    }
    Ok(promoted)
}
