//! Audnexus chapters stage.
//!
//! Fetches `GET /books/{asin}/chapters` for each enriched book,
//! walks the configured region order until a hit, persists the
//! returned chapter list to the `chapters` table (`source =
//! 'audnexus'`), and writes the brand intro / outro durations to
//! `books.audiologo_intro_ms` / `audiologo_outro_ms` so the
//! player can skip publisher jingles before the audiologo-trim
//! stage exists.
//!
//! # Source-of-truth ordering
//!
//! Chapters are keyed `(book_id, idx)` (UNIQUE) and `(book_id,
//! source)` taken together gives a recoverable identity per
//! source. This stage owns rows where `source = 'audnexus'` and
//! clears-then-inserts them on each run (idempotent). Other
//! sources (`embedded`, `cue`, `epub`, `transcript`, `silence`)
//! co-exist; a future "chapter-pick-winner" step decides which
//! source's chapters are surfaced to the player.

use async_trait::async_trait;

use ab_core::tunables::NetworkTunables;
use ab_core::{BookId, Error, Result};
use ab_pipeline::{Stage, StageContext, StageOutcome};

use crate::AudnexusClient;

/// Provenance source tag for chapters this stage writes.
pub const CHAPTER_SOURCE: &str = "audnexus";

/// Stage that fetches chapter `ToC` + brand markers from Audnexus.
pub struct AudnexusChaptersStage {
    client: AudnexusClient,
    region_order: Vec<String>,
    allowed: bool,
}

impl AudnexusChaptersStage {
    /// Build with a pre-configured client + network tunables.
    /// Empty region list falls back to `["us"]`.
    #[must_use]
    pub fn new(client: AudnexusClient, network: &NetworkTunables) -> Self {
        let region_order = if network.audnexus_region_order.is_empty() {
            vec!["us".to_owned()]
        } else {
            network.audnexus_region_order.clone()
        };
        Self {
            client,
            region_order,
            allowed: network.audnexus_allowed,
        }
    }
}

#[async_trait]
impl Stage for AudnexusChaptersStage {
    fn name(&self) -> &'static str {
        "audnexus-chapters"
    }

    fn requires(&self) -> &'static [&'static str] {
        // audnexus-enrich populates `books.asin` (the join key
        // this stage uses). Without it we'd have no ASIN to look
        // up against the chapters endpoint.
        &["audnexus-enrich"]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        if !self.allowed {
            tracing::debug!(
                book = %book_id,
                "audnexus.chapters.disabled_by_tunables"
            );
            return Ok(StageOutcome::Skipped);
        }

        let Some(asin) = fetch_book_asin(&ctx.library, book_id).await? else {
            // No ASIN means audnexus-enrich didn't find a match.
            // Nothing to look up.
            return Ok(StageOutcome::Skipped);
        };

        // Walk regions the same way audnexus-enrich does so a
        // book that only resolves in `de` (say) finds its chapters
        // in `de` too. Transport errors are warn-logged + skipped
        // per-region.
        let mut hit: Option<crate::audnexus::AudnexusChapters> = None;
        for region in &self.region_order {
            match self.client.lookup_chapters(region, &asin).await {
                Ok(Some(c)) if !c.chapters.is_empty() => {
                    hit = Some(c);
                    break;
                }
                Ok(_) => {
                    tracing::debug!(
                        book = %book_id,
                        asin = %asin,
                        region = %region,
                        "audnexus.chapters.region_miss"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        book = %book_id,
                        asin = %asin,
                        region = %region,
                        error = %e,
                        "audnexus.chapters.region_error"
                    );
                }
            }
        }

        let Some(chapters) = hit else {
            return Ok(StageOutcome::Skipped);
        };

        write_chapters(&ctx.library, book_id, &chapters).await?;
        tracing::info!(
            book = %book_id,
            asin = %asin,
            chapter_count = chapters.chapters.len(),
            intro_ms = chapters.brand_intro_duration_ms,
            outro_ms = chapters.brand_outro_duration_ms,
            accurate = chapters.is_accurate,
            "audnexus.chapters.done"
        );
        Ok(StageOutcome::Done)
    }
}

