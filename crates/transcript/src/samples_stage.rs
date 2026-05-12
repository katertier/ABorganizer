//! `transcribe-samples` pipeline stage (slice 3D.2).
//!
//! Transcribes short (~60 s) windows at configured percentage
//! positions through the book (default 25% / 50% / 75%) and
//! runs `NLLanguageRecognizer` over the combined text. The
//! sample positions land deep enough that any English-language
//! publisher jingle (intro at 0%, outro near 100%) or a
//! single-chapter non-native intro doesn't bias the result.
//!
//! ## Why this stage exists
//!
//! Two reasons:
//!
//! 1. **Authoritative language signal.** Pre-transcribe relies
//!    on tag-text quality; head-post-transcribe can be fooled
//!    by jingles + non-native intros. Samples deep in the book
//!    are robust against both — agreement across 3 samples is
//!    strong evidence, disagreement triggers a head re-transcribe
//!    (the 3A.4.2 quality gate moves here).
//!
//! 2. **Fast DNA-tag corpus.** Provides representative content
//!    text *now* for downstream extractors (DNA tags, summary,
//!    person extraction) without waiting for the full-book
//!    transcribe at Idle priority to complete (which can take
//!    hours).
//!
//! ## Storage
//!
//! Combined sample transcript → `ai_cache(cache_type =
//! 'transcript_samples')` as JSON `{locale, segments}`. Same
//! shape as head/tail/full so extractors can read any of them
//! through the same code.
//!
//! ## Failure modes
//!
//! - No cached head transcript → `Skipped` (depends on head/tail).
//! - Total duration below `min_duration_secs` → `Skipped`.
//! - Any sample hits `ModelNotInstalled` → `Skipped` (the
//!   head/tail stage already queued the locale; idle installer
//!   re-queues this stage when the model lands).

use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;

use ab_core::tunables::{LanguageTunables, TranscribeTunables};
use ab_core::{BookId, Error, Result};
use ab_db::LibraryDb;
use ab_pipeline::{Stage, StageContext, StageOutcome};

use crate::bridge::{BridgeError, TranscriptSegment, transcribe_window_typed};
use crate::language::detect;
use crate::stage::{CACHE_TYPE_HEAD, STAGE_NAME as TRANSCRIBE_HEAD_TAIL_STAGE};

/// Stage name written to `pipeline_progress` + registered with
/// the daemon.
pub const STAGE_NAME: &str = "transcribe-samples";

/// `ai_cache.cache_type` value for the combined sample transcript.
pub const CACHE_TYPE_SAMPLES: &str = "transcript_samples";

/// `book_field_provenance.source` value for the language
/// candidate produced from the samples.
///
/// Higher-trust than the `nl_language_tags` source because the
/// samples are deeper into the book content (past intros,
/// publisher jingles, and any chapter-boundary non-native
/// passages).
pub const SOURCE_NL_LANGUAGE_SAMPLES: &str = "nl_language_samples";

/// Per-book sampled-transcribe stage.
pub struct TranscribeSamplesStage {
    transcribe: Arc<TranscribeTunables>,
    language: Arc<LanguageTunables>,
}

impl TranscribeSamplesStage {
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
impl Stage for TranscribeSamplesStage {
    fn name(&self) -> &'static str {
        STAGE_NAME
    }

