//! `transcode-m4b` Stage (ADR-0027).
//!
//! Re-encodes every active non-m4b source file for a book into
//! the canonical m4b container. Each transcode acquires a
//! [`ab_db::book_file_refs`] ref on the source so concurrent AI
//! consumers (transcribe, fingerprint, detect-audiologo) can't
//! lose the file out from under them mid-read. The source rows
//! stay `is_active = 1` until the
//! [`crate::PostTranscodeSourcesTarget`] cleanup target reaps
//! them (gated on `live_ref_count == 0` AND an m4b output row
//! existing). End result: one m4b per source file, replacing
//! the source on disk + in `book_files` once cleanup runs.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use ab_core::{BookId, FileId, Result};
use ab_db::book_file_refs;
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};

/// Typed stage identifier.
pub const STAGE_ID: StageId = StageId::new("transcode-m4b");

/// Convenience alias.
pub const STAGE_NAME: &str = STAGE_ID.as_str();

/// Background-priority transcode-to-m4b stage. See module docs.
///
/// ## Failure semantics
///
/// Per-source failures (a single bad file in a multi-file book)
/// log a `tracing::warn!` and continue; the stage still returns
/// [`StageOutcome::Done`] if *any* file transcoded. The cleanup
/// target ignores books with no m4b output, so a fully-failed
/// transcode leaves the book in its original state for the next
/// pipeline pass.
#[derive(Debug, Default)]
pub struct TranscodeM4bStage;

impl TranscodeM4bStage {
    /// Construct.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

/// Derive the m4b output path next to the source. We swap the
/// extension to `.m4b`; the .m4a container is identical to .m4b
/// on disk, the extension is the audiobook-convention marker.
/// Collision with a literal existing `.m4b` neighbour is
/// extremely unlikely in practice (the source-filter rejects
/// existing m4b rows), but if it happens the Swift bridge
/// removes the stale output before exporting.
fn derive_output_path(source: &Path) -> PathBuf {
    source.with_extension("m4b")
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

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        let book_id_raw = book_id.0;

        // Idempotency gate: if this book already has an active
        // m4b row, the stage's work is done — either we ran
        // previously, or the source itself arrived as m4b.
        // Cleanup hasn't necessarily run; that's the cleanup
        // target's predicate, not ours.
        let already_m4b: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64"
                 FROM book_files
                WHERE book_id = ? AND format = 'm4b' AND is_active = 1"#,
            book_id_raw,
        )
        .fetch_one(ctx.library.pool())
        .await
        .map_err(|e| ab_core::Error::Database(format!("transcode m4b-exists check: {e}")))?;
        if already_m4b > 0 {
            tracing::debug!(book = %book_id, "transcode.skip_already_m4b");
            return Ok(StageOutcome::Skipped);
        }

        // Fetch active non-m4b sources. `format` may be NULL
        // (scan stage doesn't always tag the format); treat
        // unknown-format rows as eligible — the Swift bridge
        // surfaces an `AssetLoadFailed` if the codec is unknown,
        // which we log + skip per-source.
        let sources = sqlx::query!(
            r#"SELECT file_id  AS "file_id!: i64",
                      file_path AS "file_path!: String"
                 FROM book_files
                WHERE book_id = ?
                  AND is_active = 1
                  AND (format IS NULL OR format != 'm4b')
                ORDER BY file_id"#,
            book_id_raw,
        )
        .fetch_all(ctx.library.pool())
        .await
        .map_err(|e| ab_core::Error::Database(format!("transcode sources query: {e}")))?;

        if sources.is_empty() {
            tracing::debug!(book = %book_id, "transcode.skip_no_sources");
            return Ok(StageOutcome::Skipped);
        }

