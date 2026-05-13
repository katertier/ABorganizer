//! `tag-write-early` and `tag-write-final` Stage skeletons
//! (ADR-0028).
//!
//! Both stages are scaffolding-only at this slice: `run()` returns
//! `Skipped` until the lofty-based tag-write integration lands.
//! What's real here:
//!
//! - Typed [`StageId`] constants ([`TAG_WRITE_EARLY_STAGE_ID`],
//!   [`TAG_WRITE_FINAL_STAGE_ID`]) — usable by other stages in
//!   their `requires()` lists immediately, without waiting for
//!   the write body.
//! - `requires()` lists — the minimal subset needed for this
//!   slice's `Stage::requires` graph to type-check. The full set
//!   per ADR-0028 (every AI extractor + transcode for the late
//!   pass) lands as each upstream stage's `StageId` becomes
//!   referenceable.
//!
//! Not registered in `aborg-daemon`'s pipeline registry yet — a
//! `Skipped` skeleton has no operator-visible effect, and
//! registering it would surface a confusing "stage runs but does
//! nothing" in the pipeline-progress UI.

use async_trait::async_trait;

use ab_core::{BookId, Result};
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};

/// Typed stage identifier for the early tag-write pass.
pub const TAG_WRITE_EARLY_STAGE_ID: StageId = StageId::new("tag-write-early");
/// Stable `&'static str` mirror of [`TAG_WRITE_EARLY_STAGE_ID`].
pub const TAG_WRITE_EARLY_STAGE_NAME: &str = TAG_WRITE_EARLY_STAGE_ID.as_str();

/// Typed stage identifier for the final tag-write pass.
pub const TAG_WRITE_FINAL_STAGE_ID: StageId = StageId::new("tag-write-final");
/// Stable `&'static str` mirror of [`TAG_WRITE_FINAL_STAGE_ID`].
pub const TAG_WRITE_FINAL_STAGE_NAME: &str = TAG_WRITE_FINAL_STAGE_ID.as_str();

/// `Stage::requires` set for the early pass.
///
/// Per ADR-0028: `tag-read`, `identity-resolve`, `extract-dna-tags`.
/// Only `tag-read` exists as a referenceable `StageId` today; the
/// other two land on their owning crates' typed-`StageId` slices
/// and get appended here. Keeping the partial list live (rather
/// than empty) is intentional — the dispatcher can already enforce
/// the one ordering dependency we know about.
const TAG_WRITE_EARLY_REQUIRES: &[StageId] = &[StageId::new("tag-read")];

/// `Stage::requires` set for the final pass.
///
/// Per ADR-0028 § "`TagWriteFinal` `requires()`": every AI extractor
/// that can produce a `book_field_provenance` row, plus
/// `transcode-m4b` when present. Today only `tag-read` exists as a
/// referenceable `StageId`; the rest land slice-by-slice. The
/// scheduler treats `Skipped` outcomes as satisfied, so missing
/// upstreams don't deadlock books with no transcript.
const TAG_WRITE_FINAL_REQUIRES: &[StageId] = &[StageId::new("tag-read")];

/// Early-pass tag-write stage (ADR-0028 § `TagWriteEarly`).
///
/// Intended priority: `Foreground`. Writes the 16 fields in the
/// `book_field_provenance.field` `CHECK` set (`title`, `subtitle`,
/// `description`, `language`, `release_date`, `duration_seconds`,
/// `asin`, `isbn`, `author`, `narrator`, `publisher`, `series`,
/// `genre`, `cover_url`, `abridged`, `explicit`) using the
/// `is_winner = 1` row for each field. Skips the I/O when only
/// `source = 'tag_file'` winners exist (no point writing tags we
/// just read).
///
/// Scaffolding only at this slice — `run()` returns `Skipped`.
#[derive(Debug, Default)]
pub struct TagWriteEarlyStage;

impl TagWriteEarlyStage {
    /// Construct.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Stage for TagWriteEarlyStage {
    fn name(&self) -> &'static str {
        TAG_WRITE_EARLY_STAGE_NAME
    }

    fn requires(&self) -> &'static [StageId] {
        TAG_WRITE_EARLY_REQUIRES
    }

    async fn run(&self, _ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        // Skeleton — the lofty-based tag writer lives in a
        // follow-up slice. Returning Skipped keeps the
        // dispatcher's graph healthy.
        tracing::debug!(
            book = %book_id,
            stage = TAG_WRITE_EARLY_STAGE_NAME,
            "tag-write.early.skeleton_skip"
        );
        Ok(StageOutcome::Skipped)
    }
}

