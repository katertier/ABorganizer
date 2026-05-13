//! Source-file reaper after a successful m4b transcode (ADR-0027).
//!
//! `PostTranscodeSourcesTarget` is the [`CleanupTarget`] (ADR-0025)
//! counterpart to [`crate::TranscodeM4bStage`]. The stage encodes
//! `book_root/{book_id}.m4b` and registers a row for it in
//! `book_files`. This target reaps the *source* files (mp3, flac,
//! m4a, …) once both conditions hold:
//!
//! 1. **An m4b output exists for the same `book_id`** — an active
//!    `book_files` row whose `format = 'm4b'`. This is the
//!    "transcode succeeded" gate.
//! 2. **`live_ref_count(source_file_id) == 0`** — no stage is
//!    currently reading the source via [`ab_db::book_file_refs`].
//!
//! ## Slice scope (ADR-0027 first slice)
//!
//! The transcode stage currently ships as a `Skipped` skeleton —
//! no m4b output ever gets written. Both predicates are
//! implemented here unconditionally; the net effect in this
//! slice is a permanent no-op, because the m4b-existence gate
//! never matches. When the Swift `AVAssetExportSession` writeback
//! lands in a follow-up slice, this target activates on its own
//! without further code changes.
//!
//! ## Apply semantics
//!
//! - Disk delete first; on `NotFound` we proceed (already gone);
//!   on any other error we log and skip the DB update so the
//!   row stays active for a retry.
//! - DB update sets `is_active = 0` instead of `DELETE`. The row
//!   stays as audit trail; future scans skip non-active rows.
//! - Per-file ordering is intentional: never zero a `book_files`
//!   row while the file is still on disk.
//!
//! `Policy::force` is **not** honoured for this target — bypassing
//! the live-ref gate would defeat the whole point of ADR-0027's
//! refcount lifecycle. The age gate is also unused; m4b-existence
//! plus refcount are the only knobs.

use async_trait::async_trait;

use ab_core::cleanup::{Category, CleanupReport, Policy};
use ab_core::{Error, Result};
use ab_pipeline::cleanup::{CleanupCtx, CleanupTarget};

/// Stable target name used in logs, the API response, and the
/// `aborg clean disk` CLI summary.
pub const TARGET_NAME: &str = "post-transcode-sources";

/// One eligible source row pulled from `book_files`.
#[derive(Debug)]
#[allow(
    clippy::struct_field_names,
    reason = "fields mirror the book_files schema; renaming them would obscure the mapping"
)]
struct EligibleRow {
    file_id: i64,
    file_path: String,
    file_size: Option<i64>,
}

/// Reaps source files whose book has a finished m4b transcode
/// and no live ref. See module docs for full predicate.
#[derive(Debug, Default)]
pub struct PostTranscodeSourcesTarget;

impl PostTranscodeSourcesTarget {
    /// Construct.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

/// Select every `book_files` row eligible for reaping. Shared
/// between `report` (counts) and `apply` (iterates + deletes) so
/// the two stay byte-for-byte in sync on the predicate.
///
/// ```text
///   active source row in book_files
///   AND its book has another active row with format='m4b'
///   AND no live book_file_refs row for the source
/// ```
async fn select_eligible(pool: &sqlx::SqlitePool) -> Result<Vec<EligibleRow>> {
    let rows = sqlx::query!(
        r#"
        SELECT  bf.file_id   AS "file_id!: i64",
                bf.file_path AS "file_path!: String",
                bf.file_size AS "file_size: i64"
          FROM book_files bf
         WHERE bf.is_active = 1
           AND bf.format IS NOT NULL
           AND bf.format != 'm4b'
           AND EXISTS (
               SELECT 1 FROM book_files m4b
                WHERE m4b.book_id   = bf.book_id
                  AND m4b.is_active = 1
                  AND m4b.format    = 'm4b'
                  AND m4b.file_id   != bf.file_id
           )
           AND NOT EXISTS (
               SELECT 1 FROM book_file_refs r
                WHERE r.file_id    = bf.file_id
                  AND r.released_at IS NULL
           )
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| Error::Database(format!("post-transcode-sources select: {e}")))?;

    Ok(rows
        .into_iter()
        .map(|r| EligibleRow {
            file_id: r.file_id,
            file_path: r.file_path,
            file_size: r.file_size,
        })
        .collect())
}

#[async_trait]
impl CleanupTarget for PostTranscodeSourcesTarget {
    fn category(&self) -> Category {
        Category::Disk
    }

    fn name(&self) -> &'static str {
        TARGET_NAME
    }