    fn requires(&self) -> &'static [&'static str] {
        // We read locale from the head transcript's embedded
        // payload to avoid re-running the pre-transcribe gate.
        // If the head re-transcribed on its own quality signal
        // (still in place during 3D.2 transition), the cached
        // locale reflects the corrected choice.
        &[TRANSCRIBE_HEAD_TAIL_STAGE]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        let Some(plan) = plan_samples(&ctx.library, book_id, &self.transcribe).await? else {
            return Ok(StageOutcome::Skipped);
        };

        let mut all_segments = Vec::new();
        for (idx, window) in plan.windows.iter().enumerate() {
            let file = &plan.files[window.file_index];
            tracing::debug!(
                book = %book_id,
                idx,
                file_idx = window.file_index,
                in_file_start = window.in_file_start_secs,
                in_file_end = window.in_file_end_secs,
                "transcribe.samples.window"
            );
            match transcribe_window_typed(
                &file.path,
                window.in_file_start_secs,
                window.in_file_end_secs,
                &plan.locale,
            )
            .await
            {
                Ok(mut segs) => {
                    // Rebase per-file timestamps into book
                    // time-base via the cumulative offset.
                    crate::multi_file::rebase_segments(&mut segs, file.cumulative_offset_secs);
                    all_segments.append(&mut segs);
                }
                Err(BridgeError::ModelNotInstalled) => {
                    tracing::warn!(
                        locale = %plan.locale,
                        book = %book_id,
                        "transcribe.samples.skip.model_not_installed"
                    );
                    return Ok(StageOutcome::Skipped);
                }
                Err(e) => return Err(e.into()),
            }
        }

        if all_segments.is_empty() {
            tracing::warn!(book = %book_id, "transcribe.samples.no_segments");
            return Ok(StageOutcome::Skipped);
        }

        // Persist the combined transcript so DNA-tag /
        // summary / person extractors have one cache row to
        // read.
        write_samples_cache(
            &ctx.library,
            book_id,
            &all_segments,
            &plan.locale,
            &self.transcribe.model_version,
        )
        .await?;

        // Language detection over the joined text. This is the
        // authoritative post-transcribe signal — the
        // head-post-detect path from 3A.4.2 is being retired.
        let joined: String = all_segments
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        if let Some(d) = detect(&joined, self.language.max_alternatives).await? {
            // Reuse the existing language-candidate writer in
            // the head/tail stage module — it already routes
            // through `language_code::normalize` per 3D.1.
            crate::stage::write_language_candidate_for_samples(
                &ctx.library,
                book_id,
                SOURCE_NL_LANGUAGE_SAMPLES,
                &d,
            )
            .await?;
        }

        Ok(StageOutcome::Done)
    }
}

// ── Planning ────────────────────────────────────────────────────

/// One sample's resolved coordinates: which file, and where
/// inside it. Filled in by `plan_samples` via
/// `multi_file::map_position`.
#[derive(Debug, Clone)]
struct SampleWindow {
    file_index: usize,
    in_file_start_secs: f64,
    in_file_end_secs: f64,
}

#[derive(Debug)]
struct SamplePlan {
    /// Every active file with cumulative offsets — same data
    /// the full-stage uses. We need both `path` (for the
    /// transcribe call) and `cumulative_offset_secs` (for
    /// segment rebasing).
    files: Vec<crate::multi_file::FileEntry>,
    locale: String,
    windows: Vec<SampleWindow>,
}

#[derive(serde::Deserialize)]
struct HeadPayload {
    locale: String,
}

#[derive(serde::Deserialize)]
struct CachedSamplesLocale {
    locale: String,
}

/// Resolve file path, total duration, locale (from head cache),
/// and the sample windows. Returns `None` on skip conditions.
async fn plan_samples(
    library: &LibraryDb,
    book_id: BookId,
    transcribe: &TranscribeTunables,
) -> Result<Option<SamplePlan>> {
    let id = book_id.0;

    // Locale from head transcript.
    let head_row = sqlx::query!(
        "SELECT content FROM ai_cache WHERE book_id = ? AND cache_type = ?",
        id,
        CACHE_TYPE_HEAD,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("samples head lookup: {e}")))?;
    let Some(head_row) = head_row else {
        return Ok(None);
    };
    let Some(bytes) = head_row.content else {
        return Ok(None);
    };
    let Ok(parsed) = serde_json::from_slice::<HeadPayload>(&bytes) else {
        return Ok(None);
    };
    let locale = parsed.locale;

    // Idempotency.
    if samples_cache_fresh(library, book_id, &transcribe.model_version, &locale).await? {
        return Ok(None);
    }

    // Active files with offsets. Same helper as full_stage.
    let files = crate::multi_file::active_files(library, book_id).await?;
    if files.is_empty() {
        return Ok(None);
    }
    let total_secs = crate::multi_file::total_duration_secs(&files);
    if total_secs < transcribe.min_duration_secs {
        return Ok(None);
    }

    let windows = build_windows(
        &files,
        total_secs,
        &transcribe.sample_positions,
        transcribe.sample_secs,
    );
    if windows.is_empty() {
        return Ok(None);
    }

    Ok(Some(SamplePlan {
        files,
        locale,
        windows,
    }))
}

