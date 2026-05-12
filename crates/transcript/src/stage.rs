//! `transcribe-head-tail` pipeline stage.
//!
//! For each book this stage:
//!
//! 1. Picks a `SpeechTranscriber` locale by running
//!    `NLLanguageRecognizer` over concatenated tag text
//!    (title, author, subtitle, description, narrator). Falls
//!    back to `LanguageTunables.default_locale` when no usable
//!    tag text exists.
//! 2. Transcribes `[0, head_secs)` of the book's first active
//!    file, then `[duration - tail_secs, duration)` of the
//!    last active file (or skips the tail when total duration
//!    is below `head_secs + tail_secs`).
//! 3. Stores both transcripts (gzip-free JSON, the segment
//!    array is tiny) in `ai_cache` keyed by
//!    `(book_id, cache_type)` with `cache_type` ∈
//!    {`transcript_head`, `transcript_tail`} and the
//!    `model_version` tunable stamped on the row.
//! 4. Runs post-transcribe `NLLanguageRecognizer` on the head
//!    transcript past `LanguageTunables.post_transcribe_skip_ms`
//!    (skips the publisher jingle window), writes a language
//!    candidate row to `book_field_provenance`.
//! 5. Writes the pre-transcribe language pick as a separate
//!    candidate row.
//!
//! ## Idempotency
//!
//! The stage skips a book when both head and tail rows already
//! exist in `ai_cache` at the configured `model_version`. Bump
//! the version (or wipe rows manually) to force re-transcribe.
//!
//! ## Failure modes
//!
//! - No active file → `Skipped`.
//! - Duration below `min_duration_secs` → `Skipped`.
//! - `SpeechAnalyzer` reports the model isn't installed → log
//!   warning + `Skipped`. The daemon's idle-priority installer
//!   (future slice) will fix that; meanwhile the stage retries
//!   on the next scan when the model is in.
//! - Other transcribe / detect errors → log warning + return
//!   `Err` so the executor records a failure (won't poison the
//!   queue — the executor handles failures per-job, not
//!   per-stage).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;

use ab_core::tunables::{LanguageTunables, TranscribeTunables};
use ab_core::{BookId, Error, Result};
use ab_db::LibraryDb;
use ab_pipeline::{Stage, StageContext, StageOutcome};

use crate::bridge::TranscriptSegment;
use crate::language::{LanguageDetection, detect, detect_from_transcript};

/// Stage that runs head + tail transcription and seeds the
/// language candidates.
pub struct TranscribeHeadTailStage {
    transcribe: Arc<TranscribeTunables>,
    language: Arc<LanguageTunables>,
}

impl TranscribeHeadTailStage {
    /// Construct.
    #[must_use]
    pub fn new(transcribe: &TranscribeTunables, language: &LanguageTunables) -> Self {
        Self {
            transcribe: Arc::new(transcribe.clone()),
            language: Arc::new(language.clone()),
        }
    }
}

#[async_trait]
impl Stage for TranscribeHeadTailStage {
    fn name(&self) -> &'static str {
        STAGE_NAME
    }

    fn requires(&self) -> &'static [&'static str] {
        // tag-read writes the title/author/subtitle/description/
        // narrator provenance rows the pre-transcribe gate reads.
        // Without it the gate degrades to default_locale; with it
        // the engine usually picks the correct locale on the
        // first try.
        &["tag-read"]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        let Some(plan) = plan_book(&ctx.library, book_id, &self.transcribe).await? else {
            return Ok(StageOutcome::Skipped);
        };

        // Pre-transcribe gate: tag text → locale.
        let pre = pre_transcribe_locale(&ctx.library, book_id, &self.language).await?;
        let locale = pre.detection.as_ref().map_or_else(
            || self.language.default_locale.clone(),
            |d| d.language.clone(),
        );

        // Persist the pre-transcribe candidate so downstream
        // consensus has the same view of "where the locale came
        // from."
        if let Some(d) = pre.detection.as_ref() {
            write_language_candidate(&ctx.library, book_id, SOURCE_NL_LANGUAGE_TAGS, d).await?;
        }

        // Head window.
        let Some(head_segments) = transcribe_window_with_skip_on_no_model(
            &plan.head_path,
            0.0,
            plan.head_end_secs,
            &locale,
        )
        .await?
        else {
            return Ok(StageOutcome::Skipped);
        };
        write_transcript_cache(
            &ctx.library,
            book_id,
            CACHE_TYPE_HEAD,
            CacheWrite {
                segments: &head_segments,
                locale: &locale,
                model_version: &self.transcribe.model_version,
            },
        )
        .await?;

        // Post-transcribe language candidate (head, with skip).
        if let Some(d) = detect_from_transcript(
            &head_segments,
            self.language.post_transcribe_skip_ms,
            self.language.max_alternatives,
        )
        .await?
        {
            write_language_candidate(
                &ctx.library,
                book_id,
                SOURCE_NL_LANGUAGE_TRANSCRIPT_HEAD,
                &d,
            )
            .await?;
        }

        // Tail window (skipped on short books).
        if let Some(tail) = plan.tail.as_ref() {
            let tail_segments = transcribe_window_with_skip_on_no_model(
                &tail.path,
                tail.start_secs,
                tail.end_secs,
                &locale,
            )
            .await?;
            if let Some(segments) = tail_segments {
                write_transcript_cache(
                    &ctx.library,
                    book_id,
                    CACHE_TYPE_TAIL,
                    CacheWrite {
                        segments: &segments,
                        locale: &locale,
                        model_version: &self.transcribe.model_version,
                    },
                )
                .await?;
                // No post-transcribe language candidate on the
                // tail: it's 30 s of outro jingle by design, and
                // jingles are English-biased per the LanguageTunables
                // skip semantics. Tail's job is audiologo /
                // last-sentence work, not language confirmation.
            }
        }

        Ok(StageOutcome::Done)
    }
}

