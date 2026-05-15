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
//!    `extractor_version` tunable stamped on the row.
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
//! exist in `ai_cache` at the configured `extractor_version`. Bump
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
use ab_core::{BookId, CacheKey, Error, Result};
use ab_db::{EphemeralDb, LibraryDb};
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};

use ab_speech::{BridgeError, TranscriptSegment, transcribe_window_typed};
use ab_speech::{LanguageDetection, detect};

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

    fn requires(&self) -> &'static [StageId] {
        // read-tags writes the title/author/subtitle/description/
        // narrator provenance rows the pre-transcribe gate reads.
        // Without it the gate degrades to default_locale; with it
        // the engine usually picks the correct locale on the
        // first try.
        &[ab_tag_read::STAGE_ID]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        // Pre-transcribe gate picks the locale from tag text. The
        // cache freshness check below uses extractor_version ONLY (no
        // locale comparison): pre-transcribe locale changing
        // between scans isn't enough to re-transcribe by itself.
        // The re-transcribe trigger is the post-transcribe quality
        // signal — see the disagreement check after head_segments
        // are produced.
        let pre = pre_transcribe_locale(&ctx.library, book_id, &self.language).await?;
        let locale = pre.detection.as_ref().map_or_else(
            || self.language.default_locale.clone(),
            |d| d.language.clone(),
        );

        let Some(plan) = plan_book(&ctx.library, book_id, &self.transcribe).await? else {
            return Ok(StageOutcome::Skipped);
        };

        // Persist the pre-transcribe candidate so downstream
        // consensus has the same view of "where the locale came
        // from."
        if let Some(d) = pre.detection.as_ref() {
            write_language_candidate(
                &ctx.library,
                book_id,
                SOURCE_NL_LANGUAGE_TAGS,
                STAGE_ID.as_str(),
                d,
            )
            .await?;
        }

        // Head window. Per 3D.2 the post-transcribe quality
        // gate moved to the `transcribe-samples` stage —
        // language detection on samples deep in the book is the
        // authoritative signal, not on the head (which can be
        // poisoned by jingles + non-native intros). The head
        // stage just transcribes in the pre-picked locale here.
        let head_segments = match transcribe_window_with_skip_on_no_model(
            &plan.head_path,
            0.0,
            plan.head_end_secs,
            &locale,
        )
        .await?
        {
            TranscribeWindowOutcome::Segments(s) => s,
            TranscribeWindowOutcome::ModelMissing => {
                queue_locale_install(&ctx.ephemeral, book_id, &locale).await?;
                return Ok(StageOutcome::Skipped);
            }
        };

        write_transcript_cache(
            &ctx.library,
            book_id,
            CacheKey::TranscriptHead,
            CacheWrite {
                segments: &head_segments,
                locale: &locale,
                extractor_version: &self.transcribe.extractor_version,
            },
        )
        .await?;

        // Tail window (skipped on short books). On `ModelMissing`
        // the head path already queued the locale; we just skip
        // the tail and return Done — head transcript is the more
        // valuable artifact.
        if let Some(tail) = plan.tail.as_ref() {
            match transcribe_window_with_skip_on_no_model(
                &tail.path,
                tail.start_secs,
                tail.end_secs,
                &locale,
            )
            .await?
            {
                TranscribeWindowOutcome::Segments(mut segments) => {
                    // Rebase last-file segments into book time-
                    // base. No-op for single-file books (offset
                    // is 0); shifts by cumulative offset for
                    // multi-file.
                    crate::multi_file::rebase_segments(&mut segments, tail.cumulative_offset_secs);
                    write_transcript_cache(
                        &ctx.library,
                        book_id,
                        CacheKey::TranscriptTail,
                        CacheWrite {
                            segments: &segments,
                            locale: &locale,
                            extractor_version: &self.transcribe.extractor_version,
                        },
                    )
                    .await?;
                    // No post-transcribe language candidate on
                    // the tail: it's 30 s of outro jingle by
                    // design, and jingles are English-biased per
                    // the LanguageTunables skip semantics. Tail's
                    // job is audiologo / last-sentence work, not
                    // language confirmation.
                }
                TranscribeWindowOutcome::ModelMissing => {
                    // Head landed but tail's locale is gone? That
                    // would be an Apple-side glitch — the install
                    // was good 6 minutes ago. Log and move on;
                    // the book is mostly useful with just the
                    // head transcript.
                    tracing::warn!(
                        book = %book_id,
                        locale,
                        "transcribe.tail.model_unexpectedly_missing"
                    );
                }
            }
        }

        Ok(StageOutcome::Done)
    }
}

