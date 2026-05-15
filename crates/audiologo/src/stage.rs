//! `detect-audiologo` pipeline stage.
//!
//! Slice 4B (the parent slice for this file): runs publisher-
//! jingle detection across each active book file. Per ADR-0024
//! Revision 2 the detection path is fingerprint-only; the
//! `audiologos` table holds known publisher fingerprints
//! (seeded + grown via review confirmation + `ABtagger` import),
//! and this stage windows the head/tail of each file via the
//! `ab_audio::read_samples_window` Swift FFI bridge, fingerprints
//! the samples via `ab_fingerprint::fingerprint_samples`, and
//! `slide_match`-es every candidate audiologo against the result.
//!
//! ## Slice ladder (4B complete as of slice 4B.6)
//!
//! - **4B.0:** rename `books.audiologo_intro_ms` →
//!   `brand_intro_duration_ms` (migration 017); audnexus-chapters
//!   stage restores the writeback. ✓
//! - **4B.1:** Swift `AVAssetReader` FFI in `crates/audio` for
//!   windowed Float32 PCM decode. ✓
//! - **4B.2:** chromaprint matching helpers in `ab-fingerprint`
//!   (`fingerprint_samples`, `slide_match`,
//!   `confidence_from_hamming`). ✓
//! - **4B.3:** stage skeleton + `Method` enum prune (drop
//!   `CatalogBrandDuration`) + `Stage::reset` override. ✓
//! - **4B.4a:** load helpers + tunables wiring; `run()` data
//!   plumbing only. ✓
//! - **4B.4b:** detection-loop body — sample-fingerprint-slide-
//!   match-persist; candidate rows only. ✓
//! - **4B.5:** auto-apply path (candidate → applied for
//!   high-confidence `FingerprintFull` / `FingerprintBookend`) +
//!   chapter-shift maths on apply + Libation-stripped path. ✓
//! - **4B.6:** integration tests + ADR-0024 closure note; auto-
//!   apply runs every pass (not just when this run produced
//!   candidates). ✓
//!
//! What's still owed for the audiologo theme overall (not 4B):
//! - **4C:** transcript-aided tiers (`FingerprintAndTranscript` +
//!   `TranscriptOnly`) + tier-4 vocab.
//! - **4D:** review CLI / GUI + `match_count`-driven
//!   `verified_via` auto-promotion.
//!
//! ## `Stage::reset` semantics
//!
//! Per ADR-0024 § state-machine diagram: a reset doesn't delete
//! `book_file_audiologos` rows. Instead it flips rows currently
//! at `applied` → `re_detected` (preserving the audit trail),
//! NULLs `audiologo_status` back to its default, and clears
//! `pipeline_progress`. The next run produces fresh `candidate`
//! / `applied` rows; the prior `re_detected` ones surface in the
//! review UI as "previously applied → superseded."
//!
//! Rows already at `candidate` or `rejected` are left alone —
//! `candidate` rows are normal pre-apply state; `rejected` rows
//! are user-final decisions and shouldn't reappear as candidates.

use async_trait::async_trait;

use ab_core::tunables::AudiologoTunables;
use ab_core::{BookId, Error, Result};
use ab_db::LibraryDb;
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};

use crate::{Kind, Status};

/// Typed stage identifier for this stage.
pub const STAGE_ID: StageId = StageId::new("detect-audiologo");

/// Convenience alias matching the per-stage `STAGE_NAME = STAGE_ID.as_str()`
/// pattern used across the workspace.
pub const STAGE_NAME: &str = STAGE_ID.as_str();

/// Background-priority stage that detects publisher jingles at
/// the head + tail of each active book file.
///
/// 4B.3 ships the skeleton; detection logic lands in 4B.4.
#[derive(Debug)]
pub struct DetectAudiologoStage {
    intro_window_ms: u64,
    outro_window_ms: u64,
    /// Whole tunables ref kept for the auto-apply phase (4B.5).
    /// Auto-apply reads per-method confidence floors + padding
    /// defaults, so the stage hangs onto the full struct rather
    /// than copying every field individually.
    tunables: AudiologoTunables,
}

impl Default for DetectAudiologoStage {
    fn default() -> Self {
        Self::new(&AudiologoTunables::default())
    }
}

impl DetectAudiologoStage {
    /// Construct from runtime tunables. Captures the intro / outro
    /// scan window lengths up front so the stage doesn't need to
    /// re-read the live tunables on every book.
    #[must_use]
    pub fn new(tunables: &AudiologoTunables) -> Self {
        // intro/outro_window_secs are f64 seconds in tunables. The
        // cast saturates negative values (which a tunable should
        // never be) to 0 and large values to u64::MAX, both safe.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let intro_window_ms = (tunables.intro_window_secs.max(0.0) * 1000.0) as u64;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let outro_window_ms = (tunables.outro_window_secs.max(0.0) * 1000.0) as u64;
        Self {
            intro_window_ms,
            outro_window_ms,
            tunables: tunables.clone(),
        }
    }
}

/// Row from the `audiologos` table, decoded for matching.
//
// Fields beyond `audiologo_id` + `fingerprint` are reads-pending
// until the slide-match loop lands in 4B.4b. The `dead_code` allow
// captures the slice-ladder commitment: these fields are part of
// the data-plumbing contract finalized in 4B.4a even though only
// some are observed in tests today.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct AudiologoCandidate {
    pub audiologo_id: i64,
    pub kind: Kind,
    pub fingerprint: Vec<u32>,
    pub duration_ms: u64,
    pub match_threshold: f32,
}

/// Active book-file row, decoded for windowed sampling.
//
// Same dead-code situation as `AudiologoCandidate` above — the
// fields ship in 4B.4a and the slide-match consumers in 4B.4b
// will exercise them.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct ActiveBookFile {
    pub file_id: i64,
    pub file_path: String,
    /// File duration in ms. `None` when `book_files.duration_ms`
    /// is NULL (early-stage scan that hasn't probed durations
    /// yet); the outro window then can't be computed and the
    /// stage logs + skips outro detection for that file.
    pub duration_ms: Option<u64>,
}

