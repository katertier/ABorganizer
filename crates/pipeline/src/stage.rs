//! Stage trait + per-stage context.

use std::sync::Arc;

use async_trait::async_trait;

use ab_core::{BookId, Result};
use ab_db::{EphemeralDb, LibraryDb};

/// Typed stage identifier.
///
/// Every pipeline stage exposes a `pub const STAGE_ID: StageId`
/// constant. [`Stage::requires`] returns `&'static [StageId]`,
/// so cross-stage dependencies are stored as the typed
/// identifier rather than the loose `&'static str` the old API
/// used. Renaming a stage now means changing its `STAGE_ID`
/// once; dependents either compile against the new symbol or
/// fail at compile time. The previous "rename a string and
/// silently break the DAG" failure mode is gone.
///
/// The wrapped string is the canonical name written to
/// `pipeline_progress.stage` and surfaced in `tracing` fields.
/// Convert with [`StageId::as_str`] / `Display` / `AsRef<str>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StageId(&'static str);

impl StageId {
    /// Construct from a static string. `const`, so stages can
    /// `pub const STAGE_ID: StageId = StageId::new("…")`.
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        Self(name)
    }

    /// The wrapped name as it lives in `pipeline_progress` /
    /// tracing / job submission.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl std::fmt::Display for StageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl AsRef<str> for StageId {
    fn as_ref(&self) -> &str {
        self.0
    }
}

/// What every stage gets at run time. Shared, cheap to clone.
#[derive(Clone)]
pub struct StageContext {
    /// Persistent library DB.
    pub library: LibraryDb,
    /// Restartable state DB.
    pub ephemeral: EphemeralDb,
    /// Stop-token; stages check `is_cancelled()` periodically.
    pub cancel: tokio_util::sync::CancellationToken,
    /// Stage name (passed in by the executor).
    pub stage_name: &'static str,
}

/// Outcome of a single stage invocation. We don't return heavy data —
/// the stage has already persisted it to storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageOutcome {
    /// Work completed; mark stage as succeeded for this book.
    Done,
    /// Stage was skipped (e.g., already complete on a re-run). Same
    /// as `Done` from the scheduler's perspective, but logged
    /// separately.
    Skipped,
    /// Resumable work pending: the stage made progress but isn't done
    /// yet. Stage runner re-queues for next iteration. Used by
    /// chunked transcription.
    Continue,
}

/// A pipeline stage. Implementations live in feature crates and are
/// registered in the daemon's wiring.
#[async_trait]
pub trait Stage: Send + Sync + 'static {
    /// Unique stage name. Used as a key in `pipeline_progress`.
    /// Typically `Self::STAGE_ID.as_str()` — each stage exposes a
    /// `pub const STAGE_ID: StageId` constant.
    fn name(&self) -> &'static str;

    /// Stages whose completion this one depends on. Empty
    /// vector means no dependencies (root stage). Returning
    /// [`StageId`]s (not free strings) means renaming a stage
    /// in one place propagates as a compile-time check at every
    /// dependent.
    fn requires(&self) -> &'static [StageId];

    /// Run the stage for one book.
    ///
    /// Heavy I/O is local to this method. The stage MUST persist all
    /// outputs to `ctx.library` / `ctx.ephemeral` / filesystem before
    /// returning. Anything held only in memory is lost on the next
    /// daemon restart.
    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome>;

    /// Roll back this stage's persistent output for one book so a
    /// subsequent [`Self::run`] starts from a clean slate. Called
    /// by the `aborg book retry --stage <s>[,<s>...|all]` flow
    /// (slice H.1.6, ADR-0023) before the retry submission so the
    /// stage doesn't see its own stale cache.
    ///
    /// # Default behaviour
    ///
    /// The default implementation clears the two storage tiers
    /// every stage participates in:
    ///
    /// 1. **`ai_cache` rows** matching `(book_id, cache_type)`
    ///    for every `CacheKey` that
    ///    [`ab_core::cache_keys_for_stage`] returns for this
    ///    stage. Stages with no `ai_cache` output (the bulk of
    ///    the catalog / identity / consensus side) are a no-op
    ///    here.
    /// 2. **`pipeline_progress`** row matching
    ///    `(book_id, stage.name())`. The dispatcher's eligibility
    ///    sweep treats "no row" as `'pending'` and re-submits.
    ///
    /// # Per-stage overrides
    ///
    /// Stages whose outputs aren't captured by the default
    /// (audiologo-detect's `book_file_audiologos` rows;
    /// tag-read's `book_field_provenance` rows; etc.) override
    /// to extend or replace the cleanup. **Calling the default
    /// from an override is the recommended pattern** — extend,
    /// don't replace:
    ///
    /// ```rust,ignore
    /// async fn reset(&self, ctx: &StageContext, book_id: BookId) -> Result<()> {
    ///     // Stage-specific extras...
    ///     stage_specific_cleanup(ctx, book_id).await?;
    ///     // ...then fall through to the cache + progress wipe.
    ///     ab_pipeline::stage::default_reset(self.name(), ctx, book_id).await
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// Surfaces underlying DB errors as
    /// [`ab_core::Error::Database`]. The H.1.6 retry endpoint
    /// maps these to 500s; a stage's `reset` that itself
    /// failed leaves the book in a half-reset state, which
    /// the operator can recover by re-running the retry once
    /// the underlying DB issue clears.
    async fn reset(&self, ctx: &StageContext, book_id: BookId) -> Result<()> {
        default_reset(self.name(), ctx, book_id).await
    }
}

