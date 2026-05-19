//! ADR-0057 S57.2 — `transcript-chapter-marks` stage.
//!
//! Builds chapter-aligned marks over the polished transcript by
//! combining two upstream artifacts:
//!
//! 1. **Winner chapters** from the `chapters` table — `book_id +
//!    is_winner = 1`, ordered by `idx`. Each row carries `idx`,
//!    `title`, `start_ms`, `end_ms`. The
//!    `chapter-pick-winner` stage in `ab_catalog` already picks
//!    exactly one source's rows per book.
//! 2. **Polished transcript text** from `ai_cache`. Prefers
//!    `cache_type='transcript_fm_polished'` (S57.1b output); falls
//!    back to `cache_type='transcript_full'` raw segments
//!    concatenated when polish hasn't run yet (no FM access, or
//!    the polish stage skipped this book).
//!
//! Output is `{book_id, total_duration_ms, chapters: [{idx, title,
//! start_ms, end_ms, start_char, end_char}]}` written to
//! `ai_cache.cache_type='transcript_chapter_marks'`.
//!
//! The `start_char` / `end_char` fields are byte offsets into the
//! resolved transcript text. They are approximate — computed by
//! proportional scaling of `(start_ms / total_duration_ms) ×
//! text_len`, then refined to the nearest whitespace boundary so
//! downstream consumers don't slice in the middle of a word. Exact
//! word-level alignment is deferred (would need preserving segment
//! timestamps through the FM polish, which the polish stage
//! doesn't do today).
//!
//! ## Why a stage instead of a query helper
//!
//! Time-to-char mapping requires the polished text length, which
//! changes whenever the polish re-runs (different
//! `extractor_version`). Caching the resolved marks lets
//! downstream SRT / VTT / EPUB export read a single row instead
//! of re-computing on every export call.
//!
//! ## Skip conditions (each tested)
//!
//! * Idempotency: an `ai_cache` row already exists at
//!   `cache_type='transcript_chapter_marks'` for the current
//!   `LlmTunables.extractor_version`. Bump the version to force
//!   a re-run.
//! * No winner chapters — the upstream
//!   `chapter-pick-winner` hasn't promoted a source yet.
//! * No transcript at all (neither polish cache nor full cache).
//! * `books.duration_ms` is NULL or zero — needed to scale time
//!   to char offsets; without it the marks degenerate to all-zero
//!   offsets which is worse than skipping.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use ab_core::tunables::LlmTunables;
use ab_core::{BookId, CacheKey, Error, Result};
use ab_db::LibraryDb;
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};

/// Typed identifier for this stage.
pub const STAGE_ID: StageId = StageId::new("transcript-chapter-marks");

/// Stage name written to `pipeline_progress`. Derives from `STAGE_ID`.
pub const STAGE_NAME: &str = STAGE_ID.as_str();

/// Stage that materializes chapter-aligned marks over the polished
/// transcript.
///
/// Construction is owned-`LlmTunables`-clone identical to the
/// polish stage so the daemon-side wiring is uniform.
pub struct TranscriptChapterMarksStage {
    tunables: Arc<LlmTunables>,
}

impl TranscriptChapterMarksStage {
    /// Construct a stage that reads its `extractor_version` from `tunables`.
    #[must_use]
    pub fn new(tunables: &LlmTunables) -> Self {
        Self {
            tunables: Arc::new(tunables.clone()),
        }
    }
}