        let mut any_succeeded = false;
        for src in sources {
            // Daemon shutdown / SIGTERM cancellation: bail
            // between files. The current export-session FFI
            // doesn't support cancellation mid-encode; we
            // accept the in-flight cost and stop after.
            if ctx.cancel.is_cancelled() {
                tracing::info!(book = %book_id, "transcode.cancelled");
                break;
            }

            let file_id = FileId(src.file_id);
            let input = PathBuf::from(&src.file_path);
            let output = derive_output_path(&input);

            // Acquire ref before any work. Release on every
            // exit path (success + failure).
            let handle =
                book_file_refs::acquire(ctx.library.pool(), file_id, STAGE_NAME, book_id).await?;

            let result = ab_audio::transcode_to_m4b(&input, &output).await;

            // Release the ref before we touch `book_files` so
            // a long INSERT can't accidentally hold the source
            // alive past the transcode itself.
            handle.release(ctx.library.pool()).await?;

            if let Err(e) = result {
                tracing::warn!(
                    book = %book_id,
                    file_id = src.file_id,
                    source = %input.display(),
                    error = %e,
                    "transcode.file_failed"
                );
                continue;
            }

            // Insert the m4b row. The cleanup target uses
            // `format = 'm4b' AND is_active = 1` as its
            // m4b-exists gate, so getting this row's format
            // right matters more than the file_size precision.
            let size: Option<i64> = match tokio::fs::metadata(&output).await {
                Ok(m) => i64::try_from(m.len()).ok(),
                Err(e) => {
                    tracing::warn!(
                        book = %book_id,
                        file_id = src.file_id,
                        output = %output.display(),
                        error = %e,
                        "transcode.output_stat_failed"
                    );
                    // Output stat failed but the transcode
                    // reported success — surface the path
                    // anyway; the cleanup target won't reap
                    // the source until refs settle.
                    None
                }
            };
            let output_str = output.to_string_lossy().into_owned();
            sqlx::query!(
                "INSERT INTO book_files \
                   (book_id, file_path, file_size, format, is_active) \
                 VALUES (?, ?, ?, 'm4b', 1)",
                book_id_raw,
                output_str,
                size,
            )
            .execute(ctx.library.pool())
            .await
            .map_err(|e| ab_core::Error::Database(format!("transcode insert m4b row: {e}")))?;
            any_succeeded = true;

            tracing::info!(
                book = %book_id,
                src_file_id = src.file_id,
                source = %input.display(),
                output = %output.display(),
                "transcode.file_completed"
            );
        }

