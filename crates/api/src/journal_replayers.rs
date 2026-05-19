//! Concrete [`ab_journal::Replayer`] implementations registered
//! in the daemon's `ReplayRegistry` (ADR-0039).
//!
//! Each `Replayer` claims a single `op_kind` and knows how to
//! re-execute that mutation idempotently. The handlers live in
//! `ab-api` rather than the data crates (`ab-progress`,
//! `ab-catalog`, etc.) because the journal capture half lives
//! here too — keeping both sides together avoids splitting the
//! `pre_state` contract across crate boundaries.
//!
//! Registry shape per [`ab_journal::ReplayRegistry::new`]:
//!
//! ```ignore
//! let registry = ab_journal::ReplayRegistry::new(vec![
//!     std::sync::Arc::new(ab_api::journal_replayers::StatusReplayer),
//!     // future replayers registered here as they ship
//! ]);
//! ```

use ab_core::{BookId, ReadingStatus};
use ab_journal::{JournalEntry, JournalError, ReplayDecision, Replayer};
use async_trait::async_trait;
use serde_json::{Value, json};
use sqlx::SqlitePool;

use crate::progress::{OP_KIND_BOOK_NOTES_SET, OP_KIND_BOOK_RATING_SET, OP_KIND_BOOK_STATUS_SET};

/// Re-applies `PATCH /books/{id}/status` mutations after a crash
/// or on operator-triggered retry.
///
/// Reads `pre_state.intent` to know the target status and
/// `pre_state.current` to detect drift:
///
/// - If the row's current `reading_status` equals `intent`, the
///   mutation already landed — returns [`ReplayDecision::Skipped`]
///   with a "already applied" reason. This is the idempotent
///   no-op case operators see when retrying a row that succeeded
///   silently before the crash.
/// - If the row's current `reading_status` equals `pre_state.current`
///   (no drift), call [`ab_progress::set_status`] to apply
///   `intent`. Return [`ReplayDecision::Retried`] with the new
///   `post_state`.
/// - Otherwise the world has drifted: someone else (operator,
///   another process) changed the status between the original
///   `PATCH` and this retry. Refuse — return
///   [`ReplayDecision::Skipped`] with a reason that names both
///   the drift-target and the recorded `pre_state` so the operator
///   can decide whether to re-issue.
///
/// Target row deletion is treated as drift (Skipped) rather than
/// auto-recreating — the journal entry has `reversible = true`
/// semantics meaning the operation can be undone, not that the
/// target must be resurrected.
pub struct StatusReplayer;

#[async_trait]
impl Replayer for StatusReplayer {
    fn op_kind(&self) -> &'static str {
        OP_KIND_BOOK_STATUS_SET
    }

    async fn try_replay(
        &self,
        pool: &SqlitePool,
        entry: &JournalEntry,
    ) -> Result<ReplayDecision, JournalError> {
        let book_id = entry.target.id;
        let pre = &entry.pre_state;
        let Some(intent_str) = pre.get("intent").and_then(|v| v.as_str()) else {
            return Ok(ReplayDecision::Skipped(
                "pre_state missing 'intent' field — cannot determine target status".to_owned(),
            ));
        };
        let Some(recorded_current) = pre.get("current").and_then(|v| v.as_str()) else {
            return Ok(ReplayDecision::Skipped(
                "pre_state missing 'current' field — cannot detect drift".to_owned(),
            ));
        };
        let Ok(intent) = parse_status(intent_str) else {
            return Ok(ReplayDecision::Skipped(format!(
                "pre_state.intent={intent_str:?} is not a valid ReadingStatus"
            )));
        };

        let current: Option<String> = sqlx::query_scalar!(
            r#"SELECT reading_status AS "reading_status!: String"
                 FROM books WHERE book_id = ?"#,
            book_id,
        )
        .fetch_optional(pool)
        .await?;

        let Some(current) = current else {
            return Ok(ReplayDecision::Skipped(format!(
                "book {book_id} no longer exists"
            )));
        };

        if current == intent_str {
            return Ok(ReplayDecision::Skipped(format!(
                "already applied — reading_status is already {intent_str:?}"
            )));
        }
        if current != recorded_current {
            return Ok(ReplayDecision::Skipped(format!(
                "drift detected — current reading_status is {current:?}, \
                 expected {recorded_current:?} (someone else changed it)"
            )));
        }

        ab_progress::set_status(pool, BookId(book_id), intent)
            .await
            .map_err(|e| match e {
                ab_progress::ProgressError::NotFound(_) => JournalError::NotFound(book_id),
                ab_progress::ProgressError::Core(core) => {
                    JournalError::Db(sqlx::Error::Protocol(format!("set_status replay: {core}")))
                }
            })?;

        Ok(ReplayDecision::Retried(
            json!({ "reading_status": intent_str }),
        ))
    }
}

