//! Audiologo cut-application: shared between the HTTP endpoint
//! and the (future) `detect-audiologo` Stage in slice 4B
//! (`ab_audiologo`).
//!
//! Extracted from `router.rs` so integration tests can exercise
//! the chapter-shift maths directly without spinning up an axum
//! `Router` + `ApiState`. See ADR-0024 for the full design.
//!
//! # Public surface
//!
//! - [`apply_audiologo_cut`] — the full apply pipeline (insert
//!   row, shift chapters, recompute `books.duration_ms`, flip
//!   `books.audiologo_status`). Used by the HTTP endpoint with
//!   a transaction it owns.
//! - [`shift_chapters_for_cut`] — chapter-shift maths in
//!   isolation, plus the cumulative-offset accounting that
//!   subtracts previously-applied cuts. Public so tests can
//!   pin the bug-class.
//! - [`recompute_book_duration`] — subtracts a cut from
//!   `books.duration_ms` and flips `books.audiologo_status='applied'`.
//! - [`insert_audiologo_row`] — INSERTs into `book_file_audiologos`
//!   at `status='applied'`.
//!
//! All four return [`ApiError`] for consistency with the HTTP
//! layer; tests can match on `ApiError::Internal` when needed.

use crate::error::ApiError;
use sqlx::SqliteConnection;

/// Inputs to [`apply_audiologo_cut`].
pub struct ApplyCutParams<'a> {
    /// Book that owns the file.
    pub book_id: i64,
    /// Which file inside the book the cut applies to.
    pub file_id: i64,
    /// `"intro"` or `"outro"`.
    pub kind: &'a str,
    /// File-local offset where the jingle begins (ms).
    pub jingle_start_ms: i64,
    /// File-local offset where the jingle ends (ms).
    pub jingle_end_ms: i64,
    /// Optional padding override; NULL = use the tunable default.
    pub padding_ms: Option<i64>,
    /// String form of `ab_audiologo::Method` for the row.
    pub method: &'a str,
    /// Matched `audiologos.audiologo_id` when relevant.
    pub audiologo_id: Option<i64>,
    /// Per-row confidence (0.0..=1.0).
    pub confidence: f64,
}

/// Result of a successful [`apply_audiologo_cut`].
pub struct ApplyCutOutcome {
    /// The new `book_file_audiologos.audiologo_row_id`.
    pub row_id: i64,
    /// How many `chapters` rows were shifted by the cut.
    pub chapters_shifted: i64,
    /// `books.duration_ms` after the cut applied.
    pub new_duration_ms: Option<i64>,
}

/// Insert an `applied` row in `book_file_audiologos`, shift the
/// chapters attached to the affected book, recompute
/// `books.duration_ms`, and flip `books.audiologo_status`.
///
/// Encapsulates the full apply-pipeline for any Method —
/// `Manual` (4A), `CatalogBrandDuration` (4B), and the
/// fingerprint-bearing tiers (4C) all share this path.
///
/// Chapter shift rule per ADR-0024:
/// - `start_ms >= jingle_end_ms` → both `start_ms` and `end_ms`
///   shift earlier by `cut_ms`.
/// - chapter spans the trim → `end_ms` shifts only.
/// - chapter ends before the trim → unchanged.
///
/// `cut_ms = (jingle_end_ms - jingle_start_ms) - padding_ms`.
/// Boundary verification (`chapters.boundary_verified` flag) is
/// deferred to slice 4B (needs the transcript surface).
///
/// # Errors
///
/// Returns `ApiError::Internal(ab_core::Error::Database(...))`
/// on any SQL error; the caller (HTTP endpoint or Stage) wraps
/// or surfaces.
pub async fn apply_audiologo_cut(
    pool: &sqlx::SqlitePool,
    p: ApplyCutParams<'_>,
) -> Result<ApplyCutOutcome, ApiError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| ab_core::Error::Database(format!("audiologo begin tx: {e}")))?;

    let row_id = insert_audiologo_row(&mut tx, &p).await?;

    // cut_ms = (jingle_end - jingle_start) - padding.
    // Negative cut would mean padding > jingle range; clamp
    // to 0 (effectively no shift). NULL padding → 0.
    let padding = p.padding_ms.unwrap_or(0);
    let cut_ms: i64 = ((p.jingle_end_ms - p.jingle_start_ms) - padding).max(0);

    let chapters_shifted = shift_chapters_for_cut(
        &mut tx,
        ShiftArgs {
            book_id: p.book_id,
            file_id: p.file_id,
            jingle_start_ms: p.jingle_start_ms,
            jingle_end_ms: p.jingle_end_ms,
            cut_ms,
        },
    )
    .await?;

    let new_duration = recompute_book_duration(&mut tx, p.book_id, cut_ms).await?;

    tx.commit()
        .await
        .map_err(|e| ab_core::Error::Database(format!("audiologo commit: {e}")))?;

    Ok(ApplyCutOutcome {
        row_id,
        chapters_shifted,
        new_duration_ms: new_duration,
    })
}

