//! `transcode-m4b` Stage (ADR-0027).

use async_trait::async_trait;

use ab_core::{BookId, Result};
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};

/// Typed stage identifier.
pub const STAGE_ID: StageId = StageId::new("transcode-m4b");

/// Convenience alias.
pub const STAGE_NAME: &str = STAGE_ID.as_str();

/// Background-priority transcode-to-m4b stage.
///
/// Scaffolding only at this slice: `run()` returns `Skipped`
/// until the Swift `AVAssetExportSession` wrapper lands in
/// `ab-audio`. The intended flow is:
///
/// 1. Acquire a [`ab_db::book_file_refs::acquire`] ref on the
///    source file (`holder_stage` = `'transcode-m4b'`).
/// 2. Re-encode via Swift FFI → `$book_root/{book_id}.m4b`.
/// 3. Update `book_files` to point at the new m4b file (or
///    insert a new `book_files` row for it; final shape TBD).
/// 4. Release the ref.
/// 5. The `post-transcode-sources` cleanup target (in
///    [`crate::cleanup`]) reaps the source file when its
///    `live_ref_count` drops to zero AND the m4b output exists.
#[derive(Debug, Default)]
pub struct TranscodeM4bStage;

impl TranscodeM4bStage {
    /// Construct.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Stage for TranscodeM4bStage {
    fn name(&self) -> &'static str {
        STAGE_NAME
    }

    fn requires(&self) -> &'static [StageId] {
        // Per ADR-0027: no upstream requires. Transcode runs in
        // parallel with every other stage; `book_file_refs`
        // keeps the source alive while AI consumers are still
        // reading it.
        &[]
    }

    async fn run(&self, _ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        // Skeleton — the Swift AVAssetExportSession wrapper +
        // m4b writeback live in a follow-up slice. Returning
        // Skipped (rather than Err) keeps the dispatcher's
        // graph healthy: when the stage is registered, books
        // flow through it as a no-op until the encode body
        // arrives.
        tracing::debug!(
            book = %book_id,
            stage = STAGE_NAME,
            "transcode.skeleton_skip"
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

    async fn fresh_ctx(dir: &Path) -> StageContext {
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
            stage_name: STAGE_NAME,
        }
    }

    #[tokio::test]
    async fn stage_metadata_pins_name_and_empty_requires() {
        let s = TranscodeM4bStage::new();
        assert_eq!(s.name(), "transcode-m4b");
        assert!(s.requires().is_empty(), "no upstream deps per ADR-0027");
    }

    #[tokio::test]
    async fn skeleton_run_returns_skipped() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        let outcome = TranscodeM4bStage::new()
            .run(&ctx, BookId(1))
            .await
            .expect("skeleton run");
        match outcome {
            StageOutcome::Skipped => {}
            other => panic!("expected Skipped, got {other:?}"),
        }
    }
}
