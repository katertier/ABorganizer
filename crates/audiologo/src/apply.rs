//! Apply-an-audiologo-cut helpers (slice 4B.5).
//!
//! Two entry points:
//!
//! - [`apply_auto_applicable_candidates`] promotes every
//!   `book_file_audiologos` row at `status='candidate'` whose
//!   method auto-applies AND whose confidence clears the
//!   per-method tunable floor to `status='applied'`, shifting
//!   chapter offsets to compensate for the cut. Returns the
//!   count of rows promoted.
//! - [`apply_libation_stripped`] handles the case where Audnexus
//!   reported a `brand_intro_duration_ms` but the head-window
//!   fingerprint pass found no match: the audio has been
//!   pre-stripped (e.g. Libation). Sets
//!   `books.audiologo_status='stripped'` and shifts every chapter
//!   offset by `-brand_intro_duration_ms` (treating the absent
//!   jingle as if just cut). No `book_file_audiologos` row is
//!   inserted — the cut never happened in our pipeline; the
//!   audio is already content-aligned.
//!
//! ## Chapter-shift maths (per ADR-0024 § Chapter recomputation)
//!
//! For an applied row with `[jingle_start_ms, jingle_end_ms]`
//! and effective `cut_ms = (jingle_end_ms - jingle_start_ms) -
//! padding_ms`, every chapter row for the affected file at any
//! `source` shifts:
//!
//! - If `chapter.start_ms >= jingle_end_ms` → both
//!   `start_ms` and `end_ms` decrease by `cut_ms`.
//! - If `chapter.start_ms < jingle_start_ms` AND
//!   `chapter.end_ms > jingle_end_ms` (chapter spans the
//!   cut) → only `end_ms` decreases by `cut_ms`.
//! - Otherwise unchanged.
//!
//! `books.duration_ms` decreases by the sum of `cut_ms` across
//! all applied rows for the book.
//!
//! ## What this slice deliberately does not do
//!
//! - Transcript / silence boundary verification
//!   (`chapters.boundary_verified` stays NULL). The flag is
//!   populated in a future slice that does the cross-reference
//!   against the transcript.
//! - `match_count`-driven `verified_via` promotion
//!   (`silence` → `review_confirmed`). That's the 4D review
//!   workflow.

use ab_core::tunables::AudiologoTunables;
use ab_core::{BookId, Error, Result};
use ab_db::LibraryDb;
use sqlx::{Sqlite, Transaction};

use crate::{BookStatus, Kind, Method, Status};

/// One candidate row ready for promotion decision.
#[derive(Debug, Clone)]
struct CandidateRow {
    row_id: i64,
    file_id: i64,
    kind: Kind,
    jingle_start_ms: u64,
    jingle_end_ms: u64,
    padding_ms: Option<u32>,
    method: Method,
    confidence: f32,
}

/// Promote auto-applicable candidate rows to `applied` + shift
/// chapter offsets. Returns the count of rows promoted.
pub async fn apply_auto_applicable_candidates(
    library: &LibraryDb,
    book_id: BookId,
    tunables: &AudiologoTunables,
) -> Result<usize> {
    let candidates = load_candidate_rows(library, book_id).await?;
    if candidates.is_empty() {
        return Ok(0);
    }

    let mut tx = library
        .pool()
        .begin()
        .await
        .map_err(|e| Error::Database(format!("audiologo apply tx: {e}")))?;

    let mut applied_count = 0_usize;
    for c in candidates {
        if !c.method.auto_applies() {
            continue;
        }
        let floor = method_floor(c.method, tunables);
        if c.confidence < floor {
            continue;
        }
        let padding_ms = c
            .padding_ms
            .unwrap_or_else(|| default_padding(c.kind, tunables));
        promote_row_to_applied(&mut tx, c.row_id).await?;
        shift_chapters_for_cut(
            &mut tx,
            c.file_id,
            c.jingle_start_ms,
            c.jingle_end_ms,
            padding_ms,
        )
        .await?;
        let cut_ms = cut_ms_from_row(c.jingle_start_ms, c.jingle_end_ms, padding_ms);
        decrement_book_duration_ms(&mut tx, book_id, cut_ms).await?;
        applied_count += 1;
        tracing::info!(
            book = %book_id,
            row_id = c.row_id,
            file_id = c.file_id,
            kind = %c.kind,
            method = %c.method,
            confidence = c.confidence,
            cut_ms,
            "audiologo.apply.promoted"
        );
    }

    if applied_count > 0 {
        // book.audiologo_status = 'applied' takes priority over
        // the 'detected' flip from the detection slice.
        set_book_status(&mut tx, book_id, BookStatus::Applied).await?;
    }

    tx.commit()
        .await
        .map_err(|e| Error::Database(format!("audiologo apply commit: {e}")))?;

    Ok(applied_count)
}

