//! Chapter source winner picker.
//!
//! Two stages (`fetch-audnexus-chapters` and `read-embedded-chapters`) can
//! both populate `chapters` for the same book — Audnexus when
//! Audible has a `ToC` for the book's ASIN, embedded when the M4B
//! files carry `chpl` or chapter-track atoms. Without a tiebreaker
//! the API's chapter endpoint would return both stacks and the
//! player would render duplicates.
//!
//! This stage marks exactly one source's chapter rows for each
//! book as `is_winner = 1`, all others `= 0`. The API queries
//! `WHERE is_winner = 1` to surface a single coherent `ToC`.
//!
//! # Precedence
//!
//! 1. **`audnexus`** — Audible's authoritative `ToC`, hand-curated
//!    by the Audnexus volunteers, includes brand intro/outro
//!    durations the embedded atoms lack.
//! 2. **`embedded`** — chpl / chapter-track atoms shipped with
//!    the M4B. Reliable for books that have them; many indie
//!    audiobooks ship without.
//! 3. Future sources (`cue`, `epub`, `transcript`, `silence`)
//!    slot in below in that order — explicit `ToC`s (cue/epub)
//!    beat synthesized ones (transcript / silence-derived).
//!
//! # Idempotency
//!
//! Re-running converges: same input → same winner → same flag
//! state. Each run sets `is_winner` for every row of the book,
//! so a fresh source landing later (e.g. an Audnexus re-fetch)
//! moves the flag without leaving stale winners.

use async_trait::async_trait;

use ab_core::{BookId, Error, Result};
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};

/// Source precedence — highest priority first. The first source
/// that has any chapter rows for the book wins.
const SOURCE_PRECEDENCE: &[&str] = &[
    "audnexus",
    "embedded",
    "cue",
    "epub",
    "transcript",
    "silence",
];

/// Stage that picks one chapter source per book and flags its
/// rows as winners.
pub struct ChapterWinnerStage;

impl ChapterWinnerStage {
    /// Construct. No tunables — the precedence is structural and
    /// the same for every library.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for ChapterWinnerStage {
    fn default() -> Self {
        Self::new()
    }
}

/// Typed identifier for this stage.
pub const STAGE_ID: StageId = StageId::new("pick-chapter-winner");

#[async_trait]
impl Stage for ChapterWinnerStage {
    fn name(&self) -> &'static str {
        STAGE_ID.as_str()
    }

    fn requires(&self) -> &'static [StageId] {
        // Both chapter-writing stages need to have finished
        // (regardless of whether they each found anything) before
        // we know what sources are available to choose between.
        &[
            crate::chapters::STAGE_ID,
            crate::embedded_chapters::STAGE_ID,
        ]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        let sources_present = fetch_present_sources(&ctx.library, book_id).await?;
        if sources_present.is_empty() {
            return Ok(StageOutcome::Skipped);
        }

        let winner = SOURCE_PRECEDENCE
            .iter()
            .copied()
            .find(|s| sources_present.iter().any(|p| p == s));
        let Some(winner) = winner else {
            // Sources present but none match the known precedence
            // list (e.g. an experimental future source). Don't
            // pick anything — better to surface no chapters than
            // surface arbitrary ones.
            tracing::warn!(
                book = %book_id,
                ?sources_present,
                "chapter.winner.no_known_source"
            );
            return Ok(StageOutcome::Skipped);
        };

        apply_winner(&ctx.library, book_id, winner).await?;
        tracing::info!(
            book = %book_id,
            winner = %winner,
            sources_present = sources_present.len(),
            "chapter.winner.done"
        );
        Ok(StageOutcome::Done)
    }
}

/// Distinct `source` values currently present in `chapters` for
/// this book.
async fn fetch_present_sources(library: &ab_db::LibraryDb, book_id: BookId) -> Result<Vec<String>> {
    let id = book_id.0;
    let rows = sqlx::query!("SELECT DISTINCT source FROM chapters WHERE book_id = ?", id,)
        .fetch_all(library.pool())
        .await
        .map_err(|e| Error::Database(format!("chapter winner present-sources: {e}")))?;
    Ok(rows.into_iter().map(|r| r.source).collect())
}

