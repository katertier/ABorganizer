//! EPUB chapter-title fallback stage.
//!
//! Embedded chapter atoms in commercial audiobooks frequently
//! carry placeholder titles like `Chapter 1` / `Chapter 2` (or
//! even pure numerals `1`, `2`, …) — useful as anchors but
//! useless to the reader who wants to see "The Way of Kings" /
//! "The Wind and the Rain" in the chapter list. When a paired
//! EPUB companion exists with a parseable nav-doc / NCX and the
//! count matches the winning chapter source, those richer titles
//! can be propagated onto the winner without changing the source
//! precedence.
//!
//! ## When this fires
//!
//! Runs after `pick-chapter-winner`. No-op when:
//!
//! * The winner source is already `epub` (titles already came
//!   from the EPUB).
//! * The winner has no rows OR no EPUB rows exist for the book.
//! * Winner-row count ≠ EPUB-row count (alignment heuristic
//!   "TBD at slice-start" per BACKLOG; we ship the count-match
//!   path first — partial-match alignment can layer on later
//!   without rewriting the winner side).
//! * Fewer than half of the winner-row titles look placeholder-y
//!   (heuristic: `is_placeholder_title`). One or two real titles
//!   in the mix means the embedded titles aren't blanket noise;
//!   we leave them alone rather than risk overwriting good data.
//!
//! ## What the placeholder check accepts
//!
//! * `NULL` / empty / whitespace
//! * Pure numerals (`1`, `12`, `  03  `)
//! * `Chapter N`, `Ch N`, `Ch. N`, `Track N`, `Part N`, `Pt N`
//!   (case-insensitive, optional trailing punctuation)
//!
//! Anything else is treated as a real title.
//!
//! ## Idempotency
//!
//! Updates `chapters.title` in-place for the winner rows only.
//! Re-running with no new input is a no-op (the new titles are
//! no longer placeholder-y). Re-running after an EPUB swap
//! re-propagates from the latest EPUB rows.

use async_trait::async_trait;

use ab_core::{BookId, Error, Result};
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};

/// Stage that copies EPUB chapter titles onto the winner rows
/// when the winner source carries placeholder titles.
pub struct EpubTitleFallbackStage;

impl EpubTitleFallbackStage {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for EpubTitleFallbackStage {
    fn default() -> Self {
        Self::new()
    }
}

/// Typed identifier — used in `requires()` graphs and the doctor
/// stage registry.
pub const STAGE_ID: StageId = StageId::new("enrich-chapter-titles-from-epub");

#[async_trait]
impl Stage for EpubTitleFallbackStage {
    fn name(&self) -> &'static str {
        STAGE_ID.as_str()
    }

    fn requires(&self) -> &'static [StageId] {
        // Need the winner picked + the EPUB rows written, so this
        // sits after both. `pick-chapter-winner` already depends
        // on `read-epub-chapters`, so requiring the winner is
        // transitively enough — listed explicitly for clarity.
        &[crate::chapter_winner::STAGE_ID]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        let winner = fetch_winner_rows(&ctx.library, book_id).await?;
        if winner.is_empty() {
            return Ok(StageOutcome::Skipped);
        }
        if winner.first().is_some_and(|r| r.source == "epub") {
            // EPUB itself is the winner — titles already authoritative.
            return Ok(StageOutcome::Skipped);
        }
        let epub = fetch_epub_rows(&ctx.library, book_id).await?;
        if epub.is_empty() {
            return Ok(StageOutcome::Skipped);
        }
        if winner.len() != epub.len() {
            tracing::debug!(
                book = %book_id,
                winner = winner.len(),
                epub = epub.len(),
                "epub_title_fallback.count_mismatch"
            );
            return Ok(StageOutcome::Skipped);
        }
        let placeholder_count = winner
            .iter()
            .filter(|r| is_placeholder_title(r.title.as_deref()))
            .count();
        if placeholder_count * 2 < winner.len() {
            tracing::debug!(
                book = %book_id,
                placeholder = placeholder_count,
                total = winner.len(),
                "epub_title_fallback.titles_look_real"
            );
            return Ok(StageOutcome::Skipped);
        }