/// Load every active file for `book_id`, ordered by `file_id` so
/// "first file" (intro target) and "last file" (outro target)
/// are stable.
pub(crate) async fn load_active_files(
    library: &LibraryDb,
    book_id: BookId,
) -> Result<Vec<ActiveBookFile>> {
    let id = book_id.0;
    let rows = sqlx::query!(
        r#"SELECT file_id AS "file_id!: i64", file_path, duration_ms
             FROM book_files
            WHERE book_id = ? AND is_active = 1
            ORDER BY file_id"#,
        id,
    )
    .fetch_all(library.pool())
    .await
    .map_err(|e| Error::Database(format!("detect-audiologo load files: {e}")))?;

    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let duration_ms = r.duration_ms.and_then(|ms| u64::try_from(ms).ok());
        out.push(ActiveBookFile {
            file_id: r.file_id,
            file_path: r.file_path,
            duration_ms,
        });
    }
    Ok(out)
}

/// Load every audiologo row matching `kind`, decoded for slide-
/// matching.
///
/// The fingerprint blob is little-endian-packed `Vec<u32>` (per
/// `ab_fingerprint::fingerprint_to_bytes`).
pub(crate) async fn load_audiologos_by_kind(
    library: &LibraryDb,
    kind: Kind,
) -> Result<Vec<AudiologoCandidate>> {
    let kind_str = kind.as_str();
    let rows = sqlx::query!(
        r#"SELECT audiologo_id AS "audiologo_id!: i64",
                  fingerprint,
                  duration_ms,
                  match_threshold
             FROM audiologos
            WHERE kind = ?
            ORDER BY audiologo_id"#,
        kind_str,
    )
    .fetch_all(library.pool())
    .await
    .map_err(|e| Error::Database(format!("detect-audiologo load audiologos: {e}")))?;

    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let fingerprint = ab_fingerprint::fingerprint_from_bytes(&r.fingerprint);
        if fingerprint.is_empty() {
            tracing::warn!(
                audiologo_id = r.audiologo_id,
                "audiologo.detect.empty_fingerprint_skipped"
            );
            continue;
        }
        let Ok(duration_ms) = u64::try_from(r.duration_ms) else {
            tracing::warn!(
                audiologo_id = r.audiologo_id,
                duration_ms = r.duration_ms,
                "audiologo.detect.negative_duration_skipped"
            );
            continue;
        };
        #[allow(clippy::cast_possible_truncation)]
        let match_threshold = r.match_threshold as f32;
        out.push(AudiologoCandidate {
            audiologo_id: r.audiologo_id,
            kind,
            fingerprint,
            duration_ms,
            match_threshold,
        });
    }
    Ok(out)
}

/// Best-match carrier for the slide-match loop in
/// [`detect_window`]. Module-scope so clippy's
/// `items_after_statements` doesn't trip on a struct inside the
/// function body.
struct BestHit {
    audiologo: AudiologoCandidate,
    pos: ab_fingerprint::MatchPos,
    confidence: f32,
    /// Which detection tier produced this hit. Drives the
    /// `method` column on the persisted candidate row + the
    /// auto-apply confidence floor in 4B.5.
    method: crate::Method,
    /// Bookend hits compute `(jingle_start_ms, jingle_end_ms)`
    /// from head + tail positions directly. Tier-1 full-match
    /// hits leave this `None` and let the caller derive end-ms
    /// from `audiologo.duration_ms`.
    end_ms_override: Option<(u64, u64)>,
}

/// Chromaprint hash-position to milliseconds factor for the
/// preset used by [`ab_fingerprint::fingerprint_samples`].
///
/// Each hash word covers `item_duration_in_seconds()` of audio;
/// at `preset_test1` defaults this is ~0.124 s. The conversion
/// is cached as a `u64` (rounded down) so the slide-match offset
/// translates with one multiplication.
fn chromaprint_item_duration_ms() -> u64 {
    // Local instantiation: `Configuration::preset_test1()` is
    // const-cheap; the call here is one-per-`run()`.
    let cfg = rusty_chromaprint::Configuration::preset_test1();
    let item_secs = cfg.item_duration_in_seconds();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let ms = (item_secs.max(0.0) * 1000.0) as u64;
    ms.max(1) // avoid 0 → division-by-zero in any downstream maths
}