// ── Stage metadata ────────────────────────────────────────────────

/// Stage name written to `pipeline_progress` and registered with
/// the daemon. Held as a `pub const` so call sites (API router,
/// docs, tests) can refer to one source of truth.
pub const STAGE_NAME: &str = "transcribe-head-tail";

/// `ai_cache.cache_type` value for the `[0, head_secs)` transcript.
pub const CACHE_TYPE_HEAD: &str = "transcript_head";

/// `ai_cache.cache_type` value for the
/// `[duration - tail_secs, duration)` transcript.
pub const CACHE_TYPE_TAIL: &str = "transcript_tail";

/// `book_field_provenance.source` for the pre-transcribe
/// language pick (tag text → `NLLanguageRecognizer`).
pub const SOURCE_NL_LANGUAGE_TAGS: &str = "nl_language_tags";

/// `book_field_provenance.source` for the post-transcribe head
/// language pick (transcript text past skip → recogniser).
pub const SOURCE_NL_LANGUAGE_TRANSCRIPT_HEAD: &str = "nl_language_transcript_head";

// ── Planning + idempotency ────────────────────────────────────────

/// Resolved per-book transcription plan.
#[derive(Debug, Clone)]
struct BookPlan {
    head_path: PathBuf,
    head_end_secs: f64,
    tail: Option<TailWindow>,
}

#[derive(Debug, Clone)]
struct TailWindow {
    path: PathBuf,
    start_secs: f64,
    end_secs: f64,
}

/// Resolve file paths, durations, and head/tail windows.
/// Returns `None` when the book should be skipped (no active
/// file, total duration too short, or both cached transcripts
/// already match the current `model_version`).
async fn plan_book(
    library: &LibraryDb,
    book_id: BookId,
    transcribe: &TranscribeTunables,
) -> Result<Option<BookPlan>> {
    let id = book_id.0;
    let row = sqlx::query!(
        "SELECT duration_ms, raw_duration_ms FROM books WHERE book_id = ?",
        id,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("transcribe fetch book: {e}")))?;
    let Some(row) = row else {
        return Ok(None);
    };

    // Prefer raw (untrimmed) duration so jingles fall inside the
    // head window — the audiologo + language extractors need to
    // SEE the jingle to detect it.
    let total_ms = row.raw_duration_ms.or(row.duration_ms).unwrap_or(0).max(0);
    // i64 → f64 is lossy past 2^53, which is ~285_000 years in
    // milliseconds. Audiobooks aren't that long.
    #[allow(clippy::cast_precision_loss)]
    let total_secs = total_ms as f64 / 1000.0;
    if total_secs < transcribe.min_duration_secs {
        return Ok(None);
    }

    // Head file: first active file. For multi-file books the
    // file-0 contains the publisher intro + first chapter,
    // which is what the head window targets.
    let head_row = sqlx::query!(
        "SELECT file_path, duration_ms FROM book_files \
         WHERE book_id = ? AND is_active = 1 ORDER BY file_id LIMIT 1",
        id,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("transcribe fetch head file: {e}")))?;
    let Some(head_row) = head_row else {
        return Ok(None);
    };
    #[allow(clippy::cast_precision_loss)]
    let head_file_secs = head_row.duration_ms.unwrap_or(total_ms).max(0) as f64 / 1000.0;
    let head_end_secs = transcribe.head_secs.min(head_file_secs);

    // Tail file: last active file. For single-file books it's
    // the same as the head file but a different window.
    let tail_row = sqlx::query!(
        "SELECT file_path, duration_ms FROM book_files \
         WHERE book_id = ? AND is_active = 1 ORDER BY file_id DESC LIMIT 1",
        id,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("transcribe fetch tail file: {e}")))?;

    let tail = tail_row.and_then(|r| {
        #[allow(clippy::cast_precision_loss)]
        let tail_file_secs = r.duration_ms.unwrap_or(0).max(0) as f64 / 1000.0;
        if tail_file_secs <= transcribe.tail_secs {
            // Too short to slice — the head transcript already
            // covers everything we'd want from the tail.
            return None;
        }
        Some(TailWindow {
            path: PathBuf::from(r.file_path),
            start_secs: tail_file_secs - transcribe.tail_secs,
            end_secs: tail_file_secs,
        })
    });

    // Idempotency: skip when both windows are already cached at
    // this model_version. We check head only when there's no
    // tail; with a tail, both must match.
    let head_fresh =
        cache_fresh(library, book_id, CACHE_TYPE_HEAD, &transcribe.model_version).await?;
    let tail_fresh = if tail.is_some() {
        cache_fresh(library, book_id, CACHE_TYPE_TAIL, &transcribe.model_version).await?
    } else {
        true
    };
    if head_fresh && tail_fresh {
        return Ok(None);
    }

    Ok(Some(BookPlan {
        head_path: PathBuf::from(head_row.file_path),
        head_end_secs,
        tail,
    }))
}