#[async_trait]
impl Stage for TranscriptChapterMarksStage {
    fn name(&self) -> &'static str {
        STAGE_NAME
    }

    fn requires(&self) -> &'static [StageId] {
        // Chapter winners + at least the raw full transcript are
        // hard requirements. The polish (`transcript-fm-polish`)
        // is preferred but OPTIONAL — we degrade to the raw
        // transcript when polish hasn't run, so we don't make it
        // a hard dep (which would block books with no FM access).
        &[
            ab_catalog::CHAPTER_WINNER_STAGE_ID,
            crate::full_stage::STAGE_ID,
        ]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        // 1. Idempotency.
        if chapter_marks_cache_fresh(&ctx.library, book_id, &self.tunables.extractor_version)
            .await?
        {
            return Ok(StageOutcome::Skipped);
        }

        // 2. Resolve duration. NULL / zero → can't scale.
        let Some(duration_ms) = load_duration_ms(&ctx.library, book_id).await? else {
            tracing::debug!(book_id = book_id.0, "chapter_marks.skip_no_duration");
            return Ok(StageOutcome::Skipped);
        };

        // 3. Load winner chapters. Empty → upstream hasn't run.
        let chapters = load_winner_chapters(&ctx.library, book_id).await?;
        if chapters.is_empty() {
            tracing::debug!(book_id = book_id.0, "chapter_marks.skip_no_chapters");
            return Ok(StageOutcome::Skipped);
        }

        // 4. Resolve transcript text: polished > raw_full > skip.
        let Some(text) = load_resolved_transcript(&ctx.library, book_id).await? else {
            tracing::debug!(book_id = book_id.0, "chapter_marks.skip_no_transcript");
            return Ok(StageOutcome::Skipped);
        };
        let text_len = text.len();
        if text_len == 0 {
            tracing::debug!(book_id = book_id.0, "chapter_marks.skip_empty_transcript");
            return Ok(StageOutcome::Skipped);
        }

        // 5. Compute marks. Char offsets land on whitespace
        //    boundaries to avoid mid-word splits downstream.
        let marks = compute_marks(&chapters, duration_ms, &text);
        let payload = ChapterMarksPayload {
            book_id: book_id.0,
            total_duration_ms: duration_ms,
            chapters: marks,
        };

        write_cache(
            &ctx.library,
            book_id,
            &payload,
            &self.tunables.extractor_version,
        )
        .await?;

        tracing::info!(
            book_id = book_id.0,
            chapters = payload.chapters.len(),
            text_bytes = text_len,
            duration_ms,
            "chapter_marks.done"
        );
        Ok(StageOutcome::Done)
    }
}

/// Raw chapter row read from the `chapters` table.
#[derive(Debug, Clone)]
struct WinnerChapter {
    idx: i64,
    title: String,
    start_ms: i64,
    end_ms: i64,
}

/// One mark in the `transcript_chapter_marks` cache payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChapterMark {
    /// 0-based ordinal mirroring `chapters.idx`.
    pub idx: i64,
    /// Chapter title from the winner row.
    pub title: String,
    /// Start time in milliseconds from the winner row.
    pub start_ms: i64,
    /// End time in milliseconds from the winner row.
    pub end_ms: i64,
    /// Byte offset into the resolved transcript (polished if
    /// present, raw `transcript_full` otherwise) where this
    /// chapter starts.
    pub start_char: usize,
    /// Byte offset where this chapter ends (exclusive).
    pub end_char: usize,
}

/// Top-level shape of the cache row content.
#[derive(Debug, Serialize, Deserialize)]
pub struct ChapterMarksPayload {
    /// `books.book_id` this payload describes.
    pub book_id: i64,
    /// Total audio duration in ms — used downstream for sanity
    /// checking the mark sequence.
    pub total_duration_ms: i64,
    /// One mark per winner chapter, sorted by `idx`.
    pub chapters: Vec<ChapterMark>,
}

async fn chapter_marks_cache_fresh(
    library: &LibraryDb,
    book_id: BookId,
    extractor_version: &str,
) -> Result<bool> {
    let id = book_id.0;
    let cache = CacheKey::TranscriptChapterMarks.as_str();
    let row = sqlx::query!(
        "SELECT extractor_version FROM ai_cache WHERE book_id = ? AND cache_type = ?",
        id,
        cache,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("chapter_marks cache lookup: {e}")))?;
    let Some(row) = row else { return Ok(false) };
    Ok(row.extractor_version.as_deref() == Some(extractor_version))
}

async fn load_duration_ms(library: &LibraryDb, book_id: BookId) -> Result<Option<i64>> {
    let id = book_id.0;
    let row = sqlx::query!(
        r#"SELECT duration_ms AS "duration_ms: i64" FROM books WHERE book_id = ?"#,
        id
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("chapter_marks duration lookup: {e}")))?;
    Ok(row.and_then(|r| r.duration_ms).filter(|d: &i64| *d > 0))
}

async fn load_winner_chapters(library: &LibraryDb, book_id: BookId) -> Result<Vec<WinnerChapter>> {
    let id = book_id.0;
    let rows = sqlx::query!(
        r#"SELECT idx      AS "idx!: i64",
                  title    AS "title!: String",
                  start_ms AS "start_ms!: i64",
                  end_ms   AS "end_ms!: i64"
             FROM chapters
            WHERE book_id = ? AND is_winner = 1
            ORDER BY idx"#,
        id,
    )
    .fetch_all(library.pool())
    .await
    .map_err(|e| Error::Database(format!("chapter_marks load chapters: {e}")))?;
    Ok(rows
        .into_iter()
        .map(|r| WinnerChapter {
            idx: r.idx,
            title: r.title,
            start_ms: r.start_ms,
            end_ms: r.end_ms,
        })
        .collect())
}