        let mut tx = ctx
            .library
            .pool()
            .begin()
            .await
            .map_err(|e| Error::Database(format!("epub_title_fallback tx begin: {e}")))?;
        let mut updated = 0usize;
        for (w, e) in winner.iter().zip(epub.iter()) {
            if !is_placeholder_title(w.title.as_deref()) {
                continue;
            }
            let new_title = e.title.as_deref().unwrap_or("");
            if new_title.is_empty() {
                continue;
            }
            sqlx::query!(
                "UPDATE chapters SET title = ? WHERE chapter_id = ?",
                new_title,
                w.chapter_id,
            )
            .execute(&mut *tx)
            .await
            .map_err(|err| Error::Database(format!("epub_title_fallback update: {err}")))?;
            updated += 1;
        }
        tx.commit()
            .await
            .map_err(|e| Error::Database(format!("epub_title_fallback tx commit: {e}")))?;

        if updated == 0 {
            return Ok(StageOutcome::Skipped);
        }
        tracing::info!(
            book = %book_id,
            updated,
            winner_source = %winner[0].source,
            "epub_title_fallback.done"
        );
        Ok(StageOutcome::Done)
    }
}

/// Placeholder-title heuristic. Returns true for the common
/// "needs replacing" shapes — NULL, empty, numeric-only,
/// "Chapter N", "Track N", "Part N" variants.
fn is_placeholder_title(title: Option<&str>) -> bool {
    let Some(t) = title else {
        return true;
    };
    let trimmed = t.trim();
    if trimmed.is_empty() {
        return true;
    }
    // Pure number (allowing leading zeros): `1`, `03`, `12`.
    if trimmed.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    // `Chapter N` / `Ch N` / `Ch. N` / `Track N` / `Part N` / `Pt N`.
    let lower = trimmed.to_lowercase();
    let stripped = lower.trim_end_matches(['.', ':']);
    for prefix in ["chapter ", "ch ", "ch. ", "track ", "part ", "pt ", "pt. "] {
        if let Some(rest) = stripped.strip_prefix(prefix) {
            let n = rest.trim();
            if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
                return true;
            }
        }
    }
    false
}

struct WinnerRow {
    chapter_id: i64,
    title: Option<String>,
    source: String,
}

struct EpubRow {
    title: Option<String>,
}

async fn fetch_winner_rows(library: &ab_db::LibraryDb, book_id: BookId) -> Result<Vec<WinnerRow>> {
    let id = book_id.0;
    let rows = sqlx::query!(
        r#"SELECT chapter_id AS "chapter_id!: i64",
                  title      AS "title?: String",
                  source     AS "source!: String"
             FROM chapters
            WHERE book_id = ? AND is_winner = 1
            ORDER BY idx"#,
        id,
    )
    .fetch_all(library.pool())
    .await
    .map_err(|e| Error::Database(format!("epub_title_fallback winner rows: {e}")))?;
    Ok(rows
        .into_iter()
        .map(|r| WinnerRow {
            chapter_id: r.chapter_id,
            title: r.title,
            source: r.source,
        })
        .collect())
}