fn parse_status(s: &str) -> Result<ReadingStatus, ()> {
    match s {
        "want_to_read" => Ok(ReadingStatus::WantToRead),
        "reading" => Ok(ReadingStatus::Reading),
        "finished" => Ok(ReadingStatus::Finished),
        "dnf" => Ok(ReadingStatus::Dnf),
        _ => Err(()),
    }
}

/// Re-applies `PATCH /books/{id}/rating` mutations after a crash
/// or on operator-triggered retry.
///
/// Twin of [`StatusReplayer`] — same drift-detection /
/// already-applied / book-deleted / malformed-`pre_state` decision
/// tree, applied to JSON-number or `null` rating values
/// (`null` = "no rating").
///
/// `pre_state.intent` is `null` OR an integer 1..=5; anything
/// else is rejected as malformed (the [`crate::progress::books_rating_patch`]
/// capture rejects out-of-range values BEFORE recording, so the
/// only way an out-of-range row enters the journal is via direct
/// DB tampering — we still defend against it).
pub struct RatingReplayer;

#[async_trait]
impl Replayer for RatingReplayer {
    fn op_kind(&self) -> &'static str {
        OP_KIND_BOOK_RATING_SET
    }

    async fn try_replay(
        &self,
        pool: &SqlitePool,
        entry: &JournalEntry,
    ) -> Result<ReplayDecision, JournalError> {
        let book_id = entry.target.id;
        let pre = &entry.pre_state;
        let Some(intent_value) = pre.get("intent") else {
            return Ok(ReplayDecision::Skipped(
                "pre_state missing 'intent' field".to_owned(),
            ));
        };
        let Some(current_value) = pre.get("current") else {
            return Ok(ReplayDecision::Skipped(
                "pre_state missing 'current' field".to_owned(),
            ));
        };
        let intent = match parse_rating(intent_value) {
            ParsedRating::Malformed => {
                return Ok(ReplayDecision::Skipped(format!(
                    "pre_state.intent={intent_value} is not a valid rating (null or 1..=5)"
                )));
            }
            ParsedRating::Cleared => None,
            ParsedRating::Rated(n) => Some(n),
        };
        let recorded_current = match parse_rating(current_value) {
            ParsedRating::Malformed => {
                return Ok(ReplayDecision::Skipped(format!(
                    "pre_state.current={current_value} is not a valid rating"
                )));
            }
            ParsedRating::Cleared => None,
            ParsedRating::Rated(n) => Some(n),
        };

        let current: Option<Option<i64>> = sqlx::query_scalar!(
            r#"SELECT rating AS "rating: i64" FROM books WHERE book_id = ?"#,
            book_id,
        )
        .fetch_optional(pool)
        .await?;
        let Some(current) = current else {
            return Ok(ReplayDecision::Skipped(format!(
                "book {book_id} no longer exists"
            )));
        };

        if current == intent {
            return Ok(ReplayDecision::Skipped(format!(
                "already applied — rating is already {intent_value}"
            )));
        }
        if current != recorded_current {
            return Ok(ReplayDecision::Skipped(format!(
                "drift detected — current rating is {current:?}, \
                 expected {recorded_current:?} (someone else changed it)"
            )));
        }

        let intent_u8 = intent.map(|i| {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "intent was validated as 1..=5 by parse_rating"
            )]
            {
                i as u8
            }
        });
        ab_progress::set_rating(pool, BookId(book_id), intent_u8)
            .await
            .map_err(|e| match e {
                ab_progress::ProgressError::NotFound(_) => JournalError::NotFound(book_id),
                ab_progress::ProgressError::Core(core) => {
                    JournalError::Db(sqlx::Error::Protocol(format!("set_rating replay: {core}")))
                }
            })?;

        Ok(ReplayDecision::Retried(json!({ "rating": intent_value })))
    }
}

/// Tri-state outcome of parsing a JSON rating value.
///
/// `Cleared` matches `null` (no rating); `Rated(1..=5)` matches a
/// valid integer rating; `Malformed` matches anything else (the
/// Replayer turns this into a `Skipped` decision).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParsedRating {
    Cleared,
    Rated(i64),
    Malformed,
}

fn parse_rating(v: &Value) -> ParsedRating {
    if v.is_null() {
        return ParsedRating::Cleared;
    }
    match v.as_i64() {
        Some(n) if (1..=5).contains(&n) => ParsedRating::Rated(n),
        _ => ParsedRating::Malformed,
    }
}

/// Re-applies `PATCH /books/{id}/notes` mutations after a crash
/// or on operator-triggered retry.
///
/// Twin of [`StatusReplayer`] / [`RatingReplayer`] — same
/// drift-detection / already-applied / book-deleted /
/// malformed-`pre_state` decision tree, applied to string-or-null
/// notes values.
///
/// `pre_state.intent` is `null` OR a non-empty string (the
/// [`crate::progress::books_notes_patch`] capture normalises
/// whitespace-only input to `null` BEFORE recording, so the
/// `pre_state` always matches what was persisted). The replayer
/// compares against the DB's current notes column directly.
pub struct NotesReplayer;