/// Pick the best available transcript text. Polish wins when
/// present; otherwise concat the raw `transcript_full` segments.
async fn load_resolved_transcript(library: &LibraryDb, book_id: BookId) -> Result<Option<String>> {
    if let Some(polished) = load_polished_text(library, book_id).await? {
        return Ok(Some(polished));
    }
    load_raw_full_text(library, book_id).await
}

#[derive(Debug, Deserialize)]
struct PolishCacheEnvelope {
    raw: String,
}

#[derive(Debug, Deserialize)]
struct PolishResponseShape {
    polished_text: String,
}

async fn load_polished_text(library: &LibraryDb, book_id: BookId) -> Result<Option<String>> {
    let id = book_id.0;
    let cache = CacheKey::TranscriptFmPolished.as_str();
    let row = sqlx::query!(
        "SELECT content FROM ai_cache WHERE book_id = ? AND cache_type = ?",
        id,
        cache,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("chapter_marks polish lookup: {e}")))?;
    let Some(row) = row else { return Ok(None) };
    let Some(bytes) = row.content else {
        return Ok(None);
    };
    let envelope: PolishCacheEnvelope = match ab_core::cache::deserialize_cache_content(&bytes) {
        Ok(env) => env,
        Err(e) => {
            tracing::warn!(book_id = id, error = %e, "chapter_marks.polish_unparseable");
            return Ok(None);
        }
    };
    let response: PolishResponseShape = match serde_json::from_str(&envelope.raw) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(book_id = id, error = %e, "chapter_marks.polish_response_unparseable");
            return Ok(None);
        }
    };
    let trimmed = response.polished_text.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(response.polished_text))
    }
}

#[derive(Debug, Deserialize)]
struct CachedTranscript {
    segments: Vec<Segment>,
}

#[derive(Debug, Deserialize)]
struct Segment {
    text: String,
}

async fn load_raw_full_text(library: &LibraryDb, book_id: BookId) -> Result<Option<String>> {
    let id = book_id.0;
    let cache = CacheKey::TranscriptFull.as_str();
    let row = sqlx::query!(
        "SELECT content FROM ai_cache WHERE book_id = ? AND cache_type = ?",
        id,
        cache,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("chapter_marks full lookup: {e}")))?;
    let Some(row) = row else { return Ok(None) };
    let Some(bytes) = row.content else {
        return Ok(None);
    };
    let cached: CachedTranscript = match ab_core::cache::deserialize_cache_content(&bytes) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(book_id = id, error = %e, "chapter_marks.full_unparseable");
            return Ok(None);
        }
    };
    let mut text = String::with_capacity(
        cached
            .segments
            .iter()
            .map(|s| s.text.len() + 1)
            .sum::<usize>(),
    );
    for seg in &cached.segments {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&seg.text);
    }
    if text.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(text))
    }
}

/// Build the chapter marks. Time-to-char scaling is proportional;
/// each cut is refined to the nearest preceding whitespace
/// boundary so downstream splits don't land mid-word. The last
/// chapter's `end_char` is always exactly `text.len()` so the
/// EPUB export never drops trailing text.
fn compute_marks(
    chapters: &[WinnerChapter],
    total_duration_ms: i64,
    text: &str,
) -> Vec<ChapterMark> {
    let text_len = text.len();
    if chapters.is_empty() || total_duration_ms <= 0 || text_len == 0 {
        return Vec::new();
    }

    // First pass: compute raw scaled offsets. The casts are
    // intentional — we don't need exact integer precision for
    // a proportional time-to-char approximation; we snap to a
    // whitespace boundary afterwards.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let raw_offsets: Vec<usize> = chapters
        .iter()
        .map(|c| {
            let frac = (c.start_ms as f64) / (total_duration_ms as f64);
            let clamped = frac.clamp(0.0, 1.0);
            let raw = (clamped * (text_len as f64)) as usize;
            raw.min(text_len)
        })
        .collect();

    // Second pass: refine to whitespace boundary, then enforce
    // monotonic non-decreasing offsets (a noisy chapter table
    // could otherwise produce a non-monotonic sequence).
    let mut refined: Vec<usize> = Vec::with_capacity(raw_offsets.len());
    let mut last = 0usize;
    for (i, raw) in raw_offsets.iter().copied().enumerate() {
        let snapped = if i == 0 {
            0
        } else {
            snap_to_word_boundary(text, raw).max(last)
        };
        refined.push(snapped);
        last = snapped;
    }

    // Third pass: assemble marks. `end_char` of chapter i is
    // `start_char` of chapter i+1; the last chapter's `end_char`
    // is `text.len()`.
    chapters
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let start_char = refined[i];
            let end_char = refined.get(i + 1).copied().unwrap_or(text_len);
            ChapterMark {
                idx: c.idx,
                title: c.title.clone(),
                start_ms: c.start_ms,
                end_ms: c.end_ms,
                start_char,
                end_char,
            }
        })
        .collect()
}