async fn fetch_epub_rows(library: &ab_db::LibraryDb, book_id: BookId) -> Result<Vec<EpubRow>> {
    let id = book_id.0;
    let rows = sqlx::query!(
        r#"SELECT title AS "title?: String"
             FROM chapters
            WHERE book_id = ? AND source = 'epub'
            ORDER BY idx"#,
        id,
    )
    .fetch_all(library.pool())
    .await
    .map_err(|e| Error::Database(format!("epub_title_fallback epub rows: {e}")))?;
    Ok(rows
        .into_iter()
        .map(|r| EpubRow { title: r.title })
        .collect())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
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
            stage_name: STAGE_ID.as_str(),
        }
    }

    async fn seed_book(ctx: &StageContext) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "INSERT INTO books (title, duration_ms, raw_duration_ms) \
             VALUES ('Test', 0, 0) RETURNING book_id",
        )
        .fetch_one(ctx.library.pool())
        .await
        .expect("insert book")
    }

    struct Row<'a> {
        book_id: i64,
        idx: i64,
        title: Option<&'a str>,
        source: &'a str,
        is_winner: i64,
    }

    async fn insert_chapter(ctx: &StageContext, row: Row<'_>) {
        let Row {
            book_id,
            idx,
            title,
            source,
            is_winner,
        } = row;
        sqlx::query(
            "INSERT INTO chapters (book_id, idx, start_ms, end_ms, title, source, is_winner) \
             VALUES (?, ?, 0, 0, ?, ?, ?)",
        )
        .bind(book_id)
        .bind(idx)
        .bind(title)
        .bind(source)
        .bind(is_winner)
        .execute(ctx.library.pool())
        .await
        .expect("insert chapter");
    }

    async fn fetch_winner_titles(ctx: &StageContext, book_id: i64) -> Vec<Option<String>> {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT title FROM chapters WHERE book_id = ? AND is_winner = 1 ORDER BY idx",
        )
        .bind(book_id)
        .fetch_all(ctx.library.pool())
        .await
        .expect("fetch")
    }

    #[test]
    fn placeholder_titles_recognised() {
        assert!(is_placeholder_title(None));
        assert!(is_placeholder_title(Some("")));
        assert!(is_placeholder_title(Some("   ")));
        assert!(is_placeholder_title(Some("1")));
        assert!(is_placeholder_title(Some("12")));
        assert!(is_placeholder_title(Some("  03  ")));
        assert!(is_placeholder_title(Some("Chapter 1")));
        assert!(is_placeholder_title(Some("chapter 12")));
        assert!(is_placeholder_title(Some("Ch 4")));
        assert!(is_placeholder_title(Some("Ch. 5")));
        assert!(is_placeholder_title(Some("Track 7")));
        assert!(is_placeholder_title(Some("Part 2")));
        assert!(is_placeholder_title(Some("Pt 9")));
        assert!(is_placeholder_title(Some("Chapter 1.")));
    }

    #[test]
    fn real_titles_not_placeholder() {
        assert!(!is_placeholder_title(Some("The Way of Kings")));
        assert!(!is_placeholder_title(Some("Prologue")));
        assert!(!is_placeholder_title(Some("Chapter Five: The Storm")));
        // Numbered with a real title — not a placeholder.
        assert!(!is_placeholder_title(Some("1. The Awakening")));
        assert!(!is_placeholder_title(Some("Chapter 1 - Beginning")));
    }

    #[tokio::test]
    async fn propagates_titles_when_winner_is_placeholders_and_count_matches() {
        let tmp = TempDir::new().expect("tmp");
        let ctx = fresh_ctx(tmp.path()).await;
        let book_id = seed_book(&ctx).await;
        for (idx, title, source, is_winner) in [
            (0, Some("Chapter 1"), "embedded", 1),
            (1, Some("Chapter 2"), "embedded", 1),
            (2, Some("Chapter 3"), "embedded", 1),
            (0, Some("The Storm"), "epub", 0),
            (1, Some("The Wind"), "epub", 0),
            (2, Some("The Rain"), "epub", 0),
        ] {
            insert_chapter(
                &ctx,
                Row {
                    book_id,
                    idx,
                    title,
                    source,
                    is_winner,
                },
            )
            .await;
        }

        let outcome = EpubTitleFallbackStage::new()
            .run(&ctx, BookId(book_id))
            .await
            .expect("run");
        assert_eq!(outcome, StageOutcome::Done);

        let titles = fetch_winner_titles(&ctx, book_id).await;
        assert_eq!(
            titles,
            vec![
                Some("The Storm".to_owned()),
                Some("The Wind".to_owned()),
                Some("The Rain".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn skips_when_winner_is_epub() {
        let tmp = TempDir::new().expect("tmp");
        let ctx = fresh_ctx(tmp.path()).await;
        let book_id = seed_book(&ctx).await;
        for (idx, title) in [(0_i64, "One"), (1, "Two")] {
            insert_chapter(
                &ctx,
                Row {
                    book_id,
                    idx,
                    title: Some(title),
                    source: "epub",
                    is_winner: 1,
                },
            )
            .await;
        }

        let outcome = EpubTitleFallbackStage::new()
            .run(&ctx, BookId(book_id))
            .await
            .expect("run");
        assert_eq!(outcome, StageOutcome::Skipped);
    }

    #[tokio::test]
    async fn skips_when_no_epub_rows() {
        let tmp = TempDir::new().expect("tmp");
        let ctx = fresh_ctx(tmp.path()).await;
        let book_id = seed_book(&ctx).await;
        for (idx, title) in [(0_i64, "Chapter 1"), (1, "Chapter 2")] {
            insert_chapter(
                &ctx,
                Row {
                    book_id,
                    idx,
                    title: Some(title),
                    source: "embedded",
                    is_winner: 1,
                },
            )
            .await;
        }

        let outcome = EpubTitleFallbackStage::new()
            .run(&ctx, BookId(book_id))
            .await
            .expect("run");
        assert_eq!(outcome, StageOutcome::Skipped);
    }

    #[tokio::test]
    async fn skips_when_counts_mismatch() {
        let tmp = TempDir::new().expect("tmp");
        let ctx = fresh_ctx(tmp.path()).await;
        let book_id = seed_book(&ctx).await;
        // Three EPUB rows vs two winner rows — count-match alignment
        // fails; partial-match path is deferred to a later slice.
        for (idx, title, source, is_winner) in [
            (0, Some("Chapter 1"), "embedded", 1),
            (1, Some("Chapter 2"), "embedded", 1),
            (0, Some("A"), "epub", 0),
            (1, Some("B"), "epub", 0),
            (2, Some("C"), "epub", 0),
        ] {
            insert_chapter(
                &ctx,
                Row {
                    book_id,
                    idx,
                    title,
                    source,
                    is_winner,
                },
            )
            .await;
        }

        let outcome = EpubTitleFallbackStage::new()
            .run(&ctx, BookId(book_id))
            .await
            .expect("run");
        assert_eq!(outcome, StageOutcome::Skipped);

        let titles = fetch_winner_titles(&ctx, book_id).await;
        assert_eq!(
            titles,
            vec![Some("Chapter 1".to_owned()), Some("Chapter 2".to_owned())]
        );
    }

    #[tokio::test]
    async fn skips_when_winner_titles_look_real() {
        let tmp = TempDir::new().expect("tmp");
        let ctx = fresh_ctx(tmp.path()).await;
        let book_id = seed_book(&ctx).await;
        for (idx, title, source, is_winner) in [
            (0, Some("The Storm"), "embedded", 1),
            (1, Some("The Wind"), "embedded", 1),
            (2, Some("Chapter 3"), "embedded", 1),
            (0, Some("X"), "epub", 0),
            (1, Some("Y"), "epub", 0),
            (2, Some("Z"), "epub", 0),
        ] {
            insert_chapter(
                &ctx,
                Row {
                    book_id,
                    idx,
                    title,
                    source,
                    is_winner,
                },
            )
            .await;
        }

        let outcome = EpubTitleFallbackStage::new()
            .run(&ctx, BookId(book_id))
            .await
            .expect("run");
        // Only 1/3 placeholder — under the half threshold, leave alone.
        assert_eq!(outcome, StageOutcome::Skipped);
        let titles = fetch_winner_titles(&ctx, book_id).await;
        assert_eq!(
            titles,
            vec![
                Some("The Storm".to_owned()),
                Some("The Wind".to_owned()),
                Some("Chapter 3".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn skips_blank_epub_titles_during_propagation() {
        let tmp = TempDir::new().expect("tmp");
        let ctx = fresh_ctx(tmp.path()).await;
        let book_id = seed_book(&ctx).await;
        for (idx, title, source, is_winner) in [
            (0, Some("Chapter 1"), "embedded", 1),
            (1, Some("Chapter 2"), "embedded", 1),
            (0, Some(""), "epub", 0),
            (1, Some("The Wind"), "epub", 0),
        ] {
            insert_chapter(
                &ctx,
                Row {
                    book_id,
                    idx,
                    title,
                    source,
                    is_winner,
                },
            )
            .await;
        }

        let outcome = EpubTitleFallbackStage::new()
            .run(&ctx, BookId(book_id))
            .await
            .expect("run");
        assert_eq!(outcome, StageOutcome::Done);
        let titles = fetch_winner_titles(&ctx, book_id).await;
        // First chapter stays "Chapter 1" because EPUB title is blank.
        assert_eq!(
            titles,
            vec![Some("Chapter 1".to_owned()), Some("The Wind".to_owned())]
        );
    }

    #[tokio::test]
    async fn rerun_is_idempotent_after_titles_propagated() {
        let tmp = TempDir::new().expect("tmp");
        let ctx = fresh_ctx(tmp.path()).await;
        let book_id = seed_book(&ctx).await;
        for (idx, title, source, is_winner) in [
            (0, Some("Chapter 1"), "embedded", 1),
            (1, Some("Chapter 2"), "embedded", 1),
            (0, Some("Alpha"), "epub", 0),
            (1, Some("Beta"), "epub", 0),
        ] {
            insert_chapter(
                &ctx,
                Row {
                    book_id,
                    idx,
                    title,
                    source,
                    is_winner,
                },
            )
            .await;
        }

        let stage = EpubTitleFallbackStage::new();
        assert_eq!(
            stage.run(&ctx, BookId(book_id)).await.expect("first"),
            StageOutcome::Done
        );
        // Second run: titles no longer placeholder → skipped.
        assert_eq!(
            stage.run(&ctx, BookId(book_id)).await.expect("second"),
            StageOutcome::Skipped
        );
        let titles = fetch_winner_titles(&ctx, book_id).await;
        assert_eq!(
            titles,
            vec![Some("Alpha".to_owned()), Some("Beta".to_owned())]
        );
    }
}