#[async_trait]
impl Replayer for NotesReplayer {
    fn op_kind(&self) -> &'static str {
        OP_KIND_BOOK_NOTES_SET
    }

    async fn try_replay(
        &self,
        pool: &SqlitePool,
        entry: &JournalEntry,
    ) -> Result<ReplayDecision, JournalError> {
        let book_id = entry.target.id;
        let pre = &entry.pre_state;
        let Some(intent_value) = pre.get("intent") else {
            return Ok(ReplayDecision::Skipped(
                "pre_state missing 'intent' field".to_owned(),
            ));
        };
        let Some(current_value) = pre.get("current") else {
            return Ok(ReplayDecision::Skipped(
                "pre_state missing 'current' field".to_owned(),
            ));
        };
        let intent = match parse_notes(intent_value) {
            ParsedNotes::Malformed => {
                return Ok(ReplayDecision::Skipped(format!(
                    "pre_state.intent={intent_value} is not a valid notes value (null or string)"
                )));
            }
            ParsedNotes::Cleared => None,
            ParsedNotes::Set(s) => Some(s),
        };
        let recorded_current = match parse_notes(current_value) {
            ParsedNotes::Malformed => {
                return Ok(ReplayDecision::Skipped(format!(
                    "pre_state.current={current_value} is not a valid notes value"
                )));
            }
            ParsedNotes::Cleared => None,
            ParsedNotes::Set(s) => Some(s),
        };

        let current: Option<Option<String>> = sqlx::query_scalar!(
            r#"SELECT notes AS "notes: String" FROM books WHERE book_id = ?"#,
            book_id,
        )
        .fetch_optional(pool)
        .await?;
        let Some(current) = current else {
            return Ok(ReplayDecision::Skipped(format!(
                "book {book_id} no longer exists"
            )));
        };

        if current == intent {
            return Ok(ReplayDecision::Skipped(format!(
                "already applied — notes is already {intent_value}"
            )));
        }
        if current != recorded_current {
            return Ok(ReplayDecision::Skipped(format!(
                "drift detected — current notes is {current:?}, \
                 expected {recorded_current:?} (someone else changed it)"
            )));
        }

        ab_progress::set_notes(pool, BookId(book_id), intent.as_deref())
            .await
            .map_err(|e| match e {
                ab_progress::ProgressError::NotFound(_) => JournalError::NotFound(book_id),
                ab_progress::ProgressError::Core(core) => {
                    JournalError::Db(sqlx::Error::Protocol(format!("set_notes replay: {core}")))
                }
            })?;

        Ok(ReplayDecision::Retried(json!({ "notes": intent_value })))
    }
}

/// Tri-state outcome of parsing a JSON notes value.
///
/// `Cleared` matches `null`; `Set(string)` matches a (possibly
/// empty) JSON string — the capture handler normalises so we
/// won't normally see whitespace-only or empty strings in
/// `pre_state.intent`, but we accept them rather than rejecting
/// (treating `""` as `Set("")` lets a manually-edited journal
/// row replay cleanly if the operator wants to set an empty
/// string). `Malformed` matches anything else.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ParsedNotes {
    Cleared,
    Set(String),
    Malformed,
}

fn parse_notes(v: &Value) -> ParsedNotes {
    if v.is_null() {
        return ParsedNotes::Cleared;
    }
    v.as_str()
        .map_or(ParsedNotes::Malformed, |s| ParsedNotes::Set(s.to_owned()))
}

/// Re-applies `PATCH /books/{id}` title-set mutations after a
/// crash or on operator-triggered retry.
///
/// Twin of [`StatusReplayer`] — same drift-detection /
/// already-applied / book-deleted / malformed-`pre_state` decision
/// tree, applied to a `NOT NULL` string column. Differs from the
/// status/rating/notes trio because the capture-side handler
/// ([`crate::router::OP_KIND_BOOK_TITLE_SET`]) batches title with
/// up to eight other PATCH fields in a single transaction; the
/// replay path is title-only because the journal row is
/// title-only.
///
/// Re-apply runs the same shape as the original PATCH: a
/// transaction containing `record_user_edit` on `Field::Title`
/// (restores the `confidence = 1.0`, `is_winner = 1` provenance
/// row that the rolled-back tx lost) + `UPDATE books.title`.
pub struct TitleReplayer;