/// Handle the Libation-stripped case.
///
/// When `brand_intro_duration_ms` is non-NULL but no fingerprint
/// hit landed, the audio has been pre-stripped (e.g. by Libation).
/// Sets `books.audiologo_status='stripped'` and shifts chapters
/// by `-brand_intro_duration_ms`. Idempotent: re-running on an
/// already-stripped book is a no-op (we detect this by the
/// existing `audiologo_status='stripped'` value).
pub async fn apply_libation_stripped(
    library: &LibraryDb,
    book_id: BookId,
    brand_intro_duration_ms: u64,
) -> Result<bool> {
    if brand_intro_duration_ms == 0 {
        return Ok(false);
    }
    // Idempotence check.
    let current_status = current_book_status(library, book_id).await?;
    if matches!(current_status, Some(BookStatus::Stripped)) {
        tracing::debug!(
            book = %book_id,
            "audiologo.apply.libation_already_stripped"
        );
        return Ok(false);
    }

    let mut tx = library
        .pool()
        .begin()
        .await
        .map_err(|e| Error::Database(format!("libation apply tx: {e}")))?;

    // Shift chapter offsets on the FIRST file (the one whose
    // head was searched in vain). For multi-file books with
    // chapters keyed on `book_id` alone (no per-file split), the
    // global shift applies — the brand-intro is at the start of
    // the audio sequence regardless of file boundaries.
    shift_chapters_by_offset(&mut tx, book_id, brand_intro_duration_ms).await?;
    decrement_book_duration_ms(&mut tx, book_id, brand_intro_duration_ms).await?;
    set_book_status(&mut tx, book_id, BookStatus::Stripped).await?;

    tx.commit()
        .await
        .map_err(|e| Error::Database(format!("libation apply commit: {e}")))?;

    tracing::info!(
        book = %book_id,
        brand_intro_duration_ms,
        "audiologo.apply.libation_stripped"
    );
    Ok(true)
}

// ── Helpers ───────────────────────────────────────────────────────

const fn method_floor(method: Method, t: &AudiologoTunables) -> f32 {
    match method {
        Method::FingerprintFull => t.fp_full_min_confidence,
        Method::FingerprintBookend => t.fp_bookend_min_confidence,
        Method::FingerprintAndTranscript => t.fp_and_transcript_min_confidence,
        Method::TranscriptOnly => t.transcript_only_min_confidence,
        // Manual rows always pass the floor (the operator
        // explicitly authorised the cut).
        Method::Manual => 0.0,
    }
}

const fn default_padding(kind: Kind, t: &AudiologoTunables) -> u32 {
    match kind {
        Kind::Intro => t.intro_padding_ms,
        Kind::Outro => t.outro_padding_ms,
    }
}

fn cut_ms_from_row(jingle_start_ms: u64, jingle_end_ms: u64, padding_ms: u32) -> u64 {
    let cut = jingle_end_ms.saturating_sub(jingle_start_ms);
    cut.saturating_sub(u64::from(padding_ms))
}