/// Build sample windows from position fractions + sample
/// length, mapping each book-time position to a specific file
/// + in-file offset.
///
/// Windows that would span a file boundary are clamped to the
/// end of the containing file — the rest is dropped. A 60-s
/// sample landing 30 s before a chapter boundary becomes a
/// 30-s sample. Good enough for language detection /
/// DNA-tag corpus purposes; the wholeness-of-content cost is
/// minor compared to the complexity of spanning the boundary.
fn build_windows(
    files: &[crate::multi_file::FileEntry],
    total_secs: f64,
    positions: &[f64],
    sample_secs: f64,
) -> Vec<SampleWindow> {
    let mut out = Vec::with_capacity(positions.len());
    for &pos in positions {
        let target = (pos * total_secs).max(0.0);
        let Some((file_idx, in_file_start)) = crate::multi_file::map_position(files, target) else {
            continue;
        };
        let file_duration = files[file_idx].duration_secs;
        let in_file_end = (in_file_start + sample_secs).min(file_duration);
        if in_file_end > in_file_start + 1.0 {
            // Require at least 1 s of content; sub-second
            // windows aren't useful for any extractor.
            out.push(SampleWindow {
                file_index: file_idx,
                in_file_start_secs: in_file_start,
                in_file_end_secs: in_file_end,
            });
        }
    }
    out
}

/// Idempotency check matching the head/tail and full-book
/// stages' approach.
async fn samples_cache_fresh(
    library: &LibraryDb,
    book_id: BookId,
    model_version: &str,
    current_locale: &str,
) -> Result<bool> {
    let id = book_id.0;
    let row = sqlx::query!(
        "SELECT model_version, content FROM ai_cache WHERE book_id = ? AND cache_type = ?",
        id,
        CACHE_TYPE_SAMPLES,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("samples cache lookup: {e}")))?;
    let Some(row) = row else { return Ok(false) };
    if row.model_version.as_deref() != Some(model_version) {
        return Ok(false);
    }
    let Some(bytes) = row.content else {
        return Ok(false);
    };
    let Ok(parsed) = serde_json::from_slice::<CachedSamplesLocale>(&bytes) else {
        return Ok(false);
    };
    Ok(parsed.locale == current_locale)
}

// ── Writes ──────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct SamplesPayload<'a> {
    locale: &'a str,
    segments: &'a [TranscriptSegment],
}

async fn write_samples_cache(
    library: &LibraryDb,
    book_id: BookId,
    segments: &[TranscriptSegment],
    locale: &str,
    model_version: &str,
) -> Result<()> {
    let payload = SamplesPayload { locale, segments };
    let bytes = serde_json::to_vec(&payload)
        .map_err(|e| Error::stage("transcribe-samples", format!("encode payload: {e}")))?;
    let conf = mean_confidence(segments);
    let id = book_id.0;
    sqlx::query!(
        "INSERT OR REPLACE INTO ai_cache \
         (book_id, cache_type, content, compressed, confidence, model_version) \
         VALUES (?, ?, ?, 0, ?, ?)",
        id,
        CACHE_TYPE_SAMPLES,
        bytes,
        conf,
        model_version,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("samples write cache: {e}")))?;
    Ok(())
}