#[async_trait]
impl Replayer for TitleReplayer {
    fn op_kind(&self) -> &'static str {
        crate::router::OP_KIND_BOOK_TITLE_SET
    }

    async fn try_replay(
        &self,
        pool: &SqlitePool,
        entry: &JournalEntry,
    ) -> Result<ReplayDecision, JournalError> {
        let book_id = entry.target.id;
        let pre = &entry.pre_state;
        let Some(intent) = pre.get("intent").and_then(Value::as_str) else {
            return Ok(ReplayDecision::Skipped(
                "pre_state missing 'intent' field (or not a string)".to_owned(),
            ));
        };
        let Some(recorded_current) = pre.get("current").and_then(Value::as_str) else {
            return Ok(ReplayDecision::Skipped(
                "pre_state missing 'current' field (or not a string)".to_owned(),
            ));
        };

        let current: Option<String> = sqlx::query_scalar!(
            r#"SELECT title AS "title!: String" FROM books WHERE book_id = ?"#,
            book_id,
        )
        .fetch_optional(pool)
        .await?;

        let Some(current) = current else {
            return Ok(ReplayDecision::Skipped(format!(
                "book {book_id} no longer exists"
            )));
        };

        if current == intent {
            return Ok(ReplayDecision::Skipped(format!(
                "already applied — title is already {intent:?}"
            )));
        }
        if current != recorded_current {
            return Ok(ReplayDecision::Skipped(format!(
                "drift detected — current title is {current:?}, \
                 expected {recorded_current:?} (someone else changed it)"
            )));
        }

        let mut tx = pool.begin().await?;
        crate::user_edits::record_user_edit(&mut tx, book_id, ab_core::Field::Title, Some(intent))
            .await
            .map_err(|e| {
                JournalError::Db(sqlx::Error::Protocol(format!(
                    "title replay user_edit: {e}"
                )))
            })?;
        sqlx::query!(
            "UPDATE books SET title = ? WHERE book_id = ?",
            intent,
            book_id,
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        Ok(ReplayDecision::Retried(json!({ "title": intent })))
    }
}

/// Re-applies `POST /collections` mutations after a crash or on
/// operator-triggered retry.
///
/// The capture path runs `INSERT INTO book_collections` and *then*
/// records the journal row, so a pending row only survives in two
/// cases:
///
/// 1. The insert succeeded but `record_pending` / `mark_done`
///    failed. The row exists in `book_collections` at the
///    `target.id` recorded on the journal entry — recovery should
///    flip the journal row to `Retried` (already applied) with no
///    extra DB writes.
/// 2. (Defensive) The captured row no longer exists at `target.id`
///    AND no row exists with the same `name`. This happens when
///    the operator hard-deleted the collection between the
///    original `POST` and the replay. Re-insert using
///    `pre_state.intent.*` so the journal-driven undo→redo cycle
///    is symmetric with the [`TitleReplayer`] shape.
///
/// Name collisions on replay (different `collection_id`, same
/// `name`) are treated as drift and `Skipped` — re-inserting
/// would either trip the UNIQUE constraint or silently rebind the
/// original journal target to a row the operator created
/// independently.
pub struct CollectionCreateReplayer;