async fn load_candidate_rows(library: &LibraryDb, book_id: BookId) -> Result<Vec<CandidateRow>> {
    let id = book_id.0;
    let candidate_status = Status::Candidate.as_str();
    let rows = sqlx::query!(
        r#"SELECT a.audiologo_row_id AS "row_id!: i64",
                  a.file_id,
                  a.kind,
                  a.jingle_start_ms,
                  a.jingle_end_ms,
                  a.padding_ms,
                  a.method,
                  a.confidence
             FROM book_file_audiologos a
             JOIN book_files f ON f.file_id = a.file_id
            WHERE f.book_id = ? AND a.status = ?
            ORDER BY a.audiologo_row_id"#,
        id,
        candidate_status,
    )
    .fetch_all(library.pool())
    .await
    .map_err(|e| Error::Database(format!("audiologo apply load candidates: {e}")))?;

    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let Some(kind) = Kind::parse(&r.kind) else {
            tracing::warn!(row_id = r.row_id, kind = %r.kind, "audiologo.apply.unknown_kind");
            continue;
        };
        let Some(method) = Method::parse(&r.method) else {
            tracing::warn!(row_id = r.row_id, method = %r.method, "audiologo.apply.unknown_method");
            continue;
        };
        let jingle_start_ms = u64::try_from(r.jingle_start_ms).unwrap_or(0);
        let jingle_end_ms = u64::try_from(r.jingle_end_ms).unwrap_or(0);
        let padding_ms = r.padding_ms.and_then(|p| u32::try_from(p).ok());
        #[allow(clippy::cast_possible_truncation)]
        let confidence = r.confidence as f32;
        out.push(CandidateRow {
            row_id: r.row_id,
            file_id: r.file_id,
            kind,
            jingle_start_ms,
            jingle_end_ms,
            padding_ms,
            method,
            confidence,
        });
    }
    Ok(out)
}

async fn promote_row_to_applied(tx: &mut Transaction<'_, Sqlite>, row_id: i64) -> Result<()> {
    let applied = Status::Applied.as_str();
    sqlx::query!(
        r#"UPDATE book_file_audiologos
              SET status = ?, applied_at = strftime('%s','now')
            WHERE audiologo_row_id = ?"#,
        applied,
        row_id,
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("audiologo apply promote: {e}")))?;
    Ok(())
}

/// Shift `chapters` rows for the cut at
/// `[jingle_start_ms, jingle_end_ms]` with `padding_ms`. Applies
/// the three-case maths from ADR-0024 § Chapter recomputation.
/// Operates on every chapter row attached to the file's parent
/// book (the schema doesn't tie chapters to files), across every
/// `source` value.
async fn shift_chapters_for_cut(
    tx: &mut Transaction<'_, Sqlite>,
    file_id: i64,
    jingle_start_ms: u64,
    jingle_end_ms: u64,
    padding_ms: u32,
) -> Result<()> {
    let cut_ms = cut_ms_from_row(jingle_start_ms, jingle_end_ms, padding_ms);
    if cut_ms == 0 {
        return Ok(());
    }
    let start_i64 = i64::try_from(jingle_start_ms).unwrap_or(i64::MAX);
    let end_i64 = i64::try_from(jingle_end_ms).unwrap_or(i64::MAX);
    let cut_i64 = i64::try_from(cut_ms).unwrap_or(i64::MAX);

    // Case 1: chapter starts at or after the jingle end → shift
    // both start + end down by cut_ms.
    sqlx::query!(
        r#"UPDATE chapters
              SET start_ms = start_ms - ?, end_ms = end_ms - ?
            WHERE book_id = (SELECT book_id FROM book_files WHERE file_id = ?)
              AND start_ms >= ?"#,
        cut_i64,
        cut_i64,
        file_id,
        end_i64,
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("audiologo apply chapter-shift case-1: {e}")))?;

    // Case 2: chapter spans the cut → shift only end_ms down.
    sqlx::query!(
        r#"UPDATE chapters
              SET end_ms = end_ms - ?
            WHERE book_id = (SELECT book_id FROM book_files WHERE file_id = ?)
              AND start_ms < ?
              AND end_ms > ?"#,
        cut_i64,
        file_id,
        start_i64,
        end_i64,
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("audiologo apply chapter-shift case-2: {e}")))?;

    Ok(())
}

