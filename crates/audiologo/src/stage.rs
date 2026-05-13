//! `detect-audiologo` pipeline stage.
//!
//! Slice 4B (the parent slice for this file): runs publisher-
//! jingle detection across each active book file. Per ADR-0024
//! Revision 2 the detection path is fingerprint-only; the
//! `audiologos` table holds known publisher fingerprints
//! (seeded + grown via review confirmation + `ABtagger` import),
//! and this stage windows the head/tail of each file via the
//! `ab_audio::read_samples_window` Swift FFI bridge, fingerprints
//! the samples via `ab_fingerprint::fingerprint_samples`, and
//! `slide_match`-es every candidate audiologo against the result.
//!
//! ## Slice ladder
//!
//! - **4B.3 (this slice):** stage skeleton. `Stage` trait impl +
//!   `STAGE_ID` + `Stage::reset` override + minimal `run()` body
//!   that bails Skipped (no detection logic). Pinned at this
//!   slice so the dispatcher + retry surface (ADR-0023) can wire
//!   the stage in cleanly before the detection logic lands.
//! - **4B.4:** wire `FingerprintFull` + `FingerprintBookend` into
//!   `run()`; auto-apply high-confidence matches.
//! - **4B.5:** chapter-shift maths on apply + Libation-stripped
//!   path (when `brand_intro_duration_ms` is non-NULL but no
//!   fingerprint hit).
//! - **4B.6:** integration tests + ADR-0024 closure note.
//!
//! ## `Stage::reset` semantics
//!
//! Per ADR-0024 § state-machine diagram: a reset doesn't delete
//! `book_file_audiologos` rows. Instead it flips rows currently
//! at `applied` → `re_detected` (preserving the audit trail),
//! NULLs `audiologo_status` back to its default, and clears
//! `pipeline_progress`. The next run produces fresh `candidate`
//! / `applied` rows; the prior `re_detected` ones surface in the
//! review UI as "previously applied → superseded."
//!
//! Rows already at `candidate` or `rejected` are left alone —
//! `candidate` rows are normal pre-apply state; `rejected` rows
//! are user-final decisions and shouldn't reappear as candidates.

use async_trait::async_trait;

use ab_core::{BookId, Error, Result};
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};

use crate::Status;

/// Typed stage identifier for this stage.
pub const STAGE_ID: StageId = StageId::new("detect-audiologo");

/// Convenience alias matching the per-stage `STAGE_NAME = STAGE_ID.as_str()`
/// pattern used across the workspace.
pub const STAGE_NAME: &str = STAGE_ID.as_str();

/// Background-priority stage that detects publisher jingles at
/// the head + tail of each active book file.
///
/// 4B.3 ships the skeleton; detection logic lands in 4B.4.
#[derive(Debug, Default)]
pub struct DetectAudiologoStage;

