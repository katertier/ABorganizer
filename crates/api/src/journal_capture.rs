//! Helpers for wiring `operation_journal` capture into mutating
//! handlers (ADR-0039).
//!
//! Each capture-side endpoint follows the same three-step shape:
//!
//! 1. Read the pre-state from the target row (handler-owned —
//!    the SQL is endpoint-specific).
//! 2. `record_pending` a journal row before the mutation.
//! 3. After the mutation returns: `mark_done_or_log` on success,
//!    `mark_failed_or_log` on error.
//!
//! Steps 2 and 3 are mechanical; this module owns them so each
//! new mutating endpoint adds only the endpoint-specific bits
//! (`op_kind`, `pre_state` shape, mutation call).
//!
//! Both finalize helpers are **best-effort**: a DB error during
//! `mark_done` / `mark_failed` is logged at `warn` but never
//! overrides the underlying operation's result. A pending row
//! that survives a `mark_done` failure will be flipped to
//! `failed` by the next daemon-startup recovery sweep
//! (`recover_pending_with` per ADR-0039) — the safe-by-default
//! contract is preserved.

use ab_journal::NewEntry;
use serde_json::Value;
use sqlx::SqlitePool;

use crate::error::ApiError;

/// Record a `pending` journal row before the mutation.
///
/// Returns the new `op_id` for the caller to finalize via
/// [`mark_done_or_log`] / [`mark_failed_or_log`].
///
/// # Errors
///
/// Returns [`ApiError::Internal`] for SQL or JSON failures during
/// record. The mutation has NOT been attempted in this case —
/// the handler should propagate this error and abort.
pub async fn record_pending(pool: &SqlitePool, entry: &NewEntry<'_>) -> Result<i64, ApiError> {
    ab_journal::record(pool, entry)
        .await
        .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("journal record: {e}"))))
}

/// Flip a pending row to `done` with the supplied post-state.
///
/// On failure, log + return — the underlying mutation already
/// succeeded, so the handler must NOT surface this error to the
/// caller. The startup recovery sweep will flip the still-pending
/// row to `failed` later.
///
/// `context` lands as a structured field on the warn log so
/// per-endpoint failures are searchable.
pub async fn mark_done_or_log(
    pool: &SqlitePool,
    op_id: i64,
    post_state: &Value,
    context: &'static str,
) {
    if let Err(e) = ab_journal::mark_done(pool, op_id, post_state).await {
        tracing::warn!(
            op_id,
            context,
            error = %e,
            "journal_capture.mark_done_failed"
        );
    }
}

/// Flip a pending row to `failed` with the supplied `reason`.
///
/// On failure, log + return — the handler is still responsible
/// for returning the original mutation error.
pub async fn mark_failed_or_log(
    pool: &SqlitePool,
    op_id: i64,
    reason: &str,
    context: &'static str,
) {
    if let Err(e) = ab_journal::mark_failed(pool, op_id, reason).await {
        tracing::warn!(
            op_id,
            context,
            error = %e,
            "journal_capture.mark_failed_failed"
        );
    }
}