// ── Stage metadata ────────────────────────────────────────────────

/// Typed identifier for this stage. Imported by dependents in
/// their `Stage::requires()` impls.
pub const STAGE_ID: StageId = StageId::new("transcribe-head-tail");

/// Stage name written to `pipeline_progress` and registered with
/// the daemon. Derives from `STAGE_ID` — single source of truth.
pub const STAGE_NAME: &str = STAGE_ID.as_str();

/// `book_field_provenance.source` for the pre-transcribe
/// language pick (tag text → `NLLanguageRecognizer`).
pub const SOURCE_NL_LANGUAGE_TAGS: &str = "nl_language_tags";

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
    /// For multi-file books, the last file's offset within the
    /// book. Segments produced from this window are in last-
    /// file time-base; rebasing by this offset puts them in
    /// book time-base. Zero for single-file books.
    cumulative_offset_secs: f64,
}

/// Resolve file paths, durations, and head/tail windows.
/// Returns `None` when the book should be skipped (no active
/// file, total duration too short, or both cached transcripts
/// already match the current `extractor_version`).
///
/// Idempotency is `extractor_version`-only. The 3A.4.2 re-transcribe
/// trigger is the in-stage post-transcribe disagreement quality
/// gate, not freshness here — re-running the stage doesn't
/// repeat the transcribe unless the `extractor_version` bumped or the
/// quality gate fires after the head transcript is produced.
async fn plan_book(
    library: &LibraryDb,
    book_id: BookId,
    transcribe: &TranscribeTunables,
) -> Result<Option<BookPlan>> {
    // Active files via the shared helper. For multi-file
    // books we need each file's cumulative offset so tail
    // segments can be rebased into book time-base. The helper
    // returns everything in one pass.
    let files = crate::multi_file::active_files(library, book_id).await?;
    if files.is_empty() {
        return Ok(None);
    }
    let total_secs = crate::multi_file::total_duration_secs(&files);
    if total_secs < transcribe.min_duration_secs {
        return Ok(None);
    }

    // Head: first file, [0, head_secs) clamped to file 0's
    // duration. file_0 starts at book offset 0, so the segments
    // come back already in book time-base — no rebase needed.
    let head = &files[0];
    let head_end_secs = transcribe.head_secs.min(head.duration_secs);

    // Tail: last file's trailing tail_secs. For single-file
    // books that's the same file as head; for multi-file it's
    // the last chapter. Segments from this window are in
    // last-file time-base; the cumulative_offset_secs goes
    // into the TailWindow so the run() loop can rebase before
    // writing to ai_cache.
    let tail = files.last().and_then(|last| {
        if last.duration_secs <= transcribe.tail_secs {
            // Too short to slice — head already covers it.
            return None;
        }
        Some(TailWindow {
            path: last.path.clone(),
            start_secs: last.duration_secs - transcribe.tail_secs,
            end_secs: last.duration_secs,
            cumulative_offset_secs: last.cumulative_offset_secs,
        })
    });

    // Idempotency: skip when both windows are already cached at
    // this extractor_version. Locale of the cached row is NOT a
    // freshness signal — pre-transcribe locale can change
    // between runs without invalidating the cache. The quality
    // gate after head transcription is what re-runs on
    // language disagreement.
    let head_fresh = cache_fresh(
        library,
        book_id,
        CacheKey::TranscriptHead,
        &transcribe.extractor_version,
    )
    .await?;
    let tail_fresh = if tail.is_some() {
        cache_fresh(
            library,
            book_id,
            CacheKey::TranscriptTail,
            &transcribe.extractor_version,
        )
        .await?
    } else {
        true
    };
    if head_fresh && tail_fresh {
        return Ok(None);
    }

    Ok(Some(BookPlan {
        head_path: head.path.clone(),
        head_end_secs,
        tail,
    }))
}