/// Shift every chapter for the book by a flat offset (the
/// Libation case: there's no internal "cut" range, just a
/// uniform shift back).
async fn shift_chapters_by_offset(
    tx: &mut Transaction<'_, Sqlite>,
    book_id: BookId,
    offset_ms: u64,
) -> Result<()> {
    if offset_ms == 0 {
        return Ok(());
    }
    let id = book_id.0;
    let offset_i64 = i64::try_from(offset_ms).unwrap_or(i64::MAX);
    sqlx::query!(
        r#"UPDATE chapters
              SET start_ms = MAX(0, start_ms - ?),
                  end_ms = MAX(0, end_ms - ?)
            WHERE book_id = ?"#,
        offset_i64,
        offset_i64,
        id,
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("audiologo apply libation-shift: {e}")))?;
    Ok(())
}

async fn decrement_book_duration_ms(
    tx: &mut Transaction<'_, Sqlite>,
    book_id: BookId,
    cut_ms: u64,
) -> Result<()> {
    if cut_ms == 0 {
        return Ok(());
    }
    let id = book_id.0;
    let cut_i64 = i64::try_from(cut_ms).unwrap_or(i64::MAX);
    sqlx::query!(
        r#"UPDATE books
              SET duration_ms = MAX(0, duration_ms - ?)
            WHERE book_id = ? AND duration_ms IS NOT NULL"#,
        cut_i64,
        id,
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("audiologo apply duration_ms: {e}")))?;
    Ok(())
}

async fn set_book_status(
    tx: &mut Transaction<'_, Sqlite>,
    book_id: BookId,
    status: BookStatus,
) -> Result<()> {
    let id = book_id.0;
    let s = status.as_str();
    sqlx::query!(
        "UPDATE books SET audiologo_status = ? WHERE book_id = ?",
        s,
        id,
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("audiologo apply set status: {e}")))?;
    Ok(())
}