fn mean_confidence(segments: &[TranscriptSegment]) -> f64 {
    if segments.is_empty() {
        return 0.0;
    }
    let sum: f64 = segments.iter().map(|s| f64::from(s.confidence)).sum();
    #[allow(clippy::cast_precision_loss)]
    let n = segments.len() as f64;
    sum / n
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    /// Helper: build a single-file fixture for the test cases
    /// that don't care about cross-file mapping.
    fn single_file(duration_secs: f64) -> Vec<crate::multi_file::FileEntry> {
        vec![crate::multi_file::FileEntry {
            path: std::path::PathBuf::from("/tmp/test.m4b"),
            duration_secs,
            cumulative_offset_secs: 0.0,
        }]
    }

    /// Helper: build a multi-file fixture with the given
    /// per-file durations.
    fn multi_file(durations: &[f64]) -> Vec<crate::multi_file::FileEntry> {
        let mut cum = 0.0;
        durations
            .iter()
            .enumerate()
            .map(|(i, &d)| {
                let entry = crate::multi_file::FileEntry {
                    path: std::path::PathBuf::from(format!("/tmp/f{i}.m4b")),
                    duration_secs: d,
                    cumulative_offset_secs: cum,
                };
                cum += d;
                entry
            })
            .collect()
    }

    #[test]
    fn build_windows_single_file_default_positions() {
        let files = single_file(3600.0);
        let w = build_windows(&files, 3600.0, &[0.25, 0.50, 0.75], 60.0);
        assert_eq!(w.len(), 3);
        for win in &w {
            assert_eq!(win.file_index, 0);
        }
        assert!((w[0].in_file_start_secs - 900.0).abs() < 0.001);
        assert!((w[0].in_file_end_secs - 960.0).abs() < 0.001);
        assert!((w[1].in_file_start_secs - 1800.0).abs() < 0.001);
        assert!((w[2].in_file_start_secs - 2700.0).abs() < 0.001);
    }

    #[test]
    fn build_windows_single_file_clamps_to_total() {
        let files = single_file(100.0);
        let w = build_windows(&files, 100.0, &[0.95], 60.0);
        assert_eq!(w.len(), 1);
        assert!((w[0].in_file_start_secs - 95.0).abs() < 0.001);
        assert!((w[0].in_file_end_secs - 100.0).abs() < 0.001);
    }

    #[test]
    fn build_windows_single_file_rejects_too_short() {
        let files = single_file(100.0);
        let w = build_windows(&files, 100.0, &[0.999], 60.0);
        assert!(w.is_empty(), "expected empty, got {w:?}");
    }

    #[test]
    fn build_windows_handles_empty_positions() {
        let files = single_file(3600.0);
        let w = build_windows(&files, 3600.0, &[], 60.0);
        assert!(w.is_empty());
    }

    #[test]
    fn build_windows_multi_file_maps_to_containing_file() {
        // 3 files of 600 s each → total 1800 s.
        // 25% = 450 s → file 0, offset 450
        // 50% = 900 s → file 1, offset 300
        // 75% = 1350 s → file 2, offset 150
        let files = multi_file(&[600.0, 600.0, 600.0]);
        let w = build_windows(&files, 1800.0, &[0.25, 0.50, 0.75], 60.0);
        assert_eq!(w.len(), 3);
        assert_eq!(w[0].file_index, 0);
        assert!((w[0].in_file_start_secs - 450.0).abs() < 0.001);
        assert_eq!(w[1].file_index, 1);
        assert!((w[1].in_file_start_secs - 300.0).abs() < 0.001);
        assert_eq!(w[2].file_index, 2);
        assert!((w[2].in_file_start_secs - 150.0).abs() < 0.001);
    }

    #[test]
    fn build_windows_multi_file_clamps_at_file_boundary() {
        // 2 files of 100 s each → total 200 s.
        // Position 0.45 = 90 s → file 0, offset 90.
        // Sample 60 s would span 90..150 but file 0 ends at
        // 100 — clamp to 90..100. 10 s of content, > 1 s
        // minimum, keep.
        let files = multi_file(&[100.0, 100.0]);
        let w = build_windows(&files, 200.0, &[0.45], 60.0);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].file_index, 0);
        assert!((w[0].in_file_start_secs - 90.0).abs() < 0.001);
        assert!((w[0].in_file_end_secs - 100.0).abs() < 0.001);
    }

    #[test]
    fn mean_confidence_basic() {
        let segs = vec![
            TranscriptSegment {
                start_ms: 0,
                end_ms: 100,
                text: "a".into(),
                confidence: 0.8,
            },
            TranscriptSegment {
                start_ms: 100,
                end_ms: 200,
                text: "b".into(),
                confidence: 1.0,
            },
        ];
        let m = mean_confidence(&segs);
        assert!((m - 0.9).abs() < 0.0001);
    }
}