/// Sample-fingerprint-slide-match one window of one file.
///
/// Returns the matched `audiologo_id` when a candidate row was
/// inserted, or None when the FFI sampler failed, fingerprinting
/// failed, or no audiologo cleared its match threshold.
///
/// Errors propagate only on database failure; sample / decode
/// failures log + return `Ok(None)` so a single bad file doesn't
/// fail the stage for the whole book.
#[allow(
    clippy::too_many_arguments,
    reason = "Bundling these into a config struct adds indirection that obscures the per-window dispatch; each arg is structural to the slide-match contract."
)]
#[allow(
    clippy::too_many_lines,
    reason = "Five linear steps (sample → check → fingerprint → slide-match → persist), each with its own structured error logging. Splitting into helpers fragments the read flow."
)]
async fn detect_window(
    ctx: &StageContext,
    book_id: BookId,
    file: &ActiveBookFile,
    kind: Kind,
    start_ms: u64,
    end_ms: u64,
    audiologos: &[AudiologoCandidate],
    item_dur_ms: u64,
    bookend_needle_secs: f64,
    bookend_max_gap_secs: f64,
) -> Result<Option<i64>> {
    // 1. Decode the window via AVAssetReader.
    let samples = match ab_audio::read_samples_window_typed(
        std::path::Path::new(&file.file_path),
        start_ms,
        end_ms,
        ab_fingerprint::AUDIOLOGO_SAMPLE_RATE,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                book = %book_id,
                file_id = file.file_id,
                kind = %kind,
                start_ms,
                end_ms,
                error = %e,
                "audiologo.detect.sample_failed"
            );
            return Ok(None);
        }
    };
    if samples.is_empty() {
        tracing::warn!(
            book = %book_id,
            file_id = file.file_id,
            kind = %kind,
            "audiologo.detect.empty_window"
        );
        return Ok(None);
    }

    // 2. Float32 → i16 → chromaprint hash sequence.
    let samples_i16 = ab_fingerprint::samples_f32_to_i16(&samples);
    let window_fp = match ab_fingerprint::fingerprint_samples(
        &samples_i16,
        ab_fingerprint::AUDIOLOGO_SAMPLE_RATE,
    ) {
        Ok(fp) => fp,
        Err(e) => {
            tracing::warn!(
                book = %book_id,
                file_id = file.file_id,
                kind = %kind,
                error = %e,
                "audiologo.detect.fingerprint_failed"
            );
            return Ok(None);
        }
    };
    if window_fp.is_empty() {
        return Ok(None);
    }

    // 3a. Tier 1 (FingerprintFull): slide-match each audiologo's
    //     full fingerprint; track the best hit above its
    //     row-specific threshold.
    let mut best: Option<BestHit> = None;
    for audiologo in audiologos {
        let Some(pos) = ab_fingerprint::slide_match(&window_fp, &audiologo.fingerprint) else {
            continue;
        };
        let conf =
            ab_fingerprint::confidence_from_hamming(pos.hamming, audiologo.fingerprint.len());
        if conf >= audiologo.match_threshold && best.as_ref().is_none_or(|b| conf > b.confidence) {
            best = Some(BestHit {
                audiologo: audiologo.clone(),
                pos,
                confidence: conf,
                method: crate::Method::FingerprintFull,
                end_ms_override: None,
            });
        }
    }

    // 3b. Tier 2 (FingerprintBookend): when Tier 1 misses, slice
    //     each audiologo's fingerprint into head + tail needles
    //     and try a bookend match. Useful when the jingle's
    //     middle voice line varies per book but the bookends
    //     stay stable (publisher catalogues do this routinely).
    if best.is_none() {
        let needle_hashes = secs_to_hash_count(bookend_needle_secs, item_dur_ms);
        let max_gap_hashes = secs_to_hash_count(bookend_max_gap_secs, item_dur_ms);
        for audiologo in audiologos {
            if audiologo.fingerprint.len() < needle_hashes * 2 {
                // Audiologo too short to split into non-overlapping bookends.
                continue;
            }
            let head_needle = &audiologo.fingerprint[..needle_hashes];
            let tail_start = audiologo.fingerprint.len() - needle_hashes;
            let tail_needle = &audiologo.fingerprint[tail_start..];
            let Some((head_pos, tail_pos)) = ab_fingerprint::slide_match_bookend(
                &window_fp,
                head_needle,
                tail_needle,
                Some(max_gap_hashes),
            ) else {
                continue;
            };
            // Bookend confidence: the weaker of the two ends.
            // A strong head + weak tail isn't a strong bookend.
            let head_conf =
                ab_fingerprint::confidence_from_hamming(head_pos.hamming, head_needle.len());
            let tail_conf =
                ab_fingerprint::confidence_from_hamming(tail_pos.hamming, tail_needle.len());
            let conf = head_conf.min(tail_conf);
            if conf >= audiologo.match_threshold
                && best.as_ref().is_none_or(|b| conf > b.confidence)
            {
                // Cut range spans the matched head start through
                // the matched tail end.
                let head_offset_ms = (head_pos.hash_offset as u64).saturating_mul(item_dur_ms);
                let tail_end_hashes = tail_pos.hash_offset + tail_needle.len();
                let tail_end_offset_ms = (tail_end_hashes as u64).saturating_mul(item_dur_ms);
                let jingle_start = start_ms.saturating_add(head_offset_ms);
                let jingle_end = start_ms.saturating_add(tail_end_offset_ms);
                best = Some(BestHit {
                    audiologo: audiologo.clone(),
                    pos: head_pos,
                    confidence: conf,
                    method: crate::Method::FingerprintBookend,
                    end_ms_override: Some((jingle_start, jingle_end)),
                });
            }
        }
    }

    let Some(hit) = best else {
        tracing::debug!(
            book = %book_id,
            file_id = file.file_id,
            kind = %kind,
            audiologos_tried = audiologos.len(),
            "audiologo.detect.no_match"
        );
        return Ok(None);
    };

    // 4. Convert hash-position offset → ms-since-file-start.
    //    Bookend hits carry an explicit `(start, end)` already
    //    computed from head + tail positions; full-match hits
    //    derive end from the audiologo's persisted duration.
    let (jingle_start_ms, jingle_end_ms) = if let Some(range) = hit.end_ms_override {
        range
    } else {
        let jingle_offset_ms = (hit.pos.hash_offset as u64).saturating_mul(item_dur_ms);
        let start = start_ms.saturating_add(jingle_offset_ms);
        (start, start.saturating_add(hit.audiologo.duration_ms))
    };

    // 5. Persist as `candidate`. 4B.5 promotes high-confidence
    //    auto-applying-Method rows to `applied` + does the
    //    chapter shift. Here we always insert at `candidate`,
    //    even for confidence above the auto-apply floor, so the
    //    slice boundary is clean.
    insert_candidate_row(
        &ctx.library,
        file.file_id,
        kind,
        jingle_start_ms,
        jingle_end_ms,
        hit.audiologo.audiologo_id,
        hit.confidence,
        hit.method,
    )
    .await?;
    bump_audiologo_match_count(&ctx.library, hit.audiologo.audiologo_id).await?;

    tracing::info!(
        book = %book_id,
        file_id = file.file_id,
        kind = %kind,
        audiologo_id = hit.audiologo.audiologo_id,
        method = hit.method.as_str(),
        confidence = hit.confidence,
        hash_offset = hit.pos.hash_offset,
        hamming = hit.pos.hamming,
        jingle_start_ms,
        jingle_end_ms,
        "audiologo.detect.candidate_inserted"
    );

    Ok(Some(hit.audiologo.audiologo_id))
}

/// Convert a tunable's seconds value into a hash count for the
/// chromaprint preset in use. Floors at 1 so a too-tight tunable
/// can't produce a zero-length needle (which `slide_match`
/// would reject anyway).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn secs_to_hash_count(secs: f64, item_dur_ms: u64) -> usize {
    if item_dur_ms == 0 {
        return 1;
    }
    let ms = (secs.max(0.0) * 1000.0) as u64;
    let hashes = (ms / item_dur_ms).max(1);
    usize::try_from(hashes).unwrap_or(usize::MAX)
}

/// Insert a fresh `book_file_audiologos` candidate row.
#[allow(
    clippy::too_many_arguments,
    reason = "Each column is a structural input to the INSERT; a struct here would just add a definition for no clarity gain."
)]
async fn insert_candidate_row(
    library: &LibraryDb,
    file_id: i64,
    kind: Kind,
    jingle_start_ms: u64,
    jingle_end_ms: u64,
    audiologo_id: i64,
    confidence: f32,
    method: crate::Method,
) -> Result<()> {
    let kind_str = kind.as_str();
    let method_str = method.as_str();
    let status_str = Status::Candidate.as_str();
    let start_i64 = i64::try_from(jingle_start_ms).unwrap_or(i64::MAX);
    let end_i64 = i64::try_from(jingle_end_ms).unwrap_or(i64::MAX);
    let conf_f64 = f64::from(confidence);
    sqlx::query!(
        r#"INSERT INTO book_file_audiologos
             (file_id, kind, jingle_start_ms, jingle_end_ms,
              padding_ms, method, audiologo_id, confidence, status)
           VALUES (?, ?, ?, ?, NULL, ?, ?, ?, ?)"#,
        file_id,
        kind_str,
        start_i64,
        end_i64,
        method_str,
        audiologo_id,
        conf_f64,
        status_str,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("detect-audiologo insert candidate: {e}")))?;
    Ok(())
}