/// Flip `is_winner` so exactly the rows where `source = winner`
/// are flagged, in one transaction. Two queries (set losers,
/// then set winners) keeps the path inside the closed allowlist
/// of literal-string queries the compile-time-checked macros
/// accept.
async fn apply_winner(library: &ab_db::LibraryDb, book_id: BookId, winner: &str) -> Result<()> {
    let id = book_id.0;
    let mut tx = library
        .pool()
        .begin()
        .await
        .map_err(|e| Error::Database(format!("chapter winner tx begin: {e}")))?;

    sqlx::query!(
        "UPDATE chapters SET is_winner = 0 WHERE book_id = ? AND source != ?",
        id,
        winner,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| Error::Database(format!("chapter winner clear losers: {e}")))?;
    sqlx::query!(
        "UPDATE chapters SET is_winner = 1 WHERE book_id = ? AND source = ?",
        id,
        winner,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| Error::Database(format!("chapter winner set winners: {e}")))?;

    tx.commit()
        .await
        .map_err(|e| Error::Database(format!("chapter winner tx commit: {e}")))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use ab_core::tunables::DbTunables;
    use tempfile::TempDir;

    use super::*;

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
            stage_name: "pick-chapter-winner",
        }
    }

    async fn seed_chapters(db: &ab_db::LibraryDb, book_id: i64, rows: &[(i64, &str, &str)]) {
        sqlx::query("INSERT OR IGNORE INTO books (book_id, title) VALUES (?, 'fixture')")
            .bind(book_id)
            .execute(db.pool())
            .await
            .expect("seed book");
        for (idx, title, source) in rows {
            sqlx::query(
                "INSERT INTO chapters (book_id, idx, start_ms, end_ms, title, source) \
                 VALUES (?, ?, 0, 1000, ?, ?)",
            )
            .bind(book_id)
            .bind(idx)
            .bind(title)
            .bind(source)
            .execute(db.pool())
            .await
            .expect("seed chapter");
        }
    }

    #[tokio::test]
    async fn stage_metadata_matches_pipeline_expectations() {
        let stage = ChapterWinnerStage::new();
        assert_eq!(stage.name(), "pick-chapter-winner");
        assert_eq!(
            stage.requires(),
            &[
                crate::chapters::STAGE_ID,
                crate::embedded_chapters::STAGE_ID,
            ]
        );
    }

    #[tokio::test]
    async fn audnexus_beats_embedded() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        seed_chapters(
            &ctx.library,
            1,
            &[
                (0, "audnexus-1", "audnexus"),
                (0, "embedded-1", "embedded"),
                (1, "embedded-2", "embedded"),
            ],
        )
        .await;

        ChapterWinnerStage::new()
            .run(&ctx, BookId(1))
            .await
            .expect("run");

        let winners: Vec<(String, i64)> = sqlx::query_as(
            "SELECT source, is_winner FROM chapters WHERE book_id = 1 ORDER BY source, idx",
        )
        .fetch_all(ctx.library.pool())
        .await
        .expect("read");
        // audnexus row is the only winner; both embedded rows lose.
        let winners_only: Vec<(String, i64)> =
            winners.iter().filter(|(_, w)| *w == 1).cloned().collect();
        assert_eq!(winners_only.len(), 1);
        assert_eq!(winners_only[0].0, "audnexus");
    }

    #[tokio::test]
    async fn embedded_wins_when_audnexus_absent() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        seed_chapters(
            &ctx.library,
            1,
            &[(0, "embedded-1", "embedded"), (1, "embedded-2", "embedded")],
        )
        .await;

        ChapterWinnerStage::new()
            .run(&ctx, BookId(1))
            .await
            .expect("run");

        let winner_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM chapters WHERE book_id = 1 AND is_winner = 1")
                .fetch_one(ctx.library.pool())
                .await
                .expect("count");
        assert_eq!(winner_count, 2, "both embedded rows are winners");
    }

    #[tokio::test]
    async fn rerun_moves_winner_when_new_source_lands() {
        // Start with embedded only — embedded wins.
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        seed_chapters(&ctx.library, 1, &[(0, "embedded-1", "embedded")]).await;
        ChapterWinnerStage::new()
            .run(&ctx, BookId(1))
            .await
            .expect("run 1");
        let initial: String = sqlx::query_scalar(
            "SELECT source FROM chapters WHERE book_id = 1 AND is_winner = 1 LIMIT 1",
        )
        .fetch_one(ctx.library.pool())
        .await
        .expect("read initial");
        assert_eq!(initial, "embedded");

        // Audnexus arrives later → embedded must yield.
        seed_chapters(&ctx.library, 1, &[(0, "audnexus-1", "audnexus")]).await;
        ChapterWinnerStage::new()
            .run(&ctx, BookId(1))
            .await
            .expect("run 2");

        let after: Vec<(String, i64)> =
            sqlx::query_as("SELECT source, is_winner FROM chapters WHERE book_id = 1")
                .fetch_all(ctx.library.pool())
                .await
                .expect("read after");
        for (source, is_winner) in &after {
            if source == "audnexus" {
                assert_eq!(*is_winner, 1, "audnexus wins now");
            } else {
                assert_eq!(*is_winner, 0, "{source} now loses");
            }
        }
    }

    #[tokio::test]
    async fn skips_when_no_chapters_present() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'no-chapters')")
            .execute(ctx.library.pool())
            .await
            .expect("seed");

        let outcome = ChapterWinnerStage::new()
            .run(&ctx, BookId(1))
            .await
            .expect("run");
        assert_eq!(outcome, StageOutcome::Skipped);
    }
}