/// Returns true when `ai_cache` already has a row at the
/// configured `model_version` for `(book_id, cache_type)`.
async fn cache_fresh(
    library: &LibraryDb,
    book_id: BookId,
    cache_type: &str,
    model_version: &str,
) -> Result<bool> {
    let id = book_id.0;
    let row = sqlx::query!(
        "SELECT model_version FROM ai_cache WHERE book_id = ? AND cache_type = ?",
        id,
        cache_type,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("transcribe cache lookup: {e}")))?;
    Ok(row.is_some_and(|r| r.model_version.as_deref() == Some(model_version)))
}

// ── Pre-transcribe language pick ─────────────────────────────────

/// What the pre-transcribe gate decided.
struct PrePick {
    /// `None` when there's no tag text, the text is too short,
    /// or `NLLanguageRecognizer` gave no hypothesis. Callers
    /// fall back to `LanguageTunables.default_locale`.
    detection: Option<LanguageDetection>,
}

async fn pre_transcribe_locale(
    library: &LibraryDb,
    book_id: BookId,
    language: &LanguageTunables,
) -> Result<PrePick> {
    let id = book_id.0;
    // Pull current candidate values for the five fields we use
    // as language signal. We don't care which source wrote them;
    // text from any source is signal. Empty / NULL values just
    // contribute nothing.
    let rows = sqlx::query!(
        "SELECT field, value FROM book_field_provenance \
         WHERE book_id = ? AND value IS NOT NULL AND value <> '' \
         AND field IN ('title', 'author', 'subtitle', 'description', 'narrator')",
        id,
    )
    .fetch_all(library.pool())
    .await
    .map_err(|e| Error::Database(format!("transcribe fetch tag text: {e}")))?;

    let joined = rows
        .into_iter()
        .filter_map(|r| r.value)
        .collect::<Vec<_>>()
        .join(" ");
    if joined.chars().count() < language.min_text_chars {
        return Ok(PrePick { detection: None });
    }

    let detection = detect(&joined, language.max_alternatives).await?;
    let detection = detection.filter(|d| d.confidence >= language.min_confidence);
    Ok(PrePick { detection })
}

// ── Transcribe wrapper that demotes "model not installed" ────────

/// Calls `transcribe_window`; on "model not installed" the error
/// is converted to `Ok(None)` so the stage cleanly returns
/// `Skipped`. All other errors propagate.
async fn transcribe_window_with_skip_on_no_model(
    path: &std::path::Path,
    start_secs: f64,
    end_secs: f64,
    locale: &str,
) -> Result<Option<Vec<TranscriptSegment>>> {
    match crate::bridge::transcribe_window(path, start_secs, end_secs, locale).await {
        Ok(segs) => Ok(Some(segs)),
        Err(e) => {
            let msg = format!("{e}");
            // The Swift side stringifies the Swift error variant
            // as `modelNotInstalled(...)`. This string-match is
            // brittle if we ever change the Swift error path —
            // tracked as a follow-up to introduce a typed error
            // code over the FFI.
            if msg.contains("modelNotInstalled") {
                tracing::warn!(
                    locale,
                    path = %path.display(),
                    "transcribe.skip.model_not_installed"
                );
                Ok(None)
            } else {
                Err(e)
            }
        }
    }
}