/// Bump the matched `audiologos` row's `match_count` + `last_matched_at`.
async fn bump_audiologo_match_count(library: &LibraryDb, audiologo_id: i64) -> Result<()> {
    sqlx::query!(
        "UPDATE audiologos \
            SET match_count = match_count + 1, \
                last_matched_at = strftime('%s','now'), \
                updated_at = strftime('%s','now') \
          WHERE audiologo_id = ?",
        audiologo_id,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("detect-audiologo bump match_count: {e}")))?;
    Ok(())
}

/// Persist a transcript-aided candidate row. Idempotent: skips
/// the insert when a row at the same `(file_id, kind, status)`
/// already exists (e.g. fingerprint detection inserted one in
/// the same `run()` pass).
///
/// Returns `true` when a fresh row was inserted, `false` when
/// the candidate was a duplicate (so the caller doesn't
/// double-count `any_candidate`).
async fn persist_transcript_candidate(
    library: &LibraryDb,
    file_id: i64,
    cand: &crate::TranscriptCandidate,
) -> Result<bool> {
    let kind_str = cand.kind.as_str();
    let candidate_status = Status::Candidate.as_str();
    let exists: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM book_file_audiologos \
         WHERE file_id = ? AND kind = ? AND status = ? LIMIT 1",
    )
    .bind(file_id)
    .bind(kind_str)
    .bind(candidate_status)
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("transcript candidate dedupe: {e}")))?;
    if exists.is_some() {
        return Ok(false);
    }
    let method_str = cand.method.as_str();
    let start_i64 = i64::try_from(cand.jingle_start_ms).unwrap_or(i64::MAX);
    let end_i64 = i64::try_from(cand.jingle_end_ms).unwrap_or(i64::MAX);
    let conf_f64 = f64::from(cand.confidence);
    sqlx::query!(
        r#"INSERT INTO book_file_audiologos
             (file_id, kind, jingle_start_ms, jingle_end_ms,
              padding_ms, method, audiologo_id, confidence, status)
           VALUES (?, ?, ?, ?, NULL, ?, NULL, ?, ?)"#,
        file_id,
        kind_str,
        start_i64,
        end_i64,
        method_str,
        conf_f64,
        candidate_status,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("transcript candidate insert: {e}")))?;
    tracing::info!(
        file_id,
        kind = %cand.kind,
        method = %cand.method,
        publisher_hint = ?cand.publisher_hint,
        confidence = cand.confidence,
        "audiologo.transcript.candidate_inserted"
    );
    Ok(true)
}

/// Update `books.audiologo_status` to the given value.
async fn update_book_audiologo_status(
    library: &LibraryDb,
    book_id: BookId,
    status: crate::BookStatus,
) -> Result<()> {
    let id = book_id.0;
    let status_str = status.as_str();
    sqlx::query!(
        "UPDATE books SET audiologo_status = ? WHERE book_id = ?",
        status_str,
        id,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("detect-audiologo update book status: {e}")))?;
    Ok(())
}

/// Fetch Audnexus's reported brand-intro duration for the book, if
/// any. Slice 4B.0 promotes this from the audnexus-chapters stage's
/// response into `books.brand_intro_duration_ms`. Used by 4B for
/// the Libation-stripped path (non-NULL brand duration + no
/// fingerprint hit → `audiologo_status='stripped'`).
pub(crate) async fn fetch_brand_intro_duration_ms(
    library: &LibraryDb,
    book_id: BookId,
) -> Result<Option<u64>> {
    let id = book_id.0;
    let row = sqlx::query!(
        "SELECT brand_intro_duration_ms FROM books WHERE book_id = ?",
        id,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("detect-audiologo fetch brand: {e}")))?;
    Ok(row
        .and_then(|r| r.brand_intro_duration_ms)
        .and_then(|ms| u64::try_from(ms).ok()))
}