/// Returns true when `ai_cache` already has a row at the
/// configured `extractor_version` for `(book_id, cache_type)`.
async fn cache_fresh(
    library: &LibraryDb,
    book_id: BookId,
    cache_type: CacheKey,
    extractor_version: &str,
) -> Result<bool> {
    let id = book_id.0;
    let cache_str = cache_type.as_str();
    let row = sqlx::query!(
        "SELECT extractor_version FROM ai_cache WHERE book_id = ? AND cache_type = ?",
        id,
        cache_str,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("transcribe cache lookup: {e}")))?;
    Ok(row.is_some_and(|r| r.extractor_version.as_deref() == Some(extractor_version)))
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

// ── Transcribe wrapper that surfaces "model not installed" ───────

/// Outcome from a transcribe window. `ModelMissing` is the
/// recoverable failure that the idle-priority installer
/// (`run_idle_install_loop`) handles; caller writes a
/// `book_locale_blocks` row and returns `Skipped` so the book
/// retries automatically once the model lands.
#[derive(Debug)]
enum TranscribeWindowOutcome {
    Segments(Vec<TranscriptSegment>),
    ModelMissing,
}

/// Calls [`transcribe_window_typed`]; on
/// [`BridgeError::ModelNotInstalled`] returns
/// `TranscribeWindowOutcome::ModelMissing`. Other errors
/// propagate via `BridgeError -> ab_core::Error`.
async fn transcribe_window_with_skip_on_no_model(
    path: &std::path::Path,
    start_secs: f64,
    end_secs: f64,
    locale: &str,
) -> Result<TranscribeWindowOutcome> {
    match transcribe_window_typed(path, start_secs, end_secs, locale).await {
        Ok(segs) => Ok(TranscribeWindowOutcome::Segments(segs)),
        Err(BridgeError::ModelNotInstalled) => {
            tracing::warn!(
                locale,
                path = %path.display(),
                "transcribe.skip.model_not_installed"
            );
            Ok(TranscribeWindowOutcome::ModelMissing)
        }
        Err(e) => Err(e.into()),
    }
}

// ── Writes ──────────────────────────────────────────────────────

/// JSON payload stored in `ai_cache.content`. Just the segment
/// array — the locale lives in its own `ai_cache.locale` column
/// (per slice B2), not embedded here. Borrowing (`&[T]`) because
/// we only Serialize from here.
#[derive(Debug, Serialize)]
struct TranscriptPayload<'a> {
    segments: &'a [TranscriptSegment],
}

/// Args bundle for [`write_transcript_cache`] — keeps the
/// function under clippy's `too_many_arguments` cap and matches
/// the project convention of "≤5 args, otherwise take a config
/// struct."
struct CacheWrite<'a> {
    segments: &'a [TranscriptSegment],
    locale: &'a str,
    extractor_version: &'a str,
}

async fn write_transcript_cache(
    library: &LibraryDb,
    book_id: BookId,
    cache_type: CacheKey,
    args: CacheWrite<'_>,
) -> Result<()> {
    let payload = TranscriptPayload {
        segments: args.segments,
    };
    let bytes = serde_json::to_vec(&payload)
        .map_err(|e| Error::stage("transcribe", format!("encode payload: {e}")))?;
    // Mean segment confidence as a single-number summary for the
    // ai_cache row. Used by HTML reports / debug tools; not a
    // gate for downstream extractors.
    let conf = mean_confidence(args.segments);
    let id = book_id.0;
    let extractor_version = args.extractor_version;
    let locale = args.locale;
    let cache_str = cache_type.as_str();
    sqlx::query!(
        "INSERT OR REPLACE INTO ai_cache \
         (book_id, cache_type, content, compressed, confidence, extractor_version, locale) \
         VALUES (?, ?, ?, 0, ?, ?, ?)",
        id,
        cache_str,
        bytes,
        conf,
        extractor_version,
        locale,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("transcribe write cache: {e}")))?;
    Ok(())
}