async fn current_book_status(library: &LibraryDb, book_id: BookId) -> Result<Option<BookStatus>> {
    let id = book_id.0;
    let row = sqlx::query!("SELECT audiologo_status FROM books WHERE book_id = ?", id,)
        .fetch_optional(library.pool())
        .await
        .map_err(|e| Error::Database(format!("audiologo apply read status: {e}")))?;
    Ok(row.and_then(|r| BookStatus::parse(&r.audiologo_status)))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use ab_core::tunables::DbTunables;
    use ab_db::LibraryDb;
    use std::path::Path;
    use tempfile::TempDir;

    async fn fresh_library(dir: &Path) -> LibraryDb {
        LibraryDb::open(&dir.join("library.db"), &DbTunables::default())
            .await
            .expect("open library")
    }

    async fn seed_book_with_files(library: &LibraryDb) {
        sqlx::query(
            "INSERT INTO books (book_id, title, duration_ms, raw_duration_ms) \
             VALUES (1, 'fixture', 3_600_000, 3_600_000)",
        )
        .execute(library.pool())
        .await
        .expect("seed book");
        sqlx::query(
            "INSERT INTO book_files (file_id, book_id, file_path, is_active, duration_ms) \
             VALUES (10, 1, '/tmp/a.m4b', 1, 3_600_000)",
        )
        .execute(library.pool())
        .await
        .expect("seed file");
    }

    #[tokio::test]
    async fn cut_ms_subtracts_padding() {
        // jingle is 5000 ms, padding is 250 → cut is 4750.
        assert_eq!(cut_ms_from_row(0, 5_000, 250), 4_750);
        // padding >= jingle → cut floors at 0 (saturating sub).
        assert_eq!(cut_ms_from_row(0, 200, 300), 0);
    }

    #[tokio::test]
    async fn auto_apply_promotes_high_confidence_fingerprint_full() {
        let tmp = TempDir::new().expect("tmpdir");
        let library = fresh_library(tmp.path()).await;
        seed_book_with_files(&library).await;
        // Candidate at high confidence + auto-applying method.
        sqlx::query(
            "INSERT INTO book_file_audiologos \
             (audiologo_row_id, file_id, kind, jingle_start_ms, jingle_end_ms, \
              padding_ms, method, confidence, status) \
             VALUES (100, 10, 'intro', 0, 5000, NULL, 'fingerprint_full', 0.95, 'candidate')",
        )
        .execute(library.pool())
        .await
        .expect("seed candidate");
        // Some chapters: one before the cut (no shift), one at the cut (case-1).
        sqlx::query(
            "INSERT INTO chapters (book_id, idx, start_ms, end_ms, title, source) \
             VALUES \
             (1, 0, 5_000, 60_000, 'Ch1', 'audnexus'), \
             (1, 1, 60_000, 120_000, 'Ch2', 'audnexus')",
        )
        .execute(library.pool())
        .await
        .expect("seed chapters");

        let count =
            apply_auto_applicable_candidates(&library, BookId(1), &AudiologoTunables::default())
                .await
                .expect("apply");
        assert_eq!(count, 1);

        // Row promoted to applied; applied_at populated.
        let (status, applied_at): (String, Option<i64>) = sqlx::query_as(
            "SELECT status, applied_at FROM book_file_audiologos WHERE audiologo_row_id = 100",
        )
        .fetch_one(library.pool())
        .await
        .expect("fetch");
        assert_eq!(status, "applied");
        assert!(applied_at.is_some());

        // cut_ms = (5000 - 0) - 250 = 4750. Chapter at start=5000
        // → 5000 - 4750 = 250 (still >= jingle_end_ms=5000? no).
        // Wait — case-1 fires when start_ms >= jingle_end_ms=5000.
        // start_ms=5000 satisfies the >= comparison → shifts.
        let chapters: Vec<(i64, i64, i64)> = sqlx::query_as(
            "SELECT idx, start_ms, end_ms FROM chapters WHERE book_id = 1 ORDER BY idx",
        )
        .fetch_all(library.pool())
        .await
        .expect("read chapters");
        // Both chapters had start_ms >= 5000 → both shift by 4750.
        assert_eq!(chapters[0], (0, 5_000 - 4_750, 60_000 - 4_750));
        assert_eq!(chapters[1], (1, 60_000 - 4_750, 120_000 - 4_750));

        // books.duration_ms decreased by cut_ms.
        let dur: Option<i64> =
            sqlx::query_scalar("SELECT duration_ms FROM books WHERE book_id = 1")
                .fetch_one(library.pool())
                .await
                .expect("dur");
        assert_eq!(dur, Some(3_600_000 - 4_750));

        // book status set to 'applied'.
        let book_status: String =
            sqlx::query_scalar("SELECT audiologo_status FROM books WHERE book_id = 1")
                .fetch_one(library.pool())
                .await
                .expect("status");
        assert_eq!(book_status, "applied");
    }

    #[tokio::test]
    async fn auto_apply_skips_below_floor() {
        let tmp = TempDir::new().expect("tmpdir");
        let library = fresh_library(tmp.path()).await;
        seed_book_with_files(&library).await;
        // Candidate below the fp_full floor (0.85).
        sqlx::query(
            "INSERT INTO book_file_audiologos \
             (audiologo_row_id, file_id, kind, jingle_start_ms, jingle_end_ms, \
              padding_ms, method, confidence, status) \
             VALUES (100, 10, 'intro', 0, 5000, NULL, 'fingerprint_full', 0.50, 'candidate')",
        )
        .execute(library.pool())
        .await
        .expect("seed");

        let count =
            apply_auto_applicable_candidates(&library, BookId(1), &AudiologoTunables::default())
                .await
                .expect("apply");
        assert_eq!(count, 0);

        // Row still at candidate.
        let status: String = sqlx::query_scalar(
            "SELECT status FROM book_file_audiologos WHERE audiologo_row_id = 100",
        )
        .fetch_one(library.pool())
        .await
        .expect("status");
        assert_eq!(status, "candidate");
    }

    #[tokio::test]
    async fn auto_apply_skips_transcript_only_methods() {
        let tmp = TempDir::new().expect("tmpdir");
        let library = fresh_library(tmp.path()).await;
        seed_book_with_files(&library).await;
        // Transcript-only candidate at very high confidence.
        // Method::TranscriptOnly's auto_applies()=false, so it
        // never auto-promotes regardless of confidence.
        sqlx::query(
            "INSERT INTO book_file_audiologos \
             (audiologo_row_id, file_id, kind, jingle_start_ms, jingle_end_ms, \
              padding_ms, method, confidence, status) \
             VALUES (100, 10, 'intro', 0, 5000, NULL, 'transcript_only', 0.99, 'candidate')",
        )
        .execute(library.pool())
        .await
        .expect("seed");

        let count =
            apply_auto_applicable_candidates(&library, BookId(1), &AudiologoTunables::default())
                .await
                .expect("apply");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn libation_stripped_shifts_chapters_and_flips_status() {
        let tmp = TempDir::new().expect("tmpdir");
        let library = fresh_library(tmp.path()).await;
        seed_book_with_files(&library).await;
        sqlx::query(
            "INSERT INTO chapters (book_id, idx, start_ms, end_ms, title, source) \
             VALUES \
             (1, 0, 5_000, 60_000, 'Ch1', 'audnexus'), \
             (1, 1, 60_000, 120_000, 'Ch2', 'audnexus')",
        )
        .execute(library.pool())
        .await
        .expect("seed chapters");

        let did = apply_libation_stripped(&library, BookId(1), 4_500)
            .await
            .expect("apply");
        assert!(did, "first call returns true");

        let chapters: Vec<(i64, i64)> =
            sqlx::query_as("SELECT start_ms, end_ms FROM chapters WHERE book_id = 1 ORDER BY idx")
                .fetch_all(library.pool())
                .await
                .expect("chapters");
        // Both shifted by -4500.
        assert_eq!(chapters[0], (500, 55_500));
        assert_eq!(chapters[1], (55_500, 115_500));

        let status: String =
            sqlx::query_scalar("SELECT audiologo_status FROM books WHERE book_id = 1")
                .fetch_one(library.pool())
                .await
                .expect("status");
        assert_eq!(status, "stripped");

        // Idempotent second call → returns false.
        let did2 = apply_libation_stripped(&library, BookId(1), 4_500)
            .await
            .expect("apply 2");
        assert!(!did2);

        // Chapters unchanged on second call.
        let chapters2: Vec<(i64, i64)> =
            sqlx::query_as("SELECT start_ms, end_ms FROM chapters WHERE book_id = 1 ORDER BY idx")
                .fetch_all(library.pool())
                .await
                .expect("chapters2");
        assert_eq!(chapters2, chapters, "second call left chapters alone");
    }

    #[tokio::test]
    async fn libation_stripped_clamps_negative_chapter_offsets_to_zero() {
        // brand_intro_duration_ms exceeds the first chapter's
        // start_ms → start_ms - brand > 0 would go negative. The
        // MAX(0, ...) clamp keeps the row sane.
        let tmp = TempDir::new().expect("tmpdir");
        let library = fresh_library(tmp.path()).await;
        seed_book_with_files(&library).await;
        sqlx::query(
            "INSERT INTO chapters (book_id, idx, start_ms, end_ms, title, source) \
             VALUES (1, 0, 1_000, 30_000, 'Ch1', 'audnexus')",
        )
        .execute(library.pool())
        .await
        .expect("seed");

        apply_libation_stripped(&library, BookId(1), 5_000)
            .await
            .expect("apply");

        let (start, end): (i64, i64) =
            sqlx::query_as("SELECT start_ms, end_ms FROM chapters WHERE book_id = 1 AND idx = 0")
                .fetch_one(library.pool())
                .await
                .expect("fetch");
        assert_eq!(start, 0, "clamped");
        assert_eq!(end, 25_000);
    }

    #[tokio::test]
    async fn libation_stripped_noop_when_already_stripped() {
        let tmp = TempDir::new().expect("tmpdir");
        let library = fresh_library(tmp.path()).await;
        seed_book_with_files(&library).await;
        // Pre-set the status.
        sqlx::query("UPDATE books SET audiologo_status = 'stripped' WHERE book_id = 1")
            .execute(library.pool())
            .await
            .expect("preset");

        let did = apply_libation_stripped(&library, BookId(1), 4_500)
            .await
            .expect("apply");
        assert!(!did);
    }
}