// ── Writes ──────────────────────────────────────────────────────

/// JSON payload stored in `ai_cache.content`. We keep both the
/// segment array AND the locale we transcribed in, so a
/// re-extract can know what to expect from the text. Borrowing
/// (`&str`, `&[T]`) because we only Serialize from here; decode
/// uses an owned shape (see the test module).
#[derive(Debug, Serialize)]
struct TranscriptPayload<'a> {
    locale: &'a str,
    segments: &'a [TranscriptSegment],
}

/// Args bundle for [`write_transcript_cache`] — keeps the
/// function under clippy's `too_many_arguments` cap and matches
/// the project convention of "≤5 args, otherwise take a config
/// struct."
struct CacheWrite<'a> {
    segments: &'a [TranscriptSegment],
    locale: &'a str,
    model_version: &'a str,
}

async fn write_transcript_cache(
    library: &LibraryDb,
    book_id: BookId,
    cache_type: &str,
    args: CacheWrite<'_>,
) -> Result<()> {
    let payload = TranscriptPayload {
        locale: args.locale,
        segments: args.segments,
    };
    let bytes = serde_json::to_vec(&payload)
        .map_err(|e| Error::stage("transcribe", format!("encode payload: {e}")))?;
    // Mean segment confidence as a single-number summary for the
    // ai_cache row. Used by HTML reports / debug tools; not a
    // gate for downstream extractors.
    let conf = mean_confidence(args.segments);
    let id = book_id.0;
    let model_version = args.model_version;
    sqlx::query!(
        "INSERT OR REPLACE INTO ai_cache \
         (book_id, cache_type, content, compressed, confidence, model_version) \
         VALUES (?, ?, ?, 0, ?, ?)",
        id,
        cache_type,
        bytes,
        conf,
        model_version,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("transcribe write cache: {e}")))?;
    Ok(())
}

async fn write_language_candidate(
    library: &LibraryDb,
    book_id: BookId,
    source: &str,
    detection: &LanguageDetection,
) -> Result<()> {
    let id = book_id.0;
    let conf = detection.confidence;
    sqlx::query!(
        "INSERT INTO book_field_provenance \
         (book_id, field, value, source, confidence, is_winner) \
         VALUES (?, 'language', ?, ?, ?, 0)",
        id,
        detection.language,
        source,
        conf,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("transcribe write language candidate: {e}")))?;
    Ok(())
}

/// Mean of the per-segment confidences. Returns 0.0 when the
/// vec is empty (caller has already gated on non-empty before
/// writing).
fn mean_confidence(segments: &[TranscriptSegment]) -> f64 {
    if segments.is_empty() {
        return 0.0;
    }
    let sum: f64 = segments.iter().map(|s| f64::from(s.confidence)).sum();
    // usize → f64 is lossy past 2^52, but a single book has at
    // most a few thousand segments. Comfortably inside.
    #[allow(clippy::cast_precision_loss)]
    let n = segments.len() as f64;
    sum / n
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn mean_confidence_empty_is_zero() {
        assert!((mean_confidence(&[]) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn mean_confidence_averages_per_segment() {
        let segs = vec![
            TranscriptSegment {
                start_ms: 0,
                end_ms: 100,
                text: "a".into(),
                confidence: 0.5,
            },
            TranscriptSegment {
                start_ms: 100,
                end_ms: 200,
                text: "b".into(),
                confidence: 1.0,
            },
        ];
        let m = mean_confidence(&segs);
        assert!((m - 0.75).abs() < 0.0001, "got {m}");
    }

    /// Owned mirror of [`TranscriptPayload`] for round-trip
    /// tests. The production type borrows (`&str`, `&[T]`) so
    /// `serde::Deserialize` can't derive on it cleanly; this
    /// owned twin handles the read side. The on-disk format must
    /// stay compatible between the two — drift here is a real
    /// regression.
    #[derive(serde::Deserialize)]
    struct OwnedPayload {
        locale: String,
        segments: Vec<TranscriptSegment>,
    }

    #[test]
    fn transcript_payload_round_trips() {
        let segs = vec![TranscriptSegment {
            start_ms: 0,
            end_ms: 1000,
            text: "hello".into(),
            confidence: 0.9,
        }];
        let payload = TranscriptPayload {
            locale: "en-US",
            segments: &segs,
        };
        let bytes = serde_json::to_vec(&payload).expect("encode");
        let decoded: OwnedPayload = serde_json::from_slice(&bytes).expect("decode");
        assert_eq!(decoded.locale, "en-US");
        assert_eq!(decoded.segments.len(), 1);
        assert_eq!(decoded.segments[0].text, "hello");
    }
}