#[async_trait]
impl Replayer for CollectionCreateReplayer {
    fn op_kind(&self) -> &'static str {
        crate::collections::OP_KIND_COLLECTION_CREATE
    }

    async fn try_replay(
        &self,
        pool: &SqlitePool,
        entry: &JournalEntry,
    ) -> Result<ReplayDecision, JournalError> {
        let captured_id = entry.target.id;
        let Some(intent) = entry.pre_state.get("intent") else {
            return Ok(ReplayDecision::Skipped(
                "pre_state missing 'intent' object".to_owned(),
            ));
        };
        let Some(name) = intent.get("name").and_then(Value::as_str) else {
            return Ok(ReplayDecision::Skipped(
                "pre_state.intent missing 'name' (or not a string)".to_owned(),
            ));
        };

        // Case 1: row at captured id still exists → already-applied
        // no-op. Validate the name still matches; mismatch is drift.
        let row = sqlx::query!(
            r#"SELECT name AS "name!: String" FROM book_collections WHERE collection_id = ?"#,
            captured_id,
        )
        .fetch_optional(pool)
        .await?;
        if let Some(r) = row {
            if r.name == name {
                return Ok(ReplayDecision::Retried(json!({
                    "collection_id": captured_id,
                    "name": name,
                    "already_applied": true,
                })));
            }
            return Ok(ReplayDecision::Skipped(format!(
                "drift detected — collection {captured_id} renamed from {name:?} to {:?}",
                r.name
            )));
        }

        // Case 2: captured row gone. Bail out if a *different* row
        // already claims this name (operator created an independent
        // collection during the recovery window — re-insert would
        // hit UNIQUE).
        let name_taken = sqlx::query_scalar!(
            r#"SELECT collection_id AS "id!: i64" FROM book_collections WHERE name = ?"#,
            name,
        )
        .fetch_optional(pool)
        .await?;
        if let Some(other_id) = name_taken {
            return Ok(ReplayDecision::Skipped(format!(
                "drift detected — name {name:?} now belongs to collection {other_id}"
            )));
        }

        // Case 2 continued: re-insert using captured intent.
        let canonical_name = intent
            .get("canonical_name")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let audible_id = intent
            .get("audible_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let description = intent
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let kind = intent
            .get("kind")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let new_id: i64 = sqlx::query_scalar!(
            r#"INSERT INTO book_collections
                 (name, canonical_name, audible_id, description, kind)
               VALUES (?, ?, ?, ?, ?)
               RETURNING collection_id AS "collection_id!: i64""#,
            name,
            canonical_name,
            audible_id,
            description,
            kind,
        )
        .fetch_one(pool)
        .await?;
        Ok(ReplayDecision::Retried(json!({
            "collection_id": new_id,
            "name": name,
            "recreated_from_id": captured_id,
        })))
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    use ab_core::tunables::DbTunables;
    use ab_db::LibraryDb;
    use serde_json::Value;
    use tempfile::TempDir;

    async fn open_db() -> (TempDir, LibraryDb) {
        let tmp = TempDir::new().expect("tempdir");
        let db = LibraryDb::open(&tmp.path().join("library.db"), &DbTunables::default())
            .await
            .expect("open library");
        (tmp, db)
    }

    async fn seed_book(db: &LibraryDb, status: &str) -> i64 {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO books (title, duration_ms, raw_duration_ms, reading_status) \
             VALUES ('T', 60000, 60000, ?) RETURNING book_id",
        )
        .bind(status)
        .fetch_one(db.pool())
        .await
        .expect("insert book");
        id
    }

    fn entry(op_id: i64, book_id: i64, pre: Value) -> JournalEntry {
        JournalEntry {
            op_id,
            op_kind: OP_KIND_BOOK_STATUS_SET.to_owned(),
            target: ab_journal::Target {
                kind: "book".to_owned(),
                id: book_id,
            },
            pre_state: pre,
            post_state: None,
            created_at: 0,
            reversible: true,
            batch_id: None,
            progress: ab_journal::Progress::Pending,
            failed_reason: None,
        }
    }

    #[tokio::test]
    async fn op_kind_matches_capture_constant() {
        assert_eq!(StatusReplayer.op_kind(), "book-status-set");
        assert_eq!(StatusReplayer.op_kind(), OP_KIND_BOOK_STATUS_SET);
    }

    #[tokio::test]
    async fn retries_when_no_drift() {
        let (_d, db) = open_db().await;
        let book_id = seed_book(&db, "want_to_read").await;
        let e = entry(
            1,
            book_id,
            json!({ "current": "want_to_read", "intent": "reading" }),
        );
        let decision = StatusReplayer
            .try_replay(db.pool(), &e)
            .await
            .expect("try_replay");
        match decision {
            ReplayDecision::Retried(post) => {
                assert_eq!(post["reading_status"], "reading");
            }
            ReplayDecision::Skipped(reason) => {
                panic!("expected Retried, got Skipped({reason})")
            }
        }
        // DB state matches.
        let st: String = sqlx::query_scalar("SELECT reading_status FROM books WHERE book_id = ?")
            .bind(book_id)
            .fetch_one(db.pool())
            .await
            .expect("read");
        assert_eq!(st, "reading");
    }

    #[tokio::test]
    async fn skips_when_already_applied() {
        let (_d, db) = open_db().await;
        // Book is already at the intended status.
        let book_id = seed_book(&db, "reading").await;
        let e = entry(
            1,
            book_id,
            json!({ "current": "want_to_read", "intent": "reading" }),
        );
        let decision = StatusReplayer
            .try_replay(db.pool(), &e)
            .await
            .expect("try_replay");
        match decision {
            ReplayDecision::Skipped(reason) => {
                assert!(reason.contains("already applied"), "reason: {reason}");
            }
            ReplayDecision::Retried(_) => panic!("expected Skipped, got Retried"),
        }
    }

    #[tokio::test]
    async fn skips_when_book_deleted() {
        let (_d, db) = open_db().await;
        let e = entry(
            1,
            9999,
            json!({ "current": "want_to_read", "intent": "reading" }),
        );
        let decision = StatusReplayer
            .try_replay(db.pool(), &e)
            .await
            .expect("try_replay");
        match decision {
            ReplayDecision::Skipped(reason) => {
                assert!(reason.contains("no longer exists"), "reason: {reason}");
            }
            ReplayDecision::Retried(_) => panic!("expected Skipped, got Retried"),
        }
    }

    #[tokio::test]
    async fn skips_on_drift() {
        let (_d, db) = open_db().await;
        // Book has drifted to a status that's neither current nor intent.
        let book_id = seed_book(&db, "dnf").await;
        let e = entry(
            1,
            book_id,
            json!({ "current": "want_to_read", "intent": "reading" }),
        );
        let decision = StatusReplayer
            .try_replay(db.pool(), &e)
            .await
            .expect("try_replay");
        match decision {
            ReplayDecision::Skipped(reason) => {
                assert!(reason.contains("drift"), "reason: {reason}");
                assert!(
                    reason.contains("dnf"),
                    "reason should mention current: {reason}"
                );
                assert!(
                    reason.contains("want_to_read"),
                    "reason should mention recorded pre-state: {reason}"
                );
            }
            ReplayDecision::Retried(_) => panic!("expected Skipped, got Retried"),
        }
        // DB state unchanged.
        let st: String = sqlx::query_scalar("SELECT reading_status FROM books WHERE book_id = ?")
            .bind(book_id)
            .fetch_one(db.pool())
            .await
            .expect("read");
        assert_eq!(st, "dnf");
    }

    #[tokio::test]
    async fn skips_on_malformed_pre_state() {
        let (_d, db) = open_db().await;
        let book_id = seed_book(&db, "want_to_read").await;
        // Missing intent.
        let e = entry(1, book_id, json!({ "current": "want_to_read" }));
        let decision = StatusReplayer
            .try_replay(db.pool(), &e)
            .await
            .expect("try_replay");
        assert!(matches!(decision, ReplayDecision::Skipped(ref s) if s.contains("intent")));

        // Bogus intent.
        let e = entry(
            1,
            book_id,
            json!({ "current": "want_to_read", "intent": "bogus" }),
        );
        let decision = StatusReplayer
            .try_replay(db.pool(), &e)
            .await
            .expect("try_replay");
        assert!(matches!(decision, ReplayDecision::Skipped(ref s) if s.contains("bogus")));
    }

    // ── RatingReplayer tests ──────────────────────────────────────────

    async fn seed_book_with_rating(db: &LibraryDb, rating: Option<i64>) -> i64 {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO books (title, duration_ms, raw_duration_ms, rating) \
             VALUES ('T', 60000, 60000, ?) RETURNING book_id",
        )
        .bind(rating)
        .fetch_one(db.pool())
        .await
        .expect("insert book");
        id
    }

    fn rating_entry(op_id: i64, book_id: i64, pre: Value) -> JournalEntry {
        JournalEntry {
            op_id,
            op_kind: OP_KIND_BOOK_RATING_SET.to_owned(),
            target: ab_journal::Target {
                kind: "book".to_owned(),
                id: book_id,
            },
            pre_state: pre,
            post_state: None,
            created_at: 0,
            reversible: true,
            batch_id: None,
            progress: ab_journal::Progress::Pending,
            failed_reason: None,
        }
    }

    #[tokio::test]
    async fn rating_op_kind_matches_capture_constant() {
        assert_eq!(RatingReplayer.op_kind(), "book-rating-set");
        assert_eq!(RatingReplayer.op_kind(), OP_KIND_BOOK_RATING_SET);
    }

    #[tokio::test]
    async fn rating_retries_when_no_drift_setting_value() {
        let (_d, db) = open_db().await;
        let book_id = seed_book_with_rating(&db, None).await;
        let e = rating_entry(1, book_id, json!({ "current": null, "intent": 4 }));
        let decision = RatingReplayer
            .try_replay(db.pool(), &e)
            .await
            .expect("try_replay");
        match decision {
            ReplayDecision::Retried(post) => assert_eq!(post["rating"], 4),
            ReplayDecision::Skipped(r) => panic!("expected Retried, got Skipped({r})"),
        }
        let st: Option<i64> = sqlx::query_scalar("SELECT rating FROM books WHERE book_id = ?")
            .bind(book_id)
            .fetch_one(db.pool())
            .await
            .expect("read");
        assert_eq!(st, Some(4));
    }

    #[tokio::test]
    async fn rating_retries_when_clearing_to_null() {
        let (_d, db) = open_db().await;
        let book_id = seed_book_with_rating(&db, Some(3)).await;
        let e = rating_entry(1, book_id, json!({ "current": 3, "intent": null }));
        let decision = RatingReplayer
            .try_replay(db.pool(), &e)
            .await
            .expect("try_replay");
        assert!(matches!(decision, ReplayDecision::Retried(_)));
        let st: Option<i64> = sqlx::query_scalar("SELECT rating FROM books WHERE book_id = ?")
            .bind(book_id)
            .fetch_one(db.pool())
            .await
            .expect("read");
        assert_eq!(st, None);
    }

    #[tokio::test]
    async fn rating_skips_when_already_applied() {
        let (_d, db) = open_db().await;
        let book_id = seed_book_with_rating(&db, Some(4)).await;
        let e = rating_entry(1, book_id, json!({ "current": null, "intent": 4 }));
        let decision = RatingReplayer
            .try_replay(db.pool(), &e)
            .await
            .expect("try_replay");
        match decision {
            ReplayDecision::Skipped(r) => assert!(r.contains("already applied"), "reason: {r}"),
            ReplayDecision::Retried(_) => panic!("expected Skipped"),
        }
    }

    #[tokio::test]
    async fn rating_skips_on_drift() {
        let (_d, db) = open_db().await;
        let book_id = seed_book_with_rating(&db, Some(2)).await;
        let e = rating_entry(1, book_id, json!({ "current": null, "intent": 4 }));
        let decision = RatingReplayer
            .try_replay(db.pool(), &e)
            .await
            .expect("try_replay");
        match decision {
            ReplayDecision::Skipped(r) => assert!(r.contains("drift"), "reason: {r}"),
            ReplayDecision::Retried(_) => panic!("expected Skipped"),
        }
    }

    #[tokio::test]
    async fn rating_skips_on_out_of_range_intent() {
        let (_d, db) = open_db().await;
        let book_id = seed_book_with_rating(&db, None).await;
        let e = rating_entry(1, book_id, json!({ "current": null, "intent": 9 }));
        let decision = RatingReplayer
            .try_replay(db.pool(), &e)
            .await
            .expect("try_replay");
        match decision {
            ReplayDecision::Skipped(r) => {
                assert!(r.contains("not a valid rating"), "reason: {r}");
            }
            ReplayDecision::Retried(_) => panic!("expected Skipped for malformed intent"),
        }
    }

    // ── NotesReplayer tests ──────────────────────────────────────────

    async fn seed_book_with_notes(db: &LibraryDb, notes: Option<&str>) -> i64 {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO books (title, duration_ms, raw_duration_ms, notes) \
             VALUES ('T', 60000, 60000, ?) RETURNING book_id",
        )
        .bind(notes)
        .fetch_one(db.pool())
        .await
        .expect("insert book");
        id
    }

    fn notes_entry(op_id: i64, book_id: i64, pre: Value) -> JournalEntry {
        JournalEntry {
            op_id,
            op_kind: OP_KIND_BOOK_NOTES_SET.to_owned(),
            target: ab_journal::Target {
                kind: "book".to_owned(),
                id: book_id,
            },
            pre_state: pre,
            post_state: None,
            created_at: 0,
            reversible: true,
            batch_id: None,
            progress: ab_journal::Progress::Pending,
            failed_reason: None,
        }
    }

    #[tokio::test]
    async fn notes_op_kind_matches_capture_constant() {
        assert_eq!(NotesReplayer.op_kind(), "book-notes-set");
        assert_eq!(NotesReplayer.op_kind(), OP_KIND_BOOK_NOTES_SET);
    }

    #[tokio::test]
    async fn notes_retries_when_no_drift_setting_value() {
        let (_d, db) = open_db().await;
        let book_id = seed_book_with_notes(&db, None).await;
        let e = notes_entry(1, book_id, json!({ "current": null, "intent": "loved it" }));
        let decision = NotesReplayer
            .try_replay(db.pool(), &e)
            .await
            .expect("try_replay");
        match decision {
            ReplayDecision::Retried(post) => assert_eq!(post["notes"], "loved it"),
            ReplayDecision::Skipped(r) => panic!("expected Retried, got Skipped({r})"),
        }
        let st: Option<String> = sqlx::query_scalar("SELECT notes FROM books WHERE book_id = ?")
            .bind(book_id)
            .fetch_one(db.pool())
            .await
            .expect("read");
        assert_eq!(st, Some("loved it".to_owned()));
    }

    #[tokio::test]
    async fn notes_retries_when_clearing_to_null() {
        let (_d, db) = open_db().await;
        let book_id = seed_book_with_notes(&db, Some("draft")).await;
        let e = notes_entry(1, book_id, json!({ "current": "draft", "intent": null }));
        let decision = NotesReplayer
            .try_replay(db.pool(), &e)
            .await
            .expect("try_replay");
        assert!(matches!(decision, ReplayDecision::Retried(_)));
        let st: Option<String> = sqlx::query_scalar("SELECT notes FROM books WHERE book_id = ?")
            .bind(book_id)
            .fetch_one(db.pool())
            .await
            .expect("read");
        assert_eq!(st, None);
    }

    #[tokio::test]
    async fn notes_skips_when_already_applied() {
        let (_d, db) = open_db().await;
        let book_id = seed_book_with_notes(&db, Some("loved it")).await;
        let e = notes_entry(1, book_id, json!({ "current": null, "intent": "loved it" }));
        let decision = NotesReplayer
            .try_replay(db.pool(), &e)
            .await
            .expect("try_replay");
        match decision {
            ReplayDecision::Skipped(r) => assert!(r.contains("already applied"), "reason: {r}"),
            ReplayDecision::Retried(_) => panic!("expected Skipped"),
        }
    }

    #[tokio::test]
    async fn notes_skips_on_drift() {
        let (_d, db) = open_db().await;
        let book_id = seed_book_with_notes(&db, Some("operator-edit")).await;
        let e = notes_entry(1, book_id, json!({ "current": null, "intent": "loved it" }));
        let decision = NotesReplayer
            .try_replay(db.pool(), &e)
            .await
            .expect("try_replay");
        match decision {
            ReplayDecision::Skipped(r) => assert!(r.contains("drift"), "reason: {r}"),
            ReplayDecision::Retried(_) => panic!("expected Skipped"),
        }
    }

    #[tokio::test]
    async fn notes_skips_on_malformed_intent_type() {
        let (_d, db) = open_db().await;
        let book_id = seed_book_with_notes(&db, None).await;
        // intent must be string-or-null; integer is malformed.
        let e = notes_entry(1, book_id, json!({ "current": null, "intent": 42 }));
        let decision = NotesReplayer
            .try_replay(db.pool(), &e)
            .await
            .expect("try_replay");
        match decision {
            ReplayDecision::Skipped(r) => {
                assert!(r.contains("not a valid notes value"), "reason: {r}");
            }
            ReplayDecision::Retried(_) => panic!("expected Skipped for malformed intent"),
        }
    }

    // ── TitleReplayer tests ──────────────────────────────────────────

    async fn seed_book_with_title(db: &LibraryDb, title: &str) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO books (title, duration_ms, raw_duration_ms, reading_status) \
             VALUES (?, 60000, 60000, 'want_to_read') RETURNING book_id",
        )
        .bind(title)
        .fetch_one(db.pool())
        .await
        .expect("insert book")
    }

    fn title_entry(op_id: i64, book_id: i64, pre: Value) -> JournalEntry {
        JournalEntry {
            op_id,
            op_kind: crate::router::OP_KIND_BOOK_TITLE_SET.to_owned(),
            target: ab_journal::Target {
                kind: "book".to_owned(),
                id: book_id,
            },
            pre_state: pre,
            post_state: None,
            created_at: 0,
            reversible: true,
            batch_id: None,
            progress: ab_journal::Progress::Pending,
            failed_reason: None,
        }
    }

    #[tokio::test]
    async fn title_op_kind_matches_capture_constant() {
        assert_eq!(TitleReplayer.op_kind(), "book-title-set");
        assert_eq!(
            TitleReplayer.op_kind(),
            crate::router::OP_KIND_BOOK_TITLE_SET
        );
    }

    #[tokio::test]
    async fn title_retries_when_no_drift() {
        let (_d, db) = open_db().await;
        let book_id = seed_book_with_title(&db, "Old Title").await;
        let e = title_entry(
            1,
            book_id,
            json!({ "current": "Old Title", "intent": "New Title" }),
        );
        let decision = TitleReplayer
            .try_replay(db.pool(), &e)
            .await
            .expect("try_replay");
        match decision {
            ReplayDecision::Retried(post) => {
                assert_eq!(post["title"], "New Title");
            }
            ReplayDecision::Skipped(r) => panic!("expected Retried, got Skipped({r})"),
        }
        let t: String = sqlx::query_scalar("SELECT title FROM books WHERE book_id = ?")
            .bind(book_id)
            .fetch_one(db.pool())
            .await
            .expect("read");
        assert_eq!(t, "New Title");

        // Provenance row landed too — confidence-1.0 user_edit winner.
        let prov_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM book_field_provenance \
             WHERE book_id = ? AND field = 'title' AND source = 'user_edit' \
               AND is_winner = 1 AND confidence = 1.0",
        )
        .bind(book_id)
        .fetch_one(db.pool())
        .await
        .expect("count provenance");
        assert_eq!(prov_count, 1);
    }

    #[tokio::test]
    async fn title_skips_when_already_applied() {
        let (_d, db) = open_db().await;
        let book_id = seed_book_with_title(&db, "Already New").await;
        let e = title_entry(
            1,
            book_id,
            json!({ "current": "Old Title", "intent": "Already New" }),
        );
        let decision = TitleReplayer
            .try_replay(db.pool(), &e)
            .await
            .expect("try_replay");
        match decision {
            ReplayDecision::Skipped(r) => {
                assert!(r.contains("already applied"), "reason: {r}");
            }
            ReplayDecision::Retried(_) => panic!("expected Skipped"),
        }
    }

    #[tokio::test]
    async fn title_skips_on_drift() {
        let (_d, db) = open_db().await;
        let book_id = seed_book_with_title(&db, "Drifted Title").await;
        let e = title_entry(
            1,
            book_id,
            json!({ "current": "Old Title", "intent": "New Title" }),
        );
        let decision = TitleReplayer
            .try_replay(db.pool(), &e)
            .await
            .expect("try_replay");
        match decision {
            ReplayDecision::Skipped(r) => {
                assert!(r.contains("drift detected"), "reason: {r}");
            }
            ReplayDecision::Retried(_) => panic!("expected Skipped"),
        }
    }

    #[tokio::test]
    async fn title_skips_when_book_deleted() {
        let (_d, db) = open_db().await;
        let e = title_entry(
            1,
            9999,
            json!({ "current": "Old Title", "intent": "New Title" }),
        );
        let decision = TitleReplayer
            .try_replay(db.pool(), &e)
            .await
            .expect("try_replay");
        match decision {
            ReplayDecision::Skipped(r) => {
                assert!(r.contains("no longer exists"), "reason: {r}");
            }
            ReplayDecision::Retried(_) => panic!("expected Skipped"),
        }
    }

    #[tokio::test]
    async fn title_skips_on_malformed_intent() {
        let (_d, db) = open_db().await;
        let book_id = seed_book_with_title(&db, "Old Title").await;
        let e = title_entry(1, book_id, json!({ "current": "Old Title", "intent": 42 }));
        let decision = TitleReplayer
            .try_replay(db.pool(), &e)
            .await
            .expect("try_replay");
        match decision {
            ReplayDecision::Skipped(r) => {
                assert!(r.contains("intent"), "reason: {r}");
            }
            ReplayDecision::Retried(_) => panic!("expected Skipped"),
        }
    }
}