/// Default body of [`Stage::reset`]. Exposed as a free
/// function so per-stage overrides can call it after their
/// stage-specific cleanup, in the spirit of "extend, don't
/// replace."
///
/// Clears three storage tiers, all keyed by `(book_id, stage)`:
///
/// - `library.ai_cache` rows for every `CacheKey` returned by
///   [`ab_core::cache_keys_for_stage`] for this stage. Most
///   non-extractor stages return `None` from that lookup, so
///   they no-op this branch.
/// - `library.book_field_provenance` rows where
///   `stage = self.name()` (slice H.1.2 made this column
///   exact; pre-H.1 we couldn't selectively clear here).
/// - `ephemeral.pipeline_progress` row for `(book_id, stage)`,
///   so the dispatcher's eligibility sweep sees the work as
///   pending again.
///
/// # Errors
///
/// See [`Stage::reset`].
pub async fn default_reset(stage: &str, ctx: &StageContext, book_id: BookId) -> Result<()> {
    let id = book_id.0;

    // ── library.ai_cache ─────────────────────────────────────
    if let Some(keys) = ab_core::cache_keys_for_stage(stage) {
        for key in keys {
            let cache_type = key.as_str();
            sqlx::query!(
                "DELETE FROM ai_cache WHERE book_id = ? AND cache_type = ?",
                id,
                cache_type,
            )
            .execute(ctx.library.pool())
            .await
            .map_err(|e| {
                ab_core::Error::Database(format!("reset clear ai_cache {cache_type}: {e}"))
            })?;
        }
    }

    // ── library.book_field_provenance ────────────────────────
    // Stages without provenance writes simply match zero rows.
    sqlx::query!(
        "DELETE FROM book_field_provenance WHERE book_id = ? AND stage = ?",
        id,
        stage,
    )
    .execute(ctx.library.pool())
    .await
    .map_err(|e| ab_core::Error::Database(format!("reset clear provenance: {e}")))?;

    // ── ephemeral.pipeline_progress ──────────────────────────
    sqlx::query!(
        "DELETE FROM pipeline_progress WHERE book_id = ? AND stage = ?",
        id,
        stage,
    )
    .execute(ctx.ephemeral.pool())
    .await
    .map_err(|e| ab_core::Error::Database(format!("reset clear progress: {e}")))?;

    Ok(())
}

/// Type-erased stage registration record.
pub(crate) struct StageEntry {
    pub(crate) stage: Arc<dyn Stage>,
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use ab_core::tunables::DbTunables;
    use ab_db::{EphemeralDb, LibraryDb};
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use super::*;

    async fn fresh_ctx() -> (StageContext, TempDir) {
        let tmp = TempDir::new().expect("tmpdir");
        let lib = LibraryDb::open(&tmp.path().join("library.db"), &DbTunables::default())
            .await
            .expect("open library");
        let eph = EphemeralDb::open(&tmp.path().join("ephemeral.db"), &DbTunables::default())
            .await
            .expect("open ephemeral");
        let ctx = StageContext {
            library: lib,
            ephemeral: eph,
            cancel: CancellationToken::new(),
            stage_name: "",
        };
        (ctx, tmp)
    }

    async fn insert_book(ctx: &StageContext, title: &str) -> i64 {
        sqlx::query_scalar::<_, i64>("INSERT INTO books (title) VALUES (?) RETURNING book_id")
            .bind(title)
            .fetch_one(ctx.library.pool())
            .await
            .expect("insert book")
    }