/// Snap `offset` to the nearest word boundary so chapter cuts
/// land between words, not mid-word.
///
/// Three cases:
///
/// * `offset >= text.len()` — clamp to `text.len()`.
/// * `offset` is on whitespace — advance past the whitespace run
///   to the next word's first byte.
/// * `offset` is inside a word — walk left to the start of that
///   word (the byte after the preceding whitespace, or `0` when
///   the word is the first in the text).
fn snap_to_word_boundary(text: &str, offset: usize) -> usize {
    let len = text.len();
    if offset >= len {
        return len;
    }
    let bytes = text.as_bytes();
    if bytes[offset].is_ascii_whitespace() {
        let mut i = offset;
        while i < len && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        return i;
    }
    let mut i = offset;
    while i > 0 && !bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    i
}

async fn write_cache(
    library: &LibraryDb,
    book_id: BookId,
    payload: &ChapterMarksPayload,
    extractor_version: &str,
) -> Result<()> {
    let id = book_id.0;
    let bytes = serde_json::to_vec(payload)
        .map_err(|e| Error::stage(STAGE_NAME, format!("encode cache: {e}")))?;
    let cache = CacheKey::TranscriptChapterMarks.as_str();
    sqlx::query!(
        "INSERT OR REPLACE INTO ai_cache \
         (book_id, cache_type, content, compressed, extractor_version) \
         VALUES (?, ?, ?, 0, ?)",
        id,
        cache,
        bytes,
        extractor_version,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("chapter_marks cache write: {e}")))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use ab_core::tunables::DbTunables;
    use tempfile::TempDir;

    async fn fresh_ctx(dir: &std::path::Path) -> StageContext {
        let lib = LibraryDb::open(&dir.join("library.db"), &DbTunables::default())
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

    fn stage() -> TranscriptChapterMarksStage {
        TranscriptChapterMarksStage::new(&LlmTunables::default())
    }

    async fn seed_book(ctx: &StageContext, duration_ms: i64) -> i64 {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO books (title, duration_ms, raw_duration_ms) \
             VALUES ('T', ?, ?) RETURNING book_id",
        )
        .bind(duration_ms)
        .bind(duration_ms)
        .fetch_one(ctx.library.pool())
        .await
        .expect("seed book");
        id
    }

    struct ChapterRow<'a> {
        book_id: i64,
        idx: i64,
        start_ms: i64,
        end_ms: i64,
        title: &'a str,
    }

    async fn seed_winner_chapter(ctx: &StageContext, row: ChapterRow<'_>) {
        let ChapterRow {
            book_id,
            idx,
            start_ms,
            end_ms,
            title,
        } = row;
        sqlx::query(
            "INSERT INTO chapters (book_id, idx, start_ms, end_ms, title, source, is_winner) \
             VALUES (?, ?, ?, ?, ?, 'embedded', 1)",
        )
        .bind(book_id)
        .bind(idx)
        .bind(start_ms)
        .bind(end_ms)
        .bind(title)
        .execute(ctx.library.pool())
        .await
        .expect("seed chapter");
    }

    async fn seed_polished_cache(ctx: &StageContext, book_id: i64, polished_text: &str) {
        let response = serde_json::json!({
            "polished_text": polished_text,
            "polished_lang": "en",
        });
        let envelope = serde_json::json!({ "raw": response.to_string() });
        let bytes = serde_json::to_vec(&envelope).unwrap();
        sqlx::query(
            "INSERT INTO ai_cache (book_id, cache_type, content, extractor_version) \
             VALUES (?, 'transcript_fm_polished', ?, 'fm-26.0-v1')",
        )
        .bind(book_id)
        .bind(bytes)
        .execute(ctx.library.pool())
        .await
        .expect("seed polish cache");
    }

    async fn seed_full_cache(ctx: &StageContext, book_id: i64, text: &str) {
        let payload = serde_json::json!({
            "segments": [{"text": text}],
        });
        let bytes = serde_json::to_vec(&payload).unwrap();
        sqlx::query(
            "INSERT INTO ai_cache (book_id, cache_type, content, extractor_version) \
             VALUES (?, 'transcript_full', ?, 'v1')",
        )
        .bind(book_id)
        .bind(bytes)
        .execute(ctx.library.pool())
        .await
        .expect("seed full cache");
    }

    async fn seed_marks_cache(ctx: &StageContext, book_id: i64, version: &str) {
        sqlx::query(
            "INSERT INTO ai_cache (book_id, cache_type, content, extractor_version) \
             VALUES (?, 'transcript_chapter_marks', ?, ?)",
        )
        .bind(book_id)
        .bind(b"{}".to_vec())
        .bind(version)
        .execute(ctx.library.pool())
        .await
        .expect("seed marks cache");
    }

    async fn fetch_marks(ctx: &StageContext, book_id: i64) -> ChapterMarksPayload {
        let bytes: Vec<u8> = sqlx::query_scalar(
            "SELECT content FROM ai_cache WHERE book_id = ? AND cache_type = 'transcript_chapter_marks'",
        )
        .bind(book_id)
        .fetch_one(ctx.library.pool())
        .await
        .expect("fetch marks");
        serde_json::from_slice(&bytes).expect("parse marks payload")
    }

    #[tokio::test]
    async fn skips_when_no_winner_chapters() {
        let tmp = TempDir::new().expect("tmp");
        let ctx = fresh_ctx(tmp.path()).await;
        let id = seed_book(&ctx, 1_000_000).await;
        // Polish + duration are present, but chapters table is empty.
        seed_polished_cache(
            &ctx,
            id,
            "Hello world this is some polished transcript text",
        )
        .await;
        let outcome = stage().run(&ctx, BookId(id)).await.expect("run");
        assert_eq!(outcome, StageOutcome::Skipped);
    }

    #[tokio::test]
    async fn skips_when_duration_missing() {
        let tmp = TempDir::new().expect("tmp");
        let ctx = fresh_ctx(tmp.path()).await;
        let id = seed_book(&ctx, 0).await;
        seed_winner_chapter(
            &ctx,
            ChapterRow {
                book_id: id,
                idx: 0,
                start_ms: 0,
                end_ms: 60_000,
                title: "Chapter 1",
            },
        )
        .await;
        seed_polished_cache(&ctx, id, "some text").await;
        let outcome = stage().run(&ctx, BookId(id)).await.expect("run");
        assert_eq!(outcome, StageOutcome::Skipped);
    }

    #[tokio::test]
    async fn skips_when_no_transcript() {
        let tmp = TempDir::new().expect("tmp");
        let ctx = fresh_ctx(tmp.path()).await;
        let id = seed_book(&ctx, 600_000).await;
        seed_winner_chapter(
            &ctx,
            ChapterRow {
                book_id: id,
                idx: 0,
                start_ms: 0,
                end_ms: 600_000,
                title: "Only Chapter",
            },
        )
        .await;
        let outcome = stage().run(&ctx, BookId(id)).await.expect("run");
        assert_eq!(outcome, StageOutcome::Skipped);
    }

    #[tokio::test]
    async fn skips_when_cache_fresh_at_extractor_version() {
        let tmp = TempDir::new().expect("tmp");
        let ctx = fresh_ctx(tmp.path()).await;
        let id = seed_book(&ctx, 600_000).await;
        seed_winner_chapter(
            &ctx,
            ChapterRow {
                book_id: id,
                idx: 0,
                start_ms: 0,
                end_ms: 600_000,
                title: "C1",
            },
        )
        .await;
        seed_polished_cache(&ctx, id, &"word ".repeat(200)).await;
        let tunables = LlmTunables::default();
        seed_marks_cache(&ctx, id, &tunables.extractor_version).await;
        let outcome = stage().run(&ctx, BookId(id)).await.expect("run");
        assert_eq!(outcome, StageOutcome::Skipped);
    }

    #[tokio::test]
    async fn produces_chapter_marks_from_polished_text() {
        let tmp = TempDir::new().expect("tmp");
        let ctx = fresh_ctx(tmp.path()).await;
        let id = seed_book(&ctx, 1_000_000).await;
        seed_winner_chapter(
            &ctx,
            ChapterRow {
                book_id: id,
                idx: 0,
                start_ms: 0,
                end_ms: 500_000,
                title: "Chapter One",
            },
        )
        .await;
        seed_winner_chapter(
            &ctx,
            ChapterRow {
                book_id: id,
                idx: 1,
                start_ms: 500_000,
                end_ms: 1_000_000,
                title: "Chapter Two",
            },
        )
        .await;
        // 100 chars total — easy proportional math.
        let text = "a".repeat(50) + " " + &"b".repeat(49);
        seed_polished_cache(&ctx, id, &text).await;

        let outcome = stage().run(&ctx, BookId(id)).await.expect("run");
        assert_eq!(outcome, StageOutcome::Done);

        let payload = fetch_marks(&ctx, id).await;
        assert_eq!(payload.book_id, id);
        assert_eq!(payload.total_duration_ms, 1_000_000);
        assert_eq!(payload.chapters.len(), 2);
        assert_eq!(payload.chapters[0].idx, 0);
        assert_eq!(payload.chapters[0].start_char, 0);
        assert_eq!(payload.chapters[1].idx, 1);
        // 500_000 / 1_000_000 × 100 = 50, which is on the space;
        // snap advances past whitespace to the next word (51).
        assert_eq!(payload.chapters[1].start_char, 51);
        assert_eq!(
            payload.chapters[1].end_char,
            text.len(),
            "last chapter's end_char is exactly text.len()"
        );
    }

    #[tokio::test]
    async fn falls_back_to_raw_full_when_polish_missing() {
        let tmp = TempDir::new().expect("tmp");
        let ctx = fresh_ctx(tmp.path()).await;
        let id = seed_book(&ctx, 1_000_000).await;
        seed_winner_chapter(
            &ctx,
            ChapterRow {
                book_id: id,
                idx: 0,
                start_ms: 0,
                end_ms: 1_000_000,
                title: "Only Chapter",
            },
        )
        .await;
        seed_full_cache(&ctx, id, "raw transcript fallback").await;
        let outcome = stage().run(&ctx, BookId(id)).await.expect("run");
        assert_eq!(outcome, StageOutcome::Done);

        let payload = fetch_marks(&ctx, id).await;
        assert_eq!(payload.chapters.len(), 1);
        assert_eq!(payload.chapters[0].start_char, 0);
        assert_eq!(
            payload.chapters[0].end_char,
            "raw transcript fallback".len()
        );
    }

    #[test]
    fn snap_to_word_boundary_handles_three_cases() {
        let text = "alpha bravo charlie delta";
        // Inside a word → walk left to start-of-word.
        assert_eq!(snap_to_word_boundary(text, 8), 6);
        // Already at start-of-word → stay.
        assert_eq!(snap_to_word_boundary(text, 6), 6);
        // On whitespace → advance to next word.
        assert_eq!(snap_to_word_boundary(text, 5), 6);
        // First-word case: no left-side whitespace → snap to 0.
        assert_eq!(snap_to_word_boundary(text, 3), 0);
        // Past end → clamp.
        assert_eq!(snap_to_word_boundary(text, 9999), text.len());
    }

    #[test]
    fn compute_marks_enforces_monotonic_offsets() {
        // Pathological chapter table: start_ms goes BACKWARDS
        // between chapters 1 and 2. The proportional scaling
        // would produce decreasing offsets; the monotonic-snap
        // pass corrects them.
        let chapters = vec![
            WinnerChapter {
                idx: 0,
                title: "A".into(),
                start_ms: 0,
                end_ms: 500_000,
            },
            WinnerChapter {
                idx: 1,
                title: "B".into(),
                start_ms: 700_000,
                end_ms: 800_000,
            },
            WinnerChapter {
                idx: 2,
                title: "C".into(),
                start_ms: 300_000,
                end_ms: 1_000_000,
            },
        ];
        let text = "alpha bravo charlie delta echo foxtrot golf hotel india juliet";
        let marks = compute_marks(&chapters, 1_000_000, text);
        assert_eq!(marks.len(), 3);
        assert!(marks[0].start_char <= marks[1].start_char);
        assert!(marks[1].start_char <= marks[2].start_char);
        assert_eq!(marks[2].end_char, text.len());
    }

    #[test]
    fn compute_marks_handles_empty_inputs() {
        assert!(compute_marks(&[], 1000, "abc").is_empty());
        assert!(
            compute_marks(
                &[WinnerChapter {
                    idx: 0,
                    title: "A".into(),
                    start_ms: 0,
                    end_ms: 100,
                }],
                1000,
                "",
            )
            .is_empty()
        );
    }
}