    async fn report(&self, ctx: &CleanupCtx, _policy: &Policy) -> Result<CleanupReport> {
        let rows = select_eligible(ctx.library.pool()).await?;
        let items = u64::try_from(rows.len()).unwrap_or(0);
        let bytes = rows
            .iter()
            .filter_map(|r| r.file_size)
            .map(|n| u64::try_from(n).unwrap_or(0))
            .sum();
        Ok(CleanupReport {
            category: Category::Disk,
            name: TARGET_NAME.to_owned(),
            items,
            bytes,
        })
    }

    async fn apply(&self, ctx: &CleanupCtx, _policy: &Policy) -> Result<CleanupReport> {
        let rows = select_eligible(ctx.library.pool()).await?;
        let mut items: u64 = 0;
        let mut bytes: u64 = 0;
        for row in rows {
            // FS delete first. ENOENT is fine (already gone); any
            // other error → skip the DB update so a retry can try
            // again with the still-active row intact.
            let path = std::path::Path::new(&row.file_path);
            match tokio::fs::remove_file(path).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    tracing::debug!(
                        file_id = row.file_id,
                        path = %row.file_path,
                        "transcode.cleanup.already_gone"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        file_id = row.file_id,
                        path = %row.file_path,
                        error = %e,
                        "transcode.cleanup.delete_failed"
                    );
                    continue;
                }
            }

            sqlx::query!(
                "UPDATE book_files SET is_active = 0 WHERE file_id = ?",
                row.file_id,
            )
            .execute(ctx.library.pool())
            .await
            .map_err(|e| Error::Database(format!("post-transcode-sources update: {e}")))?;