/// Insert the `book_file_audiologos` row at `status='applied'`.
///
/// # Errors
///
/// Returns `ApiError::Internal(ab_core::Error::Database(...))`
/// on SQL error. The partial UNIQUE index on
/// `(file_id, kind) WHERE status = 'applied'` enforces "at most
/// one applied cut per pair"; conflict surfaces as a sqlx error.
/// HTTP handlers should pre-check and emit 409 (router does this).
pub async fn insert_audiologo_row(
    tx: &mut SqliteConnection,
    p: &ApplyCutParams<'_>,
) -> Result<i64, ApiError> {
    let insert = sqlx::query!(
        r#"INSERT INTO book_file_audiologos
           (file_id, kind, jingle_start_ms, jingle_end_ms,
            padding_ms, method, audiologo_id, confidence,
            status, applied_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'applied',
                   strftime('%s','now'))"#,
        p.file_id,
        p.kind,
        p.jingle_start_ms,
        p.jingle_end_ms,
        p.padding_ms,
        p.method,
        p.audiologo_id,
        p.confidence,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| ab_core::Error::Database(format!("audiologo insert: {e}")))?;
    Ok(insert.last_insert_rowid())
}

/// Args for [`shift_chapters_for_cut`]. Bundled so the
/// function stays under the workspace's 5-arg ceiling.
pub struct ShiftArgs {
    /// Book that owns the chapters.
    pub book_id: i64,
    /// File the cut applies to.
    pub file_id: i64,
    /// File-local offset where the jingle begins.
    pub jingle_start_ms: i64,
    /// File-local offset where the jingle ends.
    pub jingle_end_ms: i64,
    /// Pre-computed cut amount (`jingle_end - jingle_start -
    /// padding`, clamped at 0).
    pub cut_ms: i64,
}