#[async_trait]
impl Stage for DetectAudiologoStage {
    fn name(&self) -> &'static str {
        STAGE_NAME
    }

    fn requires(&self) -> &'static [StageId] {
        // Transcript-aided tiers (4C) need the head/tail transcript
        // + sample-window transcripts. Even though 4B.3 itself
        // doesn't read transcripts, locking the requires() list
        // here means the slice ladder doesn't reshuffle dependency
        // edges as later sub-slices land — easier on the scheduler
        // + retry surface to know the full predecessor set early.
        const REQS: &[StageId] = &[
            ab_transcript::stage::STAGE_ID,
            ab_transcript::samples_stage::STAGE_ID,
        ];
        REQS
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Top-level orchestration: load → intro-detect → outro-detect → book-status update. Extracting helpers fragments the read flow without simplifying the control flow."
    )]
    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        let files = load_active_files(&ctx.library, book_id).await?;
        if files.is_empty() {
            return Ok(StageOutcome::Skipped);
        }
        let intros = load_audiologos_by_kind(&ctx.library, Kind::Intro).await?;
        let outros = load_audiologos_by_kind(&ctx.library, Kind::Outro).await?;
        let brand_intro_ms = fetch_brand_intro_duration_ms(&ctx.library, book_id).await?;

        let item_dur_ms = chromaprint_item_duration_ms();

        tracing::debug!(
            book = %book_id,
            stage = STAGE_NAME,
            files = files.len(),
            intro_audiologos = intros.len(),
            outro_audiologos = outros.len(),
            brand_intro_ms = ?brand_intro_ms,
            intro_window_ms = self.intro_window_ms,
            outro_window_ms = self.outro_window_ms,
            "audiologo.detect.start"
        );

        let mut any_candidate = false;

        // Intro detection on the first file. Single-file books
        // hit only this branch (intro AND outro can both target
        // the same file — the outro branch below kicks in too).
        if let Some(first) = files.first() {
            if !intros.is_empty() && self.intro_window_ms > 0 {
                let hit = detect_window(
                    ctx,
                    book_id,
                    first,
                    Kind::Intro,
                    0,
                    self.intro_window_ms,
                    &intros,
                    item_dur_ms,
                    self.tunables.fp_bookend_needle_secs,
                    self.tunables.fp_bookend_max_gap_secs,
                )
                .await?;
                if hit.is_some() {
                    any_candidate = true;
                }
            }
        }

        // Outro detection on the last file.
        if let Some(last) = files.last() {
            if !outros.is_empty() && self.outro_window_ms > 0 {
                match last.duration_ms {
                    Some(file_dur) if file_dur > 0 => {
                        let outro_start = file_dur.saturating_sub(self.outro_window_ms);
                        let outro_end = file_dur;
                        if outro_end > outro_start {
                            let hit = detect_window(
                                ctx,
                                book_id,
                                last,
                                Kind::Outro,
                                outro_start,
                                outro_end,
                                &outros,
                                item_dur_ms,
                                self.tunables.fp_bookend_needle_secs,
                                self.tunables.fp_bookend_max_gap_secs,
                            )
                            .await?;
                            if hit.is_some() {
                                any_candidate = true;
                            }
                        }
                    }
                    _ => {
                        tracing::warn!(
                            book = %book_id,
                            file_id = last.file_id,
                            "audiologo.detect.outro_skipped_no_duration"
                        );
                    }
                }
            }
        }

        // Slice 4C: transcript-aided detection. Runs after the
        // fingerprint pass; produces TranscriptOnly candidates
        // (or FingerprintAndTranscript when corroborating with
        // an existing fingerprint candidate row from THIS run's
        // intro / outro file). Always candidate-only — these
        // tiers never auto-apply per ADR-0024.
        let intro_transcript_hit =
            crate::transcript_aided::detect_intro_via_transcript(&ctx.library, book_id).await?;
        let outro_transcript_hit =
            crate::transcript_aided::detect_outro_via_transcript(&ctx.library, book_id).await?;

        if let Some(first) = files.first() {
            if let Some(cand) = intro_transcript_hit {
                if persist_transcript_candidate(&ctx.library, first.file_id, &cand).await? {
                    any_candidate = true;
                }
            }
        }
        if let Some(last) = files.last() {
            if let Some(cand) = outro_transcript_hit {
                if persist_transcript_candidate(&ctx.library, last.file_id, &cand).await? {
                    any_candidate = true;
                }
            }
        }

        if any_candidate {
            update_book_audiologo_status(&ctx.library, book_id, crate::BookStatus::Detected)
                .await?;
        }

        // Auto-apply path (4B.5): promote candidate rows whose
        // method auto-applies AND whose confidence clears the
        // per-method tunable floor to `applied`, shifting
        // chapter offsets. Always runs — picks up rows from this
        // detection pass, from prior detection runs (e.g. after
        // a tunable change), and from the 4A manual-cut CLI path.
        // No-op when no candidates exist.
        let promoted =
            crate::apply::apply_auto_applicable_candidates(&ctx.library, book_id, &self.tunables)
                .await?;

        // Libation-stripped path (4B.5): brand_intro_duration_ms
        // is non-NULL but no fingerprint hit landed → audio has
        // been pre-stripped elsewhere. Shift chapters by
        // -brand_intro_ms and set status='stripped'. Idempotent.
        let libation_applied = if !any_candidate && promoted == 0 {
            match brand_intro_ms {
                Some(brand_ms) => {
                    crate::apply::apply_libation_stripped(&ctx.library, book_id, brand_ms).await?
                }
                None => false,
            }
        } else {
            false
        };

        if any_candidate || promoted > 0 || libation_applied {
            tracing::info!(
                book = %book_id,
                any_candidate,
                promoted,
                libation_applied,
                "audiologo.detect.done"
            );
            Ok(StageOutcome::Done)
        } else {
            tracing::info!(
                book = %book_id,
                brand_intro_ms = ?brand_intro_ms,
                "audiologo.detect.no_candidates"
            );
            Ok(StageOutcome::Skipped)
        }
    }

    /// Per ADR-0024 § state-machine diagram. Flips `applied` rows
    /// for this book to `re_detected` (preserving the audit trail),
    /// NULLs `books.audiologo_status` back to its default, and
    /// then delegates to `default_reset` for the
    /// `pipeline_progress` / `book_field_provenance` / `ai_cache`
    /// cleanup.
    ///
    /// `candidate` / `rejected` rows are left intact — see the
    /// module docstring for the rationale.
    async fn reset(&self, ctx: &StageContext, book_id: BookId) -> Result<()> {
        let id = book_id.0;
        let applied = Status::Applied.as_str();
        let re_detected = Status::ReDetected.as_str();
        let unknown = crate::BookStatus::Unknown.as_str();

        let mut tx = ctx
            .library
            .pool()
            .begin()
            .await
            .map_err(|e| Error::Database(format!("detect-audiologo reset tx: {e}")))?;

        sqlx::query!(
            "UPDATE book_file_audiologos \
                SET status = ?, \
                    re_detected_at = strftime('%s','now') \
              WHERE file_id IN ( \
                  SELECT file_id FROM book_files WHERE book_id = ? \
              ) \
                AND status = ?",
            re_detected,
            id,
            applied,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| Error::Database(format!("detect-audiologo reset audiologo rows: {e}")))?;

        sqlx::query!(
            "UPDATE books SET audiologo_status = ? WHERE book_id = ?",
            unknown,
            id,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| Error::Database(format!("detect-audiologo reset book status: {e}")))?;

        tx.commit()
            .await
            .map_err(|e| Error::Database(format!("detect-audiologo reset commit: {e}")))?;

        ab_pipeline::default_reset(STAGE_NAME, ctx, book_id).await
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use ab_core::tunables::DbTunables;
    use ab_db::{EphemeralDb, LibraryDb};
    use std::path::Path;
    use tempfile::TempDir;

    async fn fresh_ctx(dir: &Path) -> StageContext {
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

    fn fresh_stage() -> DetectAudiologoStage {
        DetectAudiologoStage::new(&AudiologoTunables::default())
    }

    #[tokio::test]
    async fn stage_metadata_matches_pipeline_expectations() {
        let stage = fresh_stage();
        assert_eq!(stage.name(), "detect-audiologo");
        assert_eq!(
            stage.requires(),
            &[
                ab_transcript::stage::STAGE_ID,
                ab_transcript::samples_stage::STAGE_ID,
            ]
        );
    }

    #[tokio::test]
    async fn tunables_window_ms_derives_from_secs() {
        let t = AudiologoTunables {
            intro_window_secs: 120.0,
            outro_window_secs: 60.0,
            ..AudiologoTunables::default()
        };
        let stage = DetectAudiologoStage::new(&t);
        assert_eq!(stage.intro_window_ms, 120_000);
        assert_eq!(stage.outro_window_ms, 60_000);
    }

    #[tokio::test]
    async fn tunables_window_ms_clamps_negative_to_zero() {
        let t = AudiologoTunables {
            intro_window_secs: -5.0,
            outro_window_secs: -1.0,
            ..AudiologoTunables::default()
        };
        let stage = DetectAudiologoStage::new(&t);
        assert_eq!(stage.intro_window_ms, 0);
        assert_eq!(stage.outro_window_ms, 0);
    }

    #[tokio::test]
    async fn run_returns_skipped_with_no_files() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        // No book seeded → no files → Skipped.
        let outcome = fresh_stage()
            .run(&ctx, BookId(1))
            .await
            .expect("run does not error");
        match outcome {
            StageOutcome::Skipped => {}
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_skipped_with_no_audiologos_in_table() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        // Seed: one book + one active file. No audiologos table
        // rows → detection has nothing to match against → 4B.4a
        // still returns Skipped (4B.4b will still Skipped here
        // since there's nothing to fingerprint against).
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'fixture')")
            .execute(ctx.library.pool())
            .await
            .expect("seed book");
        sqlx::query(
            "INSERT INTO book_files (file_id, book_id, file_path, is_active, duration_ms) \
             VALUES (10, 1, '/tmp/a.m4b', 1, 600000)",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed file");
        let outcome = fresh_stage()
            .run(&ctx, BookId(1))
            .await
            .expect("run does not error");
        match outcome {
            StageOutcome::Skipped => {}
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn load_active_files_returns_only_active_ordered_by_id() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'fixture')")
            .execute(ctx.library.pool())
            .await
            .expect("seed book");
        sqlx::query(
            "INSERT INTO book_files (file_id, book_id, file_path, is_active, duration_ms) \
             VALUES \
             (10, 1, '/tmp/a.m4b', 1, 100000), \
             (11, 1, '/tmp/b.m4b', 0, 200000), \
             (12, 1, '/tmp/c.m4b', 1, NULL)",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed files");
        let files = load_active_files(&ctx.library, BookId(1))
            .await
            .expect("load");
        assert_eq!(files.len(), 2, "only the two is_active=1 rows");
        assert_eq!(files[0].file_id, 10);
        assert_eq!(files[0].duration_ms, Some(100_000));
        assert_eq!(files[1].file_id, 12);
        assert_eq!(files[1].duration_ms, None, "NULL duration → None");
    }

    #[tokio::test]
    async fn load_audiologos_by_kind_filters_and_decodes_fingerprint() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        let intro_fp = ab_fingerprint::fingerprint_to_bytes(&[1_u32, 2, 3]);
        let outro_fp = ab_fingerprint::fingerprint_to_bytes(&[7_u32, 8]);
        sqlx::query(
            "INSERT INTO audiologos \
             (audiologo_id, name, kind, fingerprint, duration_ms, match_threshold, verified_via) \
             VALUES \
             (1, 'intro-A', 'intro', ?, 5000, 0.85, 'seed'), \
             (2, 'outro-A', 'outro', ?, 4000, 0.80, 'seed')",
        )
        .bind(&intro_fp)
        .bind(&outro_fp)
        .execute(ctx.library.pool())
        .await
        .expect("seed audiologos");

        let intros = load_audiologos_by_kind(&ctx.library, Kind::Intro)
            .await
            .expect("load intros");
        assert_eq!(intros.len(), 1);
        assert_eq!(intros[0].audiologo_id, 1);
        assert_eq!(intros[0].fingerprint, vec![1_u32, 2, 3]);
        assert_eq!(intros[0].duration_ms, 5000);
        assert!((intros[0].match_threshold - 0.85).abs() < 1e-3);

        let outros = load_audiologos_by_kind(&ctx.library, Kind::Outro)
            .await
            .expect("load outros");
        assert_eq!(outros.len(), 1);
        assert_eq!(outros[0].fingerprint, vec![7_u32, 8]);
    }

    #[tokio::test]
    async fn fetch_brand_intro_duration_returns_none_when_null() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'fixture')")
            .execute(ctx.library.pool())
            .await
            .expect("seed");
        let v = fetch_brand_intro_duration_ms(&ctx.library, BookId(1))
            .await
            .expect("fetch");
        assert_eq!(v, None);
    }

    #[tokio::test]
    async fn chromaprint_item_duration_is_positive_few_hundred_ms() {
        // preset_test1 sits near 124 ms/item; the helper rounds
        // down and floors at 1 to avoid div-by-zero downstream.
        // The exact value isn't a contract — what matters is that
        // it's small enough to give sub-second resolution at a
        // 60-120 s window scale.
        let ms = chromaprint_item_duration_ms();
        assert!(ms >= 1, "must be >=1 ms to avoid div-by-zero math");
        assert!(
            ms < 1000,
            "preset_test1 should give <1 s per item (got {ms} ms)"
        );
    }

    #[tokio::test]
    async fn run_no_match_skipped_when_no_fingerprints_align() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        // Seed: one book + one active file that does NOT exist on
        // disk. The FFI sampler will fail to load it; detect_window
        // swallows the error + returns None; the stage returns
        // Skipped. We're not checking that the FFI succeeded —
        // only that a sample failure doesn't propagate as a stage
        // error.
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'fixture')")
            .execute(ctx.library.pool())
            .await
            .expect("seed book");
        sqlx::query(
            "INSERT INTO book_files (file_id, book_id, file_path, is_active, duration_ms) \
             VALUES (10, 1, '/tmp/does-not-exist.m4b', 1, 600000)",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed file");

        // Add an audiologo so the load is non-empty and the
        // detect_window path runs.
        let dummy_fp = ab_fingerprint::fingerprint_to_bytes(&[1_u32, 2, 3, 4]);
        sqlx::query(
            "INSERT INTO audiologos \
             (audiologo_id, name, kind, fingerprint, duration_ms, match_threshold, verified_via) \
             VALUES (1, 'intro-A', 'intro', ?, 5000, 0.85, 'seed')",
        )
        .bind(&dummy_fp)
        .execute(ctx.library.pool())
        .await
        .expect("seed audiologo");

        let outcome = fresh_stage()
            .run(&ctx, BookId(1))
            .await
            .expect("run does not propagate FFI errors");
        match outcome {
            StageOutcome::Skipped => {}
            other => panic!("expected Skipped on FFI-sample-failure path, got {other:?}"),
        }

        // No candidate row inserted.
        let candidates: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM book_file_audiologos WHERE file_id = 10")
                .fetch_one(ctx.library.pool())
                .await
                .expect("count");
        assert_eq!(candidates, 0);

        // Book status untouched (still 'unknown' default).
        let status: String =
            sqlx::query_scalar("SELECT audiologo_status FROM books WHERE book_id = 1")
                .fetch_one(ctx.library.pool())
                .await
                .expect("status");
        assert_eq!(status, "unknown");
    }

    #[tokio::test]
    async fn update_book_audiologo_status_writes_the_value() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'fixture')")
            .execute(ctx.library.pool())
            .await
            .expect("seed");

        update_book_audiologo_status(&ctx.library, BookId(1), crate::BookStatus::Detected)
            .await
            .expect("update");

        let status: String =
            sqlx::query_scalar("SELECT audiologo_status FROM books WHERE book_id = 1")
                .fetch_one(ctx.library.pool())
                .await
                .expect("status");
        assert_eq!(status, "detected");
    }

    #[tokio::test]
    async fn insert_candidate_row_persists_with_correct_fields() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'fixture')")
            .execute(ctx.library.pool())
            .await
            .expect("seed book");
        sqlx::query(
            "INSERT INTO book_files (file_id, book_id, file_path, is_active, duration_ms) \
             VALUES (10, 1, '/tmp/x.m4b', 1, 600000)",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed file");
        let fp = ab_fingerprint::fingerprint_to_bytes(&[1_u32, 2, 3]);
        sqlx::query(
            "INSERT INTO audiologos \
             (audiologo_id, name, kind, fingerprint, duration_ms, match_threshold, verified_via) \
             VALUES (42, 'intro-A', 'intro', ?, 4500, 0.85, 'seed')",
        )
        .bind(&fp)
        .execute(ctx.library.pool())
        .await
        .expect("seed audiologo");

        insert_candidate_row(
            &ctx.library,
            10,
            Kind::Intro,
            250,
            4_750,
            42,
            0.92,
            crate::Method::FingerprintFull,
        )
        .await
        .expect("insert");

        let (file_id, kind, start, end, audiologo_id, conf, status, method): (
            i64,
            String,
            i64,
            i64,
            Option<i64>,
            f64,
            String,
            String,
        ) = sqlx::query_as(
            "SELECT file_id, kind, jingle_start_ms, jingle_end_ms, audiologo_id, \
                    confidence, status, method \
               FROM book_file_audiologos WHERE file_id = 10",
        )
        .fetch_one(ctx.library.pool())
        .await
        .expect("fetch");
        assert_eq!(file_id, 10);
        assert_eq!(kind, "intro");
        assert_eq!(start, 250);
        assert_eq!(end, 4_750);
        assert_eq!(audiologo_id, Some(42));
        assert!((conf - 0.92).abs() < 1e-3);
        assert_eq!(status, "candidate");
        assert_eq!(method, "fingerprint_full");
    }

    #[tokio::test]
    async fn bump_audiologo_match_count_increments_counter() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        let fp = ab_fingerprint::fingerprint_to_bytes(&[1_u32]);
        sqlx::query(
            "INSERT INTO audiologos \
             (audiologo_id, name, kind, fingerprint, duration_ms, match_threshold, verified_via, match_count) \
             VALUES (7, 'intro-A', 'intro', ?, 1000, 0.85, 'seed', 5)",
        )
        .bind(&fp)
        .execute(ctx.library.pool())
        .await
        .expect("seed");

        bump_audiologo_match_count(&ctx.library, 7)
            .await
            .expect("bump");

        let (count, last_matched_at): (i64, Option<i64>) = sqlx::query_as(
            "SELECT match_count, last_matched_at FROM audiologos WHERE audiologo_id = 7",
        )
        .fetch_one(ctx.library.pool())
        .await
        .expect("fetch");
        assert_eq!(count, 6, "5 → 6");
        assert!(last_matched_at.is_some(), "last_matched_at populated");
    }

    #[tokio::test]
    async fn run_auto_applies_pre_existing_candidate_rows() {
        // Integration test: seed a candidate row from a prior
        // detection run (or a manual cut via the 4A CLI) and
        // verify that run() promotes it through the auto-apply
        // path even though *this* detection pass produces no
        // hits (the seeded file doesn't exist on disk). Covers
        // the slice-4B.6 fix that decouples auto-apply from
        // any_candidate-in-this-run.
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;

        sqlx::query(
            "INSERT INTO books (book_id, title, duration_ms, raw_duration_ms) \
             VALUES (1, 'fixture', 3_600_000, 3_600_000)",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed book");
        sqlx::query(
            "INSERT INTO book_files (file_id, book_id, file_path, is_active, duration_ms) \
             VALUES (10, 1, '/tmp/does-not-exist.m4b', 1, 3_600_000)",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed file");
        // Pre-existing high-confidence FingerprintFull candidate.
        sqlx::query(
            "INSERT INTO book_file_audiologos \
             (audiologo_row_id, file_id, kind, jingle_start_ms, jingle_end_ms, \
              padding_ms, method, confidence, status) \
             VALUES (100, 10, 'intro', 0, 5000, NULL, 'fingerprint_full', 0.95, 'candidate')",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed candidate");
        // Chapters that will get shifted.
        sqlx::query(
            "INSERT INTO chapters (book_id, idx, start_ms, end_ms, title, source) \
             VALUES (1, 0, 5_000, 60_000, 'Ch1', 'audnexus')",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed chapters");

        let outcome = fresh_stage()
            .run(&ctx, BookId(1))
            .await
            .expect("run does not error");
        match outcome {
            StageOutcome::Done => {}
            other => panic!("expected Done when auto-apply runs, got {other:?}"),
        }

        // Candidate promoted to applied.
        let status: String = sqlx::query_scalar(
            "SELECT status FROM book_file_audiologos WHERE audiologo_row_id = 100",
        )
        .fetch_one(ctx.library.pool())
        .await
        .expect("status");
        assert_eq!(status, "applied");

        // Book status flipped to applied.
        let book_status: String =
            sqlx::query_scalar("SELECT audiologo_status FROM books WHERE book_id = 1")
                .fetch_one(ctx.library.pool())
                .await
                .expect("book status");
        assert_eq!(book_status, "applied");

        // Chapter shifted.
        let start: i64 =
            sqlx::query_scalar("SELECT start_ms FROM chapters WHERE book_id = 1 AND idx = 0")
                .fetch_one(ctx.library.pool())
                .await
                .expect("chapter start");
        // cut_ms = (5000 - 0) - 250 = 4750. chapter at 5000 →
        // start_ms >= jingle_end_ms (5000 >= 5000), so case-1
        // fires → 5000 - 4750 = 250.
        assert_eq!(start, 250);
    }

    #[tokio::test]
    async fn run_applies_libation_path_when_brand_set_and_no_hit() {
        // brand_intro_duration_ms is non-NULL; the audiologos
        // table has an intro entry whose fingerprint won't match
        // /dev/null. Stage should NOT insert a candidate row,
        // but SHOULD fire the Libation-stripped path and flip
        // book status to 'stripped' + shift chapters.
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        sqlx::query(
            "INSERT INTO books (book_id, title, duration_ms, raw_duration_ms, brand_intro_duration_ms) \
             VALUES (1, 'fixture', 3_600_000, 3_600_000, 4_500)",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed book");
        sqlx::query(
            "INSERT INTO book_files (file_id, book_id, file_path, is_active, duration_ms) \
             VALUES (10, 1, '/tmp/does-not-exist.m4b', 1, 3_600_000)",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed file");
        // An audiologos intro row so the load path is non-empty
        // and the detect_window code runs (will fail FFI → no
        // candidate inserted).
        let fp = ab_fingerprint::fingerprint_to_bytes(&[1_u32, 2, 3]);
        sqlx::query(
            "INSERT INTO audiologos \
             (audiologo_id, name, kind, fingerprint, duration_ms, match_threshold, verified_via) \
             VALUES (1, 'intro-A', 'intro', ?, 5000, 0.85, 'seed')",
        )
        .bind(&fp)
        .execute(ctx.library.pool())
        .await
        .expect("seed audiologo");
        sqlx::query(
            "INSERT INTO chapters (book_id, idx, start_ms, end_ms, title, source) \
             VALUES (1, 0, 5_000, 60_000, 'Ch1', 'audnexus')",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed chapters");

        let outcome = fresh_stage()
            .run(&ctx, BookId(1))
            .await
            .expect("run does not error");
        match outcome {
            StageOutcome::Done => {}
            other => panic!("expected Done on libation path, got {other:?}"),
        }

        // No candidate row.
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM book_file_audiologos WHERE file_id = 10")
                .fetch_one(ctx.library.pool())
                .await
                .expect("count");
        assert_eq!(count, 0);

        // Book status flipped to stripped.
        let book_status: String =
            sqlx::query_scalar("SELECT audiologo_status FROM books WHERE book_id = 1")
                .fetch_one(ctx.library.pool())
                .await
                .expect("status");
        assert_eq!(book_status, "stripped");

        // Chapter shifted by -4500.
        let start: i64 =
            sqlx::query_scalar("SELECT start_ms FROM chapters WHERE book_id = 1 AND idx = 0")
                .fetch_one(ctx.library.pool())
                .await
                .expect("start");
        assert_eq!(start, 500, "5000 - 4500 = 500");
    }

    #[tokio::test]
    async fn fetch_brand_intro_duration_returns_value_when_set() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        sqlx::query(
            "INSERT INTO books (book_id, title, brand_intro_duration_ms) VALUES (1, 'fixture', 4321)",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed");
        let v = fetch_brand_intro_duration_ms(&ctx.library, BookId(1))
            .await
            .expect("fetch");
        assert_eq!(v, Some(4321));
    }

    #[tokio::test]
    async fn reset_flips_applied_rows_to_re_detected() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;

        // Seed: one book, one file, two audiologo rows (one
        // applied, one candidate). Reset should flip the applied
        // one + leave the candidate alone.
        sqlx::query(
            "INSERT INTO books (book_id, title, audiologo_status) VALUES (1, 'fixture', 'applied')",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed book");
        sqlx::query("INSERT INTO book_files (file_id, book_id, file_path, is_active) VALUES (10, 1, '/tmp/a.m4b', 1)")
            .execute(ctx.library.pool())
            .await
            .expect("seed file");
        sqlx::query(
            "INSERT INTO book_file_audiologos \
             (audiologo_row_id, file_id, kind, jingle_start_ms, jingle_end_ms, padding_ms, method, audiologo_id, confidence, status) \
             VALUES \
             (100, 10, 'intro', 0, 5000, 250, 'fingerprint_full', NULL, 0.9, 'applied'), \
             (101, 10, 'outro', 0, 5000, 250, 'fingerprint_full', NULL, 0.6, 'candidate')",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed audiologo rows");
        sqlx::query(
            "INSERT INTO pipeline_progress (book_id, stage, status, started_at, completed_at) \
             VALUES (1, 'detect-audiologo', 'succeeded', 0, 1)",
        )
        .execute(ctx.ephemeral.pool())
        .await
        .expect("seed progress");

        fresh_stage().reset(&ctx, BookId(1)).await.expect("reset");

        let row_100_status: String = sqlx::query_scalar(
            "SELECT status FROM book_file_audiologos WHERE audiologo_row_id = 100",
        )
        .fetch_one(ctx.library.pool())
        .await
        .expect("fetch 100");
        assert_eq!(row_100_status, "re_detected", "applied → re_detected");

        let row_100_re_detected_at: Option<i64> = sqlx::query_scalar(
            "SELECT re_detected_at FROM book_file_audiologos WHERE audiologo_row_id = 100",
        )
        .fetch_one(ctx.library.pool())
        .await
        .expect("fetch ts");
        assert!(
            row_100_re_detected_at.is_some(),
            "re_detected_at must be populated"
        );

        let row_101_status: String = sqlx::query_scalar(
            "SELECT status FROM book_file_audiologos WHERE audiologo_row_id = 101",
        )
        .fetch_one(ctx.library.pool())
        .await
        .expect("fetch 101");
        assert_eq!(row_101_status, "candidate", "candidate row untouched");

        let book_status: String =
            sqlx::query_scalar("SELECT audiologo_status FROM books WHERE book_id = 1")
                .fetch_one(ctx.library.pool())
                .await
                .expect("fetch book status");
        assert_eq!(book_status, "unknown", "book status reset to unknown");

        let progress_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pipeline_progress WHERE book_id = 1 AND stage = 'detect-audiologo'",
        )
        .fetch_one(ctx.ephemeral.pool())
        .await
        .expect("count progress");
        assert_eq!(progress_count, 0, "default_reset clears pipeline_progress");
    }
}
