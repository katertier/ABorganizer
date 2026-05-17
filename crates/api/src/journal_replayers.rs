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

use crate::progress::{OP_KIND_BOOK_RATING_SET, OP_KIND_BOOK_STATUS_SET};

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
}