/// Shift the `chapters` rows affected by a cut.
///
/// Per ADR-0024 the shift covers ALL `source` rows (not just
/// `is_winner=1`) so a later winner-switch reads already-shifted
/// data. The cut is file-local; chapters are book-cumulative
/// (per slice 2H), so we translate by:
///
/// 1. Summing the raw `book_files.duration_ms` of files
///    preceding this one in the book.
/// 2. Subtracting cuts already applied on those preceding
///    files. Without (2), a multi-file book with one cut
///    on file 0 followed by a cut on file 1 would compute
///    file 1's cumulative offset from raw durations — but
///    the chapter rows have already been shifted by file 0's
///    cut, so the comparison `chapter.start_ms >=
///    jingle_end_book_ms` would mismatch.
///
/// Returns the count of `chapters` rows touched.
///
/// # Errors
///
/// Returns `ApiError::Internal(ab_core::Error::Database(...))`
/// on any SQL error.
pub async fn shift_chapters_for_cut(
    tx: &mut SqliteConnection,
    args: ShiftArgs,
) -> Result<i64, ApiError> {
    let ShiftArgs {
        book_id,
        file_id,
        jingle_start_ms,
        jingle_end_ms,
        cut_ms,
    } = args;
    // Cumulative-offset accounting. See module / function doc
    // above for why the subtraction matters.
    //
    // `cut_amount` per applied row =
    //   max((jingle_end_ms - jingle_start_ms) - COALESCE(padding_ms, 0), 0)
    // matches the apply path's clamp.
    //
    // NULL `book_files.duration_ms` counts as 0 (best-effort;
    // the shift will be re-pass-able once the file gets a
    // duration). Empty preceding-file set also yields 0.
    let cumulative_before: i64 = sqlx::query_scalar!(
        r#"SELECT (
              COALESCE(
                  (SELECT SUM(duration_ms) FROM book_files
                   WHERE book_id = ? AND file_id < ?), 0)
              -
              COALESCE(
                  (SELECT SUM(
                       MAX((jingle_end_ms - jingle_start_ms)
                           - COALESCE(padding_ms, 0), 0))
                   FROM book_file_audiologos
                   WHERE status = 'applied'
                     AND file_id IN (
                       SELECT file_id FROM book_files
                       WHERE book_id = ? AND file_id < ?)
                  ), 0)
           ) AS "v!: i64""#,
        book_id,
        file_id,
        book_id,
        file_id,
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| ab_core::Error::Database(format!("audiologo cumulative offset: {e}")))?;
    let jingle_start_book_ms = cumulative_before + jingle_start_ms;
    let jingle_end_book_ms = cumulative_before + jingle_end_ms;

    // (a) chapter entirely after the trim → shift both
    //     start_ms and end_ms by cut_ms.
    let after = sqlx::query!(
        r#"UPDATE chapters
           SET start_ms = start_ms - ?,
               end_ms   = end_ms - ?
           WHERE book_id = ?
             AND start_ms >= ?"#,
        cut_ms,
        cut_ms,
        book_id,
        jingle_end_book_ms,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| ab_core::Error::Database(format!("audiologo shift chapters (after): {e}")))?;

    // (b) chapter spans the trim window (started before or at
    //     trim; ends after the trim's start) → shift end_ms only.
    let spanning = sqlx::query!(
        r#"UPDATE chapters
           SET end_ms = end_ms - ?
           WHERE book_id = ?
             AND start_ms < ?
             AND end_ms > ?"#,
        cut_ms,
        book_id,
        jingle_end_book_ms,
        jingle_start_book_ms,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| ab_core::Error::Database(format!("audiologo shift chapters (spanning): {e}")))?;

    // u64 → i64 cast: row counts are inside i64 by design.
    #[allow(clippy::cast_possible_wrap)]
    let shifted = (after.rows_affected() + spanning.rows_affected()) as i64;
    Ok(shifted)
}

/// Subtract a cut from `books.duration_ms` (when non-NULL) and
/// flip `books.audiologo_status='applied'`. Returns the
/// post-cut `duration_ms` for the caller.
///
/// # Errors
///
/// Returns `ApiError::Internal(ab_core::Error::Database(...))`
/// on any SQL error.
pub async fn recompute_book_duration(
    tx: &mut SqliteConnection,
    book_id: i64,
    cut_ms: i64,
) -> Result<Option<i64>, ApiError> {
    // duration_ms is post-trim; raw_duration_ms is pre-trim.
    // Subtract this cut's cut_ms from the current duration_ms
    // — handles multiple applied trims correctly (each
    // subtracts independently). For books that haven't had
    // duration_ms set yet, leave it alone (next pass sets it
    // from book_files cumulative).
    sqlx::query!(
        "UPDATE books \
         SET duration_ms = duration_ms - ?, \
             audiologo_status = 'applied' \
         WHERE book_id = ? AND duration_ms IS NOT NULL",
        cut_ms,
        book_id,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| ab_core::Error::Database(format!("audiologo recompute duration: {e}")))?;

    // Books with NULL duration_ms still get the status update.
    sqlx::query!(
        "UPDATE books SET audiologo_status = 'applied' \
         WHERE book_id = ? AND audiologo_status != 'applied'",
        book_id,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| ab_core::Error::Database(format!("audiologo status update: {e}")))?;

    sqlx::query_scalar!("SELECT duration_ms FROM books WHERE book_id = ?", book_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| ab_core::Error::Database(format!("audiologo read duration: {e}")).into())
}