impl DetectAudiologoStage {
    /// Construct.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Stage for DetectAudiologoStage {
    fn name(&self) -> &'static str {
        STAGE_NAME
    }

    fn requires(&self) -> &'static [StageId] {
        // Transcript-aided tiers (4C) need the head/tail transcript
        // + sample-window transcripts. Even though 4B.3 itself
        // doesn't read transcripts, locking the requires() list
        // here means the slice ladder doesn't reshuffle dependency
        // edges as later sub-slices land — easier on the scheduler
        // + retry surface to know the full predecessor set early.
        const REQS: &[StageId] = &[
            ab_transcript::stage::STAGE_ID,
            ab_transcript::samples_stage::STAGE_ID,
        ];
        REQS
    }

    async fn run(&self, _ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        // 4B.3 skeleton: no detection logic. The full body lands
        // in 4B.4 + 4B.5. Until then the stage runs to completion
        // (Skipped, not Done) so the dispatcher doesn't block the
        // downstream graph; once 4B.4 wires real detection in, the
        // outcome flips to Done on actual work and Skipped when a
        // book has no fingerprint candidates.
        tracing::debug!(
            book = %book_id,
            stage = STAGE_NAME,
            "audiologo.detect.skeleton_skip"
        );
        Ok(StageOutcome::Skipped)
    }

    /// Per ADR-0024 § state-machine diagram. Flips `applied` rows
    /// for this book to `re_detected` (preserving the audit trail),
    /// NULLs `books.audiologo_status` back to its default, and
    /// then delegates to `default_reset` for the
    /// `pipeline_progress` / `book_field_provenance` / `ai_cache`
    /// cleanup.
    ///
    /// `candidate` / `rejected` rows are left intact — see the
    /// module docstring for the rationale.
    async fn reset(&self, ctx: &StageContext, book_id: BookId) -> Result<()> {
        let id = book_id.0;
        let applied = Status::Applied.as_str();
        let re_detected = Status::ReDetected.as_str();
        let unknown = crate::BookStatus::Unknown.as_str();

        let mut tx = ctx
            .library
            .pool()
            .begin()
            .await
            .map_err(|e| Error::Database(format!("detect-audiologo reset tx: {e}")))?;

        sqlx::query!(
            "UPDATE book_file_audiologos \
                SET status = ?, \
                    re_detected_at = strftime('%s','now') \
              WHERE file_id IN ( \
                  SELECT file_id FROM book_files WHERE book_id = ? \
              ) \
                AND status = ?",
            re_detected,
            id,
            applied,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| Error::Database(format!("detect-audiologo reset audiologo rows: {e}")))?;

        sqlx::query!(
            "UPDATE books SET audiologo_status = ? WHERE book_id = ?",
            unknown,
            id,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| Error::Database(format!("detect-audiologo reset book status: {e}")))?;

        tx.commit()
            .await
            .map_err(|e| Error::Database(format!("detect-audiologo reset commit: {e}")))?;

        ab_pipeline::default_reset(STAGE_NAME, ctx, book_id).await
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use ab_core::tunables::DbTunables;
    use tempfile::TempDir;

    async fn fresh_ctx(dir: &std::path::Path) -> StageContext {
        let lib = ab_db::LibraryDb::open(&dir.join("library.db"), &DbTunables::default())
            .await
            .expect("open library");
        let eph = ab_db::EphemeralDb::open(&dir.join("ephemeral.db"), &DbTunables::default())
            .await
            .expect("open ephemeral");
        StageContext {
            library: lib,
            ephemeral: eph,
            cancel: tokio_util::sync::CancellationToken::new(),
            stage_name: STAGE_NAME,
        }
    }

    #[tokio::test]
    async fn stage_metadata_matches_pipeline_expectations() {
        let stage = DetectAudiologoStage::new();
        assert_eq!(stage.name(), "detect-audiologo");
        assert_eq!(
            stage.requires(),
            &[
                ab_transcript::stage::STAGE_ID,
                ab_transcript::samples_stage::STAGE_ID,
            ]
        );
    }

    #[tokio::test]
    async fn run_returns_skipped_in_skeleton() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        let stage = DetectAudiologoStage::new();
        let outcome = stage
            .run(&ctx, BookId(1))
            .await
            .expect("skeleton run does not error");
        match outcome {
            StageOutcome::Skipped => {}
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reset_flips_applied_rows_to_re_detected() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;

        // Seed: one book, one file, two audiologo rows (one
        // applied, one candidate). Reset should flip the applied
        // one + leave the candidate alone.
        sqlx::query(
            "INSERT INTO books (book_id, title, audiologo_status) VALUES (1, 'fixture', 'applied')",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed book");
        sqlx::query("INSERT INTO book_files (file_id, book_id, file_path, is_active) VALUES (10, 1, '/tmp/a.m4b', 1)")
            .execute(ctx.library.pool())
            .await
            .expect("seed file");
        sqlx::query(
            "INSERT INTO book_file_audiologos \
             (audiologo_row_id, file_id, kind, jingle_start_ms, jingle_end_ms, padding_ms, method, audiologo_id, confidence, status) \
             VALUES \
             (100, 10, 'intro', 0, 5000, 250, 'fingerprint_full', NULL, 0.9, 'applied'), \
             (101, 10, 'outro', 0, 5000, 250, 'fingerprint_full', NULL, 0.6, 'candidate')",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed audiologo rows");
        sqlx::query(
            "INSERT INTO pipeline_progress (book_id, stage, status, started_at, completed_at) \
             VALUES (1, 'detect-audiologo', 'succeeded', 0, 1)",
        )
        .execute(ctx.ephemeral.pool())
        .await
        .expect("seed progress");

        DetectAudiologoStage::new()
            .reset(&ctx, BookId(1))
            .await
            .expect("reset");

        let row_100_status: String = sqlx::query_scalar(
            "SELECT status FROM book_file_audiologos WHERE audiologo_row_id = 100",
        )
        .fetch_one(ctx.library.pool())
        .await
        .expect("fetch 100");
        assert_eq!(row_100_status, "re_detected", "applied → re_detected");

        let row_100_re_detected_at: Option<i64> = sqlx::query_scalar(
            "SELECT re_detected_at FROM book_file_audiologos WHERE audiologo_row_id = 100",
        )
        .fetch_one(ctx.library.pool())
        .await
        .expect("fetch ts");
        assert!(
            row_100_re_detected_at.is_some(),
            "re_detected_at must be populated"
        );

        let row_101_status: String = sqlx::query_scalar(
            "SELECT status FROM book_file_audiologos WHERE audiologo_row_id = 101",
        )
        .fetch_one(ctx.library.pool())
        .await
        .expect("fetch 101");
        assert_eq!(row_101_status, "candidate", "candidate row untouched");

        let book_status: String =
            sqlx::query_scalar("SELECT audiologo_status FROM books WHERE book_id = 1")
                .fetch_one(ctx.library.pool())
                .await
                .expect("fetch book status");
        assert_eq!(book_status, "unknown", "book status reset to unknown");

        let progress_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pipeline_progress WHERE book_id = 1 AND stage = 'detect-audiologo'",
        )
        .fetch_one(ctx.ephemeral.pool())
        .await
        .expect("count progress");
        assert_eq!(progress_count, 0, "default_reset clears pipeline_progress");
    }
}