/// Final-pass tag-write stage (ADR-0028 § `TagWriteFinal`).
///
/// Intended priority: `Background`. Writes every field with a
/// winner *that the early pass didn't already write the same
/// value for*, EXCEPT fields whose winner has
/// `source = 'user_edit'`. Per ADR-0028 § "Skips per-field on
/// user-edit": the user's correction wins until they explicitly
/// clear it.
///
/// The per-field skip is via [`crate::skip_for_final_pass`] —
/// kept as a free function so the convention's exact spelling
/// lives in one place (`crate::USER_EDIT_SOURCE`).
///
/// Scaffolding only at this slice — `run()` returns `Skipped`.
#[derive(Debug, Default)]
pub struct TagWriteFinalStage;

impl TagWriteFinalStage {
    /// Construct.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Stage for TagWriteFinalStage {
    fn name(&self) -> &'static str {
        TAG_WRITE_FINAL_STAGE_NAME
    }

    fn requires(&self) -> &'static [StageId] {
        TAG_WRITE_FINAL_REQUIRES
    }

    async fn run(&self, _ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        tracing::debug!(
            book = %book_id,
            stage = TAG_WRITE_FINAL_STAGE_NAME,
            "tag-write.final.skeleton_skip"
        );
        Ok(StageOutcome::Skipped)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use ab_core::tunables::DbTunables;
    use ab_db::{EphemeralDb, LibraryDb};
    use std::path::Path;
    use tempfile::TempDir;

    async fn fresh_ctx(dir: &Path, stage_name: &'static str) -> StageContext {
        let lib = LibraryDb::open(&dir.join("library.db"), &DbTunables::default())
            .await
            .expect("open library");
        let eph = EphemeralDb::open(&dir.join("ephemeral.db"), &DbTunables::default())
            .await
            .expect("open ephemeral");
        StageContext {
            library: lib,
            ephemeral: eph,
            cancel: tokio_util::sync::CancellationToken::new(),
            stage_name,
        }
    }

    #[test]
    fn typed_stage_ids_pin_strings() {
        assert_eq!(TAG_WRITE_EARLY_STAGE_ID.as_str(), "tag-write-early");
        assert_eq!(TAG_WRITE_FINAL_STAGE_ID.as_str(), "tag-write-final");
        // Mirror constants stay in lock-step.
        assert_eq!(
            TAG_WRITE_EARLY_STAGE_NAME,
            TAG_WRITE_EARLY_STAGE_ID.as_str()
        );
        assert_eq!(
            TAG_WRITE_FINAL_STAGE_NAME,
            TAG_WRITE_FINAL_STAGE_ID.as_str()
        );
    }

    #[tokio::test]
    async fn early_stage_metadata() {
        let s = TagWriteEarlyStage::new();
        assert_eq!(s.name(), "tag-write-early");
        // Partial requires: tag-read only, rest land as upstream
        // StageIds become referenceable.
        assert_eq!(s.requires(), &[StageId::new("tag-read")]);
    }

    #[tokio::test]
    async fn final_stage_metadata() {
        let s = TagWriteFinalStage::new();
        assert_eq!(s.name(), "tag-write-final");
        assert_eq!(s.requires(), &[StageId::new("tag-read")]);
    }

    #[tokio::test]
    async fn early_skeleton_run_returns_skipped() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path(), TAG_WRITE_EARLY_STAGE_NAME).await;
        let outcome = TagWriteEarlyStage::new()
            .run(&ctx, BookId(1))
            .await
            .expect("run");
        match outcome {
            StageOutcome::Skipped => {}
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn final_skeleton_run_returns_skipped() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path(), TAG_WRITE_FINAL_STAGE_NAME).await;
        let outcome = TagWriteFinalStage::new()
            .run(&ctx, BookId(1))
            .await
            .expect("run");
        match outcome {
            StageOutcome::Skipped => {}
            other => panic!("expected Skipped, got {other:?}"),
        }
    }
}
