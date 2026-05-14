//! User-edit provenance writes.
//!
//! ADR-0028 § "`Source::User`" specifies that `PATCH
//! /api/v1/books/{id}` (and any future user-facing edit endpoint)
//! INSERTs rows into `book_field_provenance` with `source =
//! 'user_edit'`, `stage = 'api-user-edit'`, `confidence = 1.0`,
//! `is_winner = 1`. The confidence floor means the consensus
//! stage's winner-picking heuristic naturally prefers user edits
//! over every AI-derived alternative, and the
//! `TagWriteFinalStage`'s per-field skip predicate
//! (`ab_tag_write::skip_for_final_pass`) keeps user corrections
//! sticky across the late tag-write pass.
//!
//! This module is the single-source-of-truth helper every
//! endpoint goes through so the convention can't be silently
//! typo'd.

use ab_core::Field;
use sqlx::{Sqlite, Transaction};

use crate::error::ApiError;

/// Canonical value of `book_field_provenance.source` for user
/// edits. Read-side consumers (`ab_tag_write::skip_for_final_pass`)
/// compare against the same string literal — keep these in sync.
pub(crate) const USER_EDIT_SOURCE: &str = "user_edit";

/// Canonical value of `book_field_provenance.stage` for user
/// edits. Free-form string distinct from any pipeline stage's
/// `STAGE_ID` (because `Stage::reset` filters on `stage = stage_id`
/// — user edits must survive every stage's reset path).
pub(crate) const USER_EDIT_STAGE: &str = "api-user-edit";

/// Confidence floor that puts user edits above every AI-derived
/// alternative in the consensus winner-pick. ADR-0028 § "User
/// edits" mandates 1.0.
pub(crate) const USER_EDIT_CONFIDENCE: f64 = 1.0;

/// Record one user edit on one field of one book inside an
/// already-open transaction.
///
/// Three steps, all inside the caller's transaction:
///
/// 1. Demote every prior `is_winner = 1` row for the same
///    `(book_id, field)` (the partial UNIQUE index from
///    migration 020 forbids two concurrent winners).
/// 2. Insert the user-edit row at `is_winner = 1`.
/// 3. Caller is responsible for the matching `UPDATE books SET <col> = ?`
///    (column derived from `field.books_column()`) when the field
///    promotes into a scalar column. Keeping the column-write
///    outside this helper means it can be skipped for join-driven
///    fields (`author`, `narrator`, `series`, `genre`) without
///    putting their identity-resolve plumbing inside an edits
///    helper.
///
/// # Errors
///
/// Returns [`ApiError::Internal`] wrapping any SQL error.
pub(crate) async fn record_user_edit(
    tx: &mut Transaction<'_, Sqlite>,
    book_id: i64,
    field: Field,
    value: Option<&str>,
) -> Result<(), ApiError> {
    let field_str = field.as_str();

    // 1. Demote prior winners.
    sqlx::query!(
        r#"UPDATE book_field_provenance
              SET is_winner = 0
            WHERE book_id = ? AND field = ? AND is_winner = 1"#,
        book_id,
        field_str,
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "record_user_edit demote: {e}"
        )))
    })?;

    // 2. Insert the new user-edit winner.
    sqlx::query!(
        r#"INSERT INTO book_field_provenance
              (book_id, field, value, source, stage, confidence, is_winner)
             VALUES (?, ?, ?, ?, ?, ?, 1)"#,
        book_id,
        field_str,
        value,
        USER_EDIT_SOURCE,
        USER_EDIT_STAGE,
        USER_EDIT_CONFIDENCE,
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "record_user_edit insert: {e}"
        )))
    })?;

    Ok(())
}