/// Fetch the ASIN that audnexus-enrich promoted into `books.asin`.
async fn fetch_book_asin(library: &ab_db::LibraryDb, book_id: BookId) -> Result<Option<String>> {
    let id = book_id.0;
    let row = sqlx::query!("SELECT asin FROM books WHERE book_id = ?", id)
        .fetch_optional(library.pool())
        .await
        .map_err(|e| Error::Database(format!("chapters asin lookup: {e}")))?;
    Ok(row.and_then(|r| r.asin))
}

/// Persist the chapter list + brand markers. Clears any existing
/// rows for this book at `source = 'audnexus'` first so a re-run
/// converges to exactly what Audnexus currently returns.
async fn write_chapters(
    library: &ab_db::LibraryDb,
    book_id: BookId,
    chapters: &crate::audnexus::AudnexusChapters,
) -> Result<()> {
    let id = book_id.0;
    let mut tx = library
        .pool()
        .begin()
        .await
        .map_err(|e| Error::Database(format!("chapters tx begin: {e}")))?;

    sqlx::query!(
        "DELETE FROM chapters WHERE book_id = ? AND source = ?",
        id,
        CHAPTER_SOURCE,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| Error::Database(format!("chapters clear: {e}")))?;

    for (idx, ch) in chapters.chapters.iter().enumerate() {
        // SQLite bind for i64; offsets are u64 from JSON but
        // audiobook durations never exceed 2^31 ms (~24 days), so
        // the saturating cast is symbolic.
        let idx_i64 = i64::try_from(idx).unwrap_or(i64::MAX);
        let start_ms = i64::try_from(ch.start_offset_ms).unwrap_or(i64::MAX);
        let end_ms =
            i64::try_from(ch.start_offset_ms.saturating_add(ch.length_ms)).unwrap_or(i64::MAX);
        let title = if ch.title.trim().is_empty() {
            format!("Chapter {}", idx + 1)
        } else {
            ch.title.clone()
        };
        sqlx::query!(
            "INSERT INTO chapters (book_id, idx, start_ms, end_ms, title, source) \
             VALUES (?, ?, ?, ?, ?, ?)",
            id,
            idx_i64,
            start_ms,
            end_ms,
            title,
            CHAPTER_SOURCE,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| Error::Database(format!("chapters insert idx={idx}: {e}")))?;
    }

    let intro_ms = i64::try_from(chapters.brand_intro_duration_ms).unwrap_or(i64::MAX);
    let outro_ms = i64::try_from(chapters.brand_outro_duration_ms).unwrap_or(i64::MAX);
    if chapters.brand_intro_duration_ms > 0 {
        sqlx::query!(
            "UPDATE books SET audiologo_intro_ms = ? WHERE book_id = ?",
            intro_ms,
            id,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| Error::Database(format!("books.audiologo_intro_ms write: {e}")))?;
    }
    if chapters.brand_outro_duration_ms > 0 {
        sqlx::query!(
            "UPDATE books SET audiologo_outro_ms = ? WHERE book_id = ?",
            outro_ms,
            id,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| Error::Database(format!("books.audiologo_outro_ms write: {e}")))?;
    }

    tx.commit()
        .await
        .map_err(|e| Error::Database(format!("chapters tx commit: {e}")))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use ab_core::tunables::{DbTunables, HttpClientTunables};
    use tempfile::TempDir;

    use super::*;
    use crate::audnexus::{AudnexusChapter, AudnexusChapters};

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
            stage_name: "audnexus-chapters",
        }
    }

    #[tokio::test]
    async fn stage_metadata_matches_pipeline_expectations() {
        let client = AudnexusClient::new(&HttpClientTunables::default());
        let stage = AudnexusChaptersStage::new(client, &NetworkTunables::default());
        assert_eq!(stage.name(), "audnexus-chapters");
        assert_eq!(stage.requires(), &["audnexus-enrich"]);
    }

    #[tokio::test]
    async fn write_chapters_persists_chapters_and_brand_markers() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'fixture')")
            .execute(ctx.library.pool())
            .await
            .expect("seed book");

        let chapters = AudnexusChapters {
            asin: "B0FIX1".into(),
            brand_intro_duration_ms: 4500,
            brand_outro_duration_ms: 3000,
            is_accurate: true,
            chapters: vec![
                AudnexusChapter {
                    length_ms: 60_000,
                    start_offset_ms: 0,
                    title: "Prologue".into(),
                },
                AudnexusChapter {
                    length_ms: 120_000,
                    start_offset_ms: 60_000,
                    title: String::new(),
                },
            ],
        };

        write_chapters(&ctx.library, BookId(1), &chapters)
            .await
            .expect("write chapters");

        let rows: Vec<(i64, i64, i64, String, String)> = sqlx::query_as(
            "SELECT idx, start_ms, end_ms, title, source FROM chapters \
             WHERE book_id = 1 ORDER BY idx",
        )
        .fetch_all(ctx.library.pool())
        .await
        .expect("read chapters");
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            (0, 0, 60_000, "Prologue".into(), "audnexus".into())
        );
        // Empty title falls back to "Chapter N".
        assert_eq!(
            rows[1],
            (1, 60_000, 180_000, "Chapter 2".into(), "audnexus".into())
        );

        let intro: Option<i64> =
            sqlx::query_scalar("SELECT audiologo_intro_ms FROM books WHERE book_id = 1")
                .fetch_one(ctx.library.pool())
                .await
                .expect("intro");
        assert_eq!(intro, Some(4500));
        let outro: Option<i64> =
            sqlx::query_scalar("SELECT audiologo_outro_ms FROM books WHERE book_id = 1")
                .fetch_one(ctx.library.pool())
                .await
                .expect("outro");
        assert_eq!(outro, Some(3000));
    }

    #[tokio::test]
    async fn write_chapters_replaces_audnexus_rows_on_rerun() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'fixture')")
            .execute(ctx.library.pool())
            .await
            .expect("seed");
        // Existing audnexus rows + an unrelated source row.
        sqlx::query(
            "INSERT INTO chapters (book_id, idx, start_ms, end_ms, title, source) VALUES \
                (1, 0, 0, 10_000, 'old-audnexus', 'audnexus'), \
                (1, 0, 0, 10_000, 'embedded-survives', 'embedded')",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed chapters");

        let chapters = AudnexusChapters {
            asin: "B0FIX1".into(),
            brand_intro_duration_ms: 0,
            brand_outro_duration_ms: 0,
            is_accurate: false,
            chapters: vec![AudnexusChapter {
                length_ms: 50_000,
                start_offset_ms: 0,
                title: "new-audnexus".into(),
            }],
        };
        write_chapters(&ctx.library, BookId(1), &chapters)
            .await
            .expect("rerun");

        let sources: Vec<(String, String)> = sqlx::query_as(
            "SELECT source, title FROM chapters WHERE book_id = 1 ORDER BY source, idx",
        )
        .fetch_all(ctx.library.pool())
        .await
        .expect("read");
        assert_eq!(
            sources,
            vec![
                ("audnexus".into(), "new-audnexus".into()),
                ("embedded".into(), "embedded-survives".into()),
            ]
        );
    }

    #[tokio::test]
    async fn skips_when_book_has_no_asin() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'fixture')")
            .execute(ctx.library.pool())
            .await
            .expect("seed");

        let client = AudnexusClient::new(&HttpClientTunables::default());
        let stage = AudnexusChaptersStage::new(client, &NetworkTunables::default());
        let outcome = stage.run(&ctx, BookId(1)).await.expect("run");
        assert_eq!(outcome, StageOutcome::Skipped);
    }
}