/// Cross-module entry point so [`crate::samples_stage`] can
/// reuse the language-candidate writer (with `normalize` baked
/// in). The plain `write_language_candidate` stays private to
/// keep the head/tail call sites short.
pub(crate) async fn write_language_candidate_for_samples(
    library: &LibraryDb,
    book_id: BookId,
    source: &str,
    stage: &str,
    detection: &LanguageDetection,
) -> Result<()> {
    write_language_candidate(library, book_id, source, stage, detection).await
}

async fn write_language_candidate(
    library: &LibraryDb,
    book_id: BookId,
    source: &str,
    stage: &str,
    detection: &LanguageDetection,
) -> Result<()> {
    // Normalise via the central language-code table.
    // `NLLanguageRecognizer` returns BCP-47-ish primary subtags
    // already (`"en"`, `"de"`, `"zh-Hans"`) so this is usually a
    // no-op — but going through `normalize` guards against
    // future Apple format changes and skips on unparseable input
    // rather than polluting consensus.
    let Some(canonical) = ab_core::language_code::normalize(&detection.language) else {
        tracing::warn!(
            raw = %detection.language,
            book = %book_id,
            source,
            "transcribe.language.unparseable"
        );
        return Ok(());
    };
    let id = book_id.0;
    let conf = detection.confidence;
    sqlx::query!(
        "INSERT INTO book_field_provenance \
         (book_id, field, value, source, stage, confidence, is_winner) \
         VALUES (?, 'language', ?, ?, ?, ?, 0)",
        id,
        canonical,
        source,
        stage,
        conf,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("transcribe write language candidate: {e}")))?;
    Ok(())
}

/// Upserts the install-needed locale + records this book as
/// waiting on it. Called when transcribe lands a
/// [`BridgeError::ModelNotInstalled`]. Idempotent on both rows;
/// PK / unique constraints make duplicate calls safe.
///
/// The idle install loop (`run_idle_install_loop`) drains both
/// tables.
async fn queue_locale_install(
    ephemeral: &EphemeralDb,
    book_id: BookId,
    locale: &str,
) -> Result<()> {
    let id = book_id.0;
    // Don't reset `status='installing'` or `'installed'` rows —
    // the idle loop is mid-flight or just finished. Re-queueing
    // the book is enough.
    sqlx::query!(
        "INSERT INTO pending_speech_installs (locale, status) \
         VALUES (?, 'pending') \
         ON CONFLICT(locale) DO UPDATE \
           SET status = CASE \
                          WHEN pending_speech_installs.status = 'failed' \
                            THEN 'pending' \
                          ELSE pending_speech_installs.status \
                        END",
        locale,
    )
    .execute(ephemeral.pool())
    .await
    .map_err(|e| Error::Database(format!("queue speech install: {e}")))?;
    sqlx::query!(
        "INSERT OR IGNORE INTO book_locale_blocks (book_id, locale) VALUES (?, ?)",
        id,
        locale,
    )
    .execute(ephemeral.pool())
    .await
    .map_err(|e| Error::Database(format!("queue locale block: {e}")))?;
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
        segments: Vec<TranscriptSegment>,
    }

    #[test]
    fn transcript_payload_round_trips() {
        // Post-B2 the payload is segments-only; locale lives in
        // the ai_cache.locale column.
        let segs = vec![TranscriptSegment {
            start_ms: 0,
            end_ms: 1000,
            text: "hello".into(),
            confidence: 0.9,
        }];
        let payload = TranscriptPayload { segments: &segs };
        let bytes = serde_json::to_vec(&payload).expect("encode");
        let decoded: OwnedPayload = serde_json::from_slice(&bytes).expect("decode");
        assert_eq!(decoded.segments.len(), 1);
        assert_eq!(decoded.segments[0].text, "hello");
    }
}