            items += 1;
            bytes += row
                .file_size
                .and_then(|n| u64::try_from(n).ok())
                .unwrap_or(0);
        }

        tracing::info!(
            target = TARGET_NAME,
            sources_retired = items,
            bytes_freed = bytes,
            "transcode.cleanup.applied"
        );
        Ok(CleanupReport {
            category: Category::Disk,
            name: TARGET_NAME.to_owned(),
            items,
            bytes,
        })
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use ab_core::tunables::DbTunables;
    use ab_db::{EphemeralDb, LibraryDb, book_file_refs};
    use tempfile::TempDir;

    async fn fresh() -> (CleanupCtx, TempDir) {
        let tmp = TempDir::new().expect("tmpdir");
        let library = LibraryDb::open(&tmp.path().join("library.db"), &DbTunables::default())
            .await
            .expect("open library");
        let ephemeral = EphemeralDb::open(&tmp.path().join("ephemeral.db"), &DbTunables::default())
            .await
            .expect("open ephemeral");
        (CleanupCtx { library, ephemeral }, tmp)
    }

    async fn seed_book(ctx: &CleanupCtx, book_id: i64) {
        sqlx::query("INSERT INTO books (book_id, title) VALUES (?, 'fixture')")
            .bind(book_id)
            .execute(ctx.library.pool())
            .await
            .expect("seed book");
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "test seed helper mirrors the book_files schema columns"
    )]
    async fn seed_file(
        ctx: &CleanupCtx,
        file_id: i64,
        book_id: i64,
        path: &str,
        format: &str,
        size: i64,
    ) {
        sqlx::query(
            "INSERT INTO book_files (file_id, book_id, file_path, format, file_size, is_active) \
             VALUES (?, ?, ?, ?, ?, 1)",
        )
        .bind(file_id)
        .bind(book_id)
        .bind(path)
        .bind(format)
        .bind(size)
        .execute(ctx.library.pool())
        .await
        .expect("seed file");
    }

    #[tokio::test]
    async fn report_zero_when_no_m4b_output_exists() {
        // Source files alone, no m4b twin → never eligible.
        let (ctx, _tmp) = fresh().await;
        seed_book(&ctx, 1).await;
        seed_file(&ctx, 100, 1, "/tmp/a.mp3", "mp3", 1024).await;
        seed_file(&ctx, 101, 1, "/tmp/a.flac", "flac", 2048).await;

        let target = PostTranscodeSourcesTarget::new();
        let report = target
            .report(&ctx, &Policy::dry_run(86_400))
            .await
            .expect("report");
        assert_eq!(report.items, 0, "no m4b output ⇒ nothing eligible");
        assert_eq!(report.bytes, 0);
        assert_eq!(report.category, Category::Disk);
        assert_eq!(report.name, TARGET_NAME);
    }

    #[tokio::test]
    async fn report_counts_source_when_m4b_exists_and_no_refs() {
        let (ctx, _tmp) = fresh().await;
        seed_book(&ctx, 1).await;
        seed_file(&ctx, 100, 1, "/tmp/a.mp3", "mp3", 5_000).await;
        seed_file(&ctx, 200, 1, "/tmp/a.m4b", "m4b", 7_000).await;

        let target = PostTranscodeSourcesTarget::new();
        let report = target
            .report(&ctx, &Policy::dry_run(86_400))
            .await
            .expect("report");
        assert_eq!(report.items, 1, "mp3 source eligible");
        assert_eq!(report.bytes, 5_000, "size of source only");
    }

    #[tokio::test]
    async fn report_skips_source_with_live_ref() {
        let (ctx, _tmp) = fresh().await;
        seed_book(&ctx, 1).await;
        seed_file(&ctx, 100, 1, "/tmp/a.mp3", "mp3", 5_000).await;
        seed_file(&ctx, 200, 1, "/tmp/a.m4b", "m4b", 7_000).await;

        // Acquire a live ref on the source — should block reaping.
        let _h = book_file_refs::acquire(ctx.library.pool(), 100, "transcribe-head-tail", 1)
            .await
            .expect("acquire");

        let target = PostTranscodeSourcesTarget::new();
        let report = target
            .report(&ctx, &Policy::dry_run(86_400))
            .await
            .expect("report");
        assert_eq!(report.items, 0, "live ref blocks eligibility");
    }

    #[tokio::test]
    async fn apply_deletes_source_and_marks_inactive() {
        let (ctx, tmp) = fresh().await;
        seed_book(&ctx, 1).await;

        // Real file on disk so the FS delete path runs end-to-end.
        let src = tmp.path().join("a.mp3");
        tokio::fs::write(&src, b"junk").await.expect("write src");
        let src_str = src.to_str().expect("utf8");
        let m4b = tmp.path().join("a.m4b");
        tokio::fs::write(&m4b, b"junk2").await.expect("write m4b");
        let m4b_str = m4b.to_str().expect("utf8");

        seed_file(&ctx, 100, 1, src_str, "mp3", 4).await;
        seed_file(&ctx, 200, 1, m4b_str, "m4b", 5).await;

        let policy = Policy {
            age_seconds: 0,
            force: false,
            apply: true,
        };
        let target = PostTranscodeSourcesTarget::new();
        let report = target.apply(&ctx, &policy).await.expect("apply");
        assert_eq!(report.items, 1);
        assert_eq!(report.bytes, 4);
        assert!(!src.exists(), "source file deleted from disk");
        assert!(m4b.exists(), "m4b output preserved");

        let is_active: i64 =
            sqlx::query_scalar("SELECT is_active FROM book_files WHERE file_id=100")
                .fetch_one(ctx.library.pool())
                .await
                .expect("query");
        assert_eq!(is_active, 0, "source row marked inactive");
    }

    #[tokio::test]
    async fn apply_is_idempotent() {
        let (ctx, tmp) = fresh().await;
        seed_book(&ctx, 1).await;
        let src = tmp.path().join("a.mp3");
        tokio::fs::write(&src, b"x").await.expect("write src");
        let src_str = src.to_str().expect("utf8");
        let m4b_str = "/tmp/never-touched.m4b";
        seed_file(&ctx, 100, 1, src_str, "mp3", 1).await;
        seed_file(&ctx, 200, 1, m4b_str, "m4b", 1).await;

        let policy = Policy {
            age_seconds: 0,
            force: false,
            apply: true,
        };
        let target = PostTranscodeSourcesTarget::new();
        let first = target.apply(&ctx, &policy).await.expect("first apply");
        assert_eq!(first.items, 1);
        let second = target.apply(&ctx, &policy).await.expect("second apply");
        assert_eq!(second.items, 0, "second pass is a no-op");
    }

    #[tokio::test]
    async fn apply_proceeds_when_file_already_gone() {
        let (ctx, _tmp) = fresh().await;
        seed_book(&ctx, 1).await;
        seed_file(&ctx, 100, 1, "/tmp/does-not-exist.mp3", "mp3", 999).await;
        seed_file(&ctx, 200, 1, "/tmp/m4b.m4b", "m4b", 999).await;

        let policy = Policy {
            age_seconds: 0,
            force: false,
            apply: true,
        };
        let target = PostTranscodeSourcesTarget::new();
        let report = target.apply(&ctx, &policy).await.expect("apply");
        // NotFound during unlink is OK — the DB row still moves
        // to is_active = 0 so the next pass is a no-op.
        assert_eq!(report.items, 1);
        let is_active: i64 =
            sqlx::query_scalar("SELECT is_active FROM book_files WHERE file_id=100")
                .fetch_one(ctx.library.pool())
                .await
                .expect("query");
        assert_eq!(is_active, 0);
    }
}