    #[tokio::test]
    async fn default_reset_clears_ai_cache_book_field_provenance_and_progress() {
        let (ctx, _tmp) = fresh_ctx().await;
        let book_id = insert_book(&ctx, "fixture").await;

        // Seed: ai_cache row for `extract-dna-tags`, a
        // book_field_provenance row owned by `extract-dna-tags`,
        // and a pipeline_progress row. Also seed rows owned by
        // a DIFFERENT stage so we can confirm the reset is
        // scoped.
        sqlx::query(
            "INSERT INTO ai_cache (book_id, cache_type, content, extractor_version) \
             VALUES (?, 'dna_tags', X'2020', 'fm-26-v1'), \
                    (?, 'transcript_head', X'2020', 'fm-26-v1')",
        )
        .bind(book_id)
        .bind(book_id)
        .execute(ctx.library.pool())
        .await
        .expect("seed cache");

        sqlx::query(
            "INSERT INTO book_field_provenance \
             (book_id, field, value, source, stage, confidence) VALUES \
               (?, 'genre', 'fantasy', 'dna-text', 'extract-dna-tags', 0.8), \
               (?, 'author', 'X',       'tag_file', 'tag-read',        0.7)",
        )
        .bind(book_id)
        .bind(book_id)
        .execute(ctx.library.pool())
        .await
        .expect("seed provenance");

        sqlx::query(
            "INSERT INTO pipeline_progress (book_id, stage, status) VALUES \
               (?, 'extract-dna-tags', 'succeeded'), \
               (?, 'tag-read',         'succeeded')",
        )
        .bind(book_id)
        .bind(book_id)
        .execute(ctx.ephemeral.pool())
        .await
        .expect("seed progress");

        // Reset only `extract-dna-tags`.
        default_reset("extract-dna-tags", &ctx, BookId(book_id))
            .await
            .expect("reset");

        // ai_cache: dna_tags gone, transcript_head untouched.
        let cache_rows: Vec<String> = sqlx::query_scalar(
            "SELECT cache_type FROM ai_cache WHERE book_id = ? ORDER BY cache_type",
        )
        .bind(book_id)
        .fetch_all(ctx.library.pool())
        .await
        .expect("cache rows");
        assert_eq!(cache_rows, vec!["transcript_head".to_owned()]);

        // book_field_provenance: dna_tags row gone, tag-read row
        // untouched.
        let prov_stages: Vec<String> = sqlx::query_scalar(
            "SELECT stage FROM book_field_provenance WHERE book_id = ? ORDER BY stage",
        )
        .bind(book_id)
        .fetch_all(ctx.library.pool())
        .await
        .expect("prov rows");
        assert_eq!(prov_stages, vec!["tag-read".to_owned()]);

        // pipeline_progress: extract-dna-tags row gone, tag-read
        // row untouched.
        let pp_stages: Vec<String> = sqlx::query_scalar(
            "SELECT stage FROM pipeline_progress WHERE book_id = ? ORDER BY stage",
        )
        .bind(book_id)
        .fetch_all(ctx.ephemeral.pool())
        .await
        .expect("progress rows");
        assert_eq!(pp_stages, vec!["tag-read".to_owned()]);
    }

    #[tokio::test]
    async fn default_reset_is_idempotent_on_clean_state() {
        // Reset a stage that has no rows. Must not error.
        let (ctx, _tmp) = fresh_ctx().await;
        let book_id = insert_book(&ctx, "empty").await;
        default_reset("extract-dna-tags", &ctx, BookId(book_id))
            .await
            .expect("reset on empty state");
        // Second call: still fine.
        default_reset("extract-dna-tags", &ctx, BookId(book_id))
            .await
            .expect("reset twice");
    }

    #[tokio::test]
    async fn default_reset_noop_for_stages_with_no_ai_cache_keys() {
        // `tag-read` has no `ai_cache` keys but does have a
        // pipeline_progress row + book_field_provenance rows.
        // The default reset must still wipe those, just skip
        // the ai_cache branch.
        let (ctx, _tmp) = fresh_ctx().await;
        let book_id = insert_book(&ctx, "tag-only").await;

        sqlx::query(
            "INSERT INTO book_field_provenance \
             (book_id, field, value, source, stage, confidence) \
             VALUES (?, 'title', 'X', 'tag_file', 'tag-read', 0.7)",
        )
        .bind(book_id)
        .execute(ctx.library.pool())
        .await
        .expect("seed");
        sqlx::query(
            "INSERT INTO pipeline_progress (book_id, stage, status) \
             VALUES (?, 'tag-read', 'succeeded')",
        )
        .bind(book_id)
        .execute(ctx.ephemeral.pool())
        .await
        .expect("seed progress");

        default_reset("tag-read", &ctx, BookId(book_id))
            .await
            .expect("reset");

        let prov: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM book_field_provenance WHERE book_id = ?")
                .bind(book_id)
                .fetch_one(ctx.library.pool())
                .await
                .expect("count");
        assert_eq!(prov, 0);
        let progress: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM pipeline_progress WHERE book_id = ?")
                .bind(book_id)
                .fetch_one(ctx.ephemeral.pool())
                .await
                .expect("count");
        assert_eq!(progress, 0);
    }
}