        if any_succeeded {
            Ok(StageOutcome::Done)
        } else {
            // No file transcoded successfully — leave
            // `pipeline_progress` so the retry endpoint can
            // re-trigger after the operator fixes whatever
            // caused the per-file failures.
            Ok(StageOutcome::Skipped)
        }
    }

    /// Reset semantics: rip out every m4b row + on-disk file
    /// that this stage would have produced. Reset is operator-
    /// triggered (via `/books/{id}/retry`), so the implicit
    /// contract is "the operator wants the transcode to re-run."
    ///
    /// We deliberately delete every active `format='m4b'` row
    /// — including books whose source already arrived as m4b
    /// (no transcode happened, but a retry would still find
    /// `already_m4b > 0` and skip anyway, so the operator's
    /// only path forward IS to clear that row). If the source
    /// arrived as m4b the on-disk file is the source itself —
    /// the operator will need to re-scan to repopulate the
    /// row. This is the same tradeoff every "best-effort
    /// reset" stage takes.
    ///
    /// Live refs leaked by this stage (somehow) are also
    /// released: future invocations would otherwise see
    /// `live_ref_count > 0` from a previous-run leak and the
    /// cleanup target would never reap the source.
    async fn reset(&self, ctx: &StageContext, book_id: BookId) -> Result<()> {
        let book_id_raw = book_id.0;

        // Find every active m4b row for this book.
        let rows = sqlx::query!(
            r#"SELECT file_id AS "file_id!: i64",
                      file_path AS "file_path!: String"
                 FROM book_files
                WHERE book_id = ? AND format = 'm4b' AND is_active = 1"#,
            book_id_raw,
        )
        .fetch_all(ctx.library.pool())
        .await
        .map_err(|e| ab_core::Error::Database(format!("transcode reset query: {e}")))?;

        for row in rows {
            // Best-effort disk delete. `ENOENT` is fine — the
            // file was already gone (operator-deleted, or the
            // cleanup target already reaped it).
            match tokio::fs::remove_file(&row.file_path).await {
                Ok(()) | Err(_) => {}
            }
            sqlx::query!("DELETE FROM book_files WHERE file_id = ?", row.file_id)
                .execute(ctx.library.pool())
                .await
                .map_err(|e| ab_core::Error::Database(format!("transcode reset delete: {e}")))?;
        }

        // Release any live refs this stage holds for this
        // (stage, book) pair. Idempotent — the UPDATE matches
        // zero rows when there are no leaks.
        let stage_name = STAGE_NAME;
        sqlx::query!(
            "UPDATE book_file_refs \
                SET released_at = strftime('%s','now') \
              WHERE holder_stage = ? \
                AND holder_book_id = ? \
                AND released_at IS NULL",
            stage_name,
            book_id_raw,
        )
        .execute(ctx.library.pool())
        .await
        .map_err(|e| ab_core::Error::Database(format!("transcode reset leaked refs: {e}")))?;

        // Fall through to the default reset to clear
        // `pipeline_progress` + any cache rows (none for this
        // stage, but the default is idempotent).
        ab_pipeline::stage::default_reset(STAGE_NAME, ctx, book_id).await
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use ab_core::tunables::DbTunables;
    use ab_db::{EphemeralDb, LibraryDb};
    use std::path::Path as StdPath;
    use tempfile::TempDir;

    async fn fresh_ctx(dir: &StdPath) -> StageContext {
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

    /// Generate a small m4a fixture from a system-shipped AIFF
    /// via `afconvert`. Returns `None` when the bridge isn't
    /// linked (Linux CI, no swiftc) or the system file is
    /// missing — callers skip the test in that case rather
    /// than failing.
    async fn seed_m4a_fixture(dir: &StdPath) -> Option<PathBuf> {
        if !ab_audio::is_bridge_compiled() {
            return None;
        }
        let src_aiff = StdPath::new("/System/Library/Sounds/Submarine.aiff");
        if !src_aiff.exists() {
            return None;
        }
        let out_m4a = dir.join("source.m4a");
        let status = tokio::process::Command::new("/usr/bin/afconvert")
            .args(["-d", "aac", "-f", "m4af"])
            .arg(src_aiff)
            .arg(&out_m4a)
            .status()
            .await
            .ok()?;
        if !status.success() {
            return None;
        }
        Some(out_m4a)
    }

    async fn seed_book_with_source(library: &LibraryDb, source_path: &StdPath) -> (i64, i64) {
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'Fixture Book')")
            .execute(library.pool())
            .await
            .expect("seed book");
        let path_str = source_path.to_string_lossy().into_owned();
        let r = sqlx::query(
            "INSERT INTO book_files \
               (book_id, file_path, file_size, format, is_active) \
             VALUES (1, ?, 0, 'm4a', 1) \
             RETURNING file_id",
        )
        .bind(&path_str)
        .execute(library.pool())
        .await
        .expect("seed file");
        // `RETURNING file_id` via `.execute` doesn't surface the
        // returned column; re-fetch.
        let file_id: i64 = sqlx::query_scalar("SELECT file_id FROM book_files WHERE file_path = ?")
            .bind(&path_str)
            .fetch_one(library.pool())
            .await
            .expect("fetch file_id");
        let _ = r;
        (1, file_id)
    }

    #[tokio::test]
    async fn stage_metadata_pins_name_and_empty_requires() {
        let s = TranscodeM4bStage::new();
        assert_eq!(s.name(), "transcode-m4b");
        assert!(s.requires().is_empty(), "no upstream deps per ADR-0027");
    }

    #[tokio::test]
    async fn run_returns_skipped_when_no_sources() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'Empty')")
            .execute(ctx.library.pool())
            .await
            .expect("seed");
        let outcome = TranscodeM4bStage::new()
            .run(&ctx, BookId(1))
            .await
            .expect("run");
        assert!(
            matches!(outcome, StageOutcome::Skipped),
            "no sources → Skipped"
        );
    }

    #[tokio::test]
    async fn run_returns_skipped_when_book_already_has_m4b() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'Already-m4b')")
            .execute(ctx.library.pool())
            .await
            .expect("seed book");
        sqlx::query(
            "INSERT INTO book_files \
               (book_id, file_path, format, is_active) \
             VALUES (1, '/tmp/already.m4b', 'm4b', 1)",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed m4b");
        let outcome = TranscodeM4bStage::new()
            .run(&ctx, BookId(1))
            .await
            .expect("run");
        assert!(
            matches!(outcome, StageOutcome::Skipped),
            "existing m4b → Skipped"
        );
    }

    /// End-to-end: seed a book + a real m4a fixture, run the
    /// stage, assert a new m4b row exists + the source's
    /// `live_ref_count` is back to zero.
    #[tokio::test]
    async fn run_transcodes_source_and_inserts_m4b_row() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        let Some(source) = seed_m4a_fixture(tmp.path()).await else {
            return; // no bridge / no system fixture — skip.
        };
        let (book_id, src_file_id) = seed_book_with_source(&ctx.library, &source).await;

        let outcome = TranscodeM4bStage::new()
            .run(&ctx, BookId(book_id))
            .await
            .expect("run");
        assert!(matches!(outcome, StageOutcome::Done), "got {outcome:?}");

        // The expected m4b output path is the source with .m4b
        // extension, sitting next to it.
        let expected_output = source.with_extension("m4b");
        let meta = std::fs::metadata(&expected_output).expect("output exists on disk");
        assert!(meta.len() > 0, "output is empty");

        // book_files: source row still active; new m4b row
        // also active.
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM book_files \
              WHERE book_id = ? AND format = 'm4b' AND is_active = 1",
        )
        .bind(book_id)
        .fetch_one(ctx.library.pool())
        .await
        .expect("count m4b");
        assert_eq!(count, 1, "expected exactly one new m4b row");

        let src_active: i64 =
            sqlx::query_scalar("SELECT is_active FROM book_files WHERE file_id = ?")
                .bind(src_file_id)
                .fetch_one(ctx.library.pool())
                .await
                .expect("src row");
        assert_eq!(
            src_active, 1,
            "source row stays active; cleanup target reaps later"
        );

        // live_ref_count for the source must be 0 — the stage
        // released its handle.
        let live = book_file_refs::live_ref_count(ctx.library.pool(), FileId(src_file_id))
            .await
            .expect("count");
        assert_eq!(live, 0, "ref leaked");
    }

    /// Idempotency: a second run on the same book is a Skipped
    /// no-op (because the first run already created an m4b row).
    #[tokio::test]
    async fn second_run_is_skipped() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        let Some(source) = seed_m4a_fixture(tmp.path()).await else {
            return;
        };
        let (book_id, _) = seed_book_with_source(&ctx.library, &source).await;

        let stage = TranscodeM4bStage::new();
        let _ = stage.run(&ctx, BookId(book_id)).await.expect("run1");
        let outcome2 = stage.run(&ctx, BookId(book_id)).await.expect("run2");
        assert!(
            matches!(outcome2, StageOutcome::Skipped),
            "second run should Skip, got {outcome2:?}"
        );
    }

    /// `reset()` removes the on-disk m4b + the row + clears
    /// the `pipeline_progress` entry, so a subsequent run starts
    /// from scratch.
    #[tokio::test]
    async fn reset_clears_m4b_row_and_file() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        let Some(source) = seed_m4a_fixture(tmp.path()).await else {
            return;
        };
        let (book_id, _) = seed_book_with_source(&ctx.library, &source).await;
        let stage = TranscodeM4bStage::new();
        stage.run(&ctx, BookId(book_id)).await.expect("run");
        let expected_output = source.with_extension("m4b");
        assert!(expected_output.exists(), "pre-reset: m4b should exist");

        stage.reset(&ctx, BookId(book_id)).await.expect("reset");
        assert!(
            !expected_output.exists(),
            "reset should delete the m4b file"
        );
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM book_files WHERE book_id = ? AND format = 'm4b'",
        )
        .bind(book_id)
        .fetch_one(ctx.library.pool())
        .await
        .expect("count");
        assert_eq!(count, 0, "reset should delete the m4b row");
    }
}
