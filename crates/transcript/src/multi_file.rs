//! Multi-file book helpers for the transcribe stages.
//!
//! A book is a sequence of one or more audio files. The
//! `book_files` table holds each file's path + duration; an
//! active file is one with `is_active = 1`. Multi-file books
//! (audiobook CDs, chaptered exports) get one row per file in
//! `file_id` order; single-file books just have one row.
//!
//! ## Time-base convention
//!
//! Segment timestamps emitted by the Swift bridge are
//! file-relative — `start_ms = 0` means "the start of the file
//! we transcribed". Downstream consumers (chapter alignment,
//! UI, full-book search) want a single coherent timeline. The
//! [`rebase_segments`] helper shifts a segment vec's timestamps
//! by a cumulative offset; transcribe loops call it after each
//! per-file transcribe to land everything in book time-base.
//!
//! ## Why iterate per file (Rust outer loop) instead of Swift
//! streaming across files
//!
//! - The Swift bridge takes one URL per call. Extending to a
//!   multi-URL signature widens the FFI surface for marginal
//!   benefit.
//! - Per-file analyzer sessions cost ~300 ms of setup each. For
//!   a 20-CD audiobook that's ~6 s overhead — negligible at
//!   Idle priority.
//! - Chunk-boundary artifacts (the concern that drove the
//!   `AVAssetReader` rewrite in 3D.3) don't apply here: file
//!   boundaries fall at chapter breaks, which are natural
//!   reset points by design. The transcriber resetting context
//!   at a chapter break is fine.

use std::path::PathBuf;

use ab_core::{BookId, Error, Result};
use ab_db::LibraryDb;

use ab_speech::TranscriptSegment;

/// One active audio file belonging to a book.
#[derive(Debug, Clone)]
pub struct FileEntry {
    /// Filesystem path the Swift bridge opens.
    pub path: PathBuf,
    /// File's own duration in seconds.
    pub duration_secs: f64,
    /// Cumulative duration of all preceding active files in
    /// the book. The first file's `cumulative_offset_secs` is
    /// 0.0; the second's is `files[0].duration_secs`; etc.
    /// Used to rebase per-file segment timestamps into the
    /// book's global time-base.
    pub cumulative_offset_secs: f64,
}

/// Load every active file for a book in `file_id` order, with
/// each file's cumulative offset pre-computed.
///
/// Returns an empty vec when the book has no rows or no active
/// files. Callers treat that as `Skipped`.
///
/// # Errors
///
/// Propagates database errors.
pub async fn active_files(library: &LibraryDb, book_id: BookId) -> Result<Vec<FileEntry>> {
    let id = book_id.0;
    let rows = sqlx::query!(
        "SELECT file_path, duration_ms FROM book_files \
         WHERE book_id = ? AND is_active = 1 \
         ORDER BY `file_id`",
        id,
    )
    .fetch_all(library.pool())
    .await
    .map_err(|e| Error::Database(format!("multi_file load active files: {e}")))?;

    let mut out = Vec::with_capacity(rows.len());
    let mut cumulative: f64 = 0.0;
    for row in rows {
        // i64 → f64 is lossy past 2^53 milliseconds (~285,000
        // years). A single audiobook file isn't going to come
        // anywhere near that.
        #[allow(clippy::cast_precision_loss)]
        let duration_secs = row.duration_ms.unwrap_or(0).max(0) as f64 / 1000.0;
        out.push(FileEntry {
            path: PathBuf::from(row.file_path),
            duration_secs,
            cumulative_offset_secs: cumulative,
        });
        cumulative += duration_secs;
    }
    Ok(out)
}

/// Sum the duration of every active file in `files`. Equals
/// `files.last().cumulative_offset_secs +
/// files.last().duration_secs` (or 0 when empty).
#[must_use]
pub fn total_duration_secs(files: &[FileEntry]) -> f64 {
    files
        .last()
        .map_or(0.0, |f| f.cumulative_offset_secs + f.duration_secs)
}

/// Map a book-time position (seconds since book start) to
/// `(file_index, in_file_offset_secs)`. Returns `None` when
/// the position falls outside the book's total duration.
///
/// # Examples
///
/// ```ignore
/// // 3-file book, files of 600s each.
/// // total = 1800s; 25% = 450s
/// // → file 0, offset 450s
/// let (idx, off) = map_position(&files, 450.0).unwrap();
/// assert_eq!(idx, 0);
/// // 50% = 900s → file 1, offset 300s
/// let (idx, off) = map_position(&files, 900.0).unwrap();
/// assert_eq!(idx, 1);
/// ```
#[must_use]
pub fn map_position(files: &[FileEntry], target_secs: f64) -> Option<(usize, f64)> {
    if target_secs < 0.0 {
        return None;
    }
    let total = total_duration_secs(files);
    if target_secs >= total {
        return None;
    }
    for (i, f) in files.iter().enumerate() {
        let file_end = f.cumulative_offset_secs + f.duration_secs;
        if target_secs < file_end {
            return Some((i, target_secs - f.cumulative_offset_secs));
        }
    }
    None
}

/// Shift every segment's `start_ms` / `end_ms` by
/// `offset_secs * 1000`, in place. Used after a per-file
/// transcribe to rebase file-relative timestamps into book
/// time-base.
///
/// `offset_secs` must be non-negative; negative values are
/// silently clamped to 0 (defensive — the bridge never emits
/// negative offsets but a future caller could pass garbage).
pub fn rebase_segments(segments: &mut [TranscriptSegment], offset_secs: f64) {
    if offset_secs <= 0.0 {
        return;
    }
    // f64 → u64 floor cast: offset_secs is non-negative and
    // bounded by the book's total duration. Fits in u64 for
    // any plausible audiobook (u64::MAX / 1000 seconds ≈ 600M
    // years).
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let offset_ms = (offset_secs * 1000.0) as u64;
    for seg in segments.iter_mut() {
        seg.start_ms = seg.start_ms.saturating_add(offset_ms);
        seg.end_ms = seg.end_ms.saturating_add(offset_ms);
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn fixture(durations: &[f64]) -> Vec<FileEntry> {
        let mut cum = 0.0;
        durations
            .iter()
            .enumerate()
            .map(|(i, &d)| {
                let entry = FileEntry {
                    path: PathBuf::from(format!("/tmp/file{i}.m4b")),
                    duration_secs: d,
                    cumulative_offset_secs: cum,
                };
                cum += d;
                entry
            })
            .collect()
    }

    #[test]
    fn total_duration_sums() {
        let files = fixture(&[100.0, 200.0, 50.0]);
        assert!((total_duration_secs(&files) - 350.0).abs() < 0.001);
    }

    #[test]
    fn total_duration_empty() {
        assert!((total_duration_secs(&[]) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn map_position_first_file() {
        let files = fixture(&[100.0, 200.0, 50.0]);
        let (idx, off) = map_position(&files, 50.0).expect("in range");
        assert_eq!(idx, 0);
        assert!((off - 50.0).abs() < 0.001);
    }

    #[test]
    fn map_position_middle_file() {
        let files = fixture(&[100.0, 200.0, 50.0]);
        // 175.0 → file 1, offset 75 (file 1 spans 100..300)
        let (idx, off) = map_position(&files, 175.0).expect("in range");
        assert_eq!(idx, 1);
        assert!((off - 75.0).abs() < 0.001);
    }

    #[test]
    fn map_position_last_file() {
        let files = fixture(&[100.0, 200.0, 50.0]);
        // 325 → file 2, offset 25 (file 2 spans 300..350)
        let (idx, off) = map_position(&files, 325.0).expect("in range");
        assert_eq!(idx, 2);
        assert!((off - 25.0).abs() < 0.001);
    }

    #[test]
    fn map_position_out_of_range() {
        let files = fixture(&[100.0, 200.0, 50.0]);
        assert!(map_position(&files, -1.0).is_none());
        assert!(map_position(&files, 350.0).is_none());
        assert!(map_position(&files, 1000.0).is_none());
    }

    #[test]
    fn map_position_empty_files() {
        assert!(map_position(&[], 0.0).is_none());
        assert!(map_position(&[], 100.0).is_none());
    }

    #[test]
    fn rebase_shifts_segments() {
        let mut segs = vec![
            TranscriptSegment {
                start_ms: 0,
                end_ms: 1000,
                text: "a".into(),
                confidence: 0.9,
            },
            TranscriptSegment {
                start_ms: 1000,
                end_ms: 2000,
                text: "b".into(),
                confidence: 0.9,
            },
        ];
        rebase_segments(&mut segs, 100.0);
        assert_eq!(segs[0].start_ms, 100_000);
        assert_eq!(segs[0].end_ms, 101_000);
        assert_eq!(segs[1].start_ms, 101_000);
        assert_eq!(segs[1].end_ms, 102_000);
    }

    #[test]
    fn rebase_no_op_on_zero_offset() {
        let mut segs = vec![TranscriptSegment {
            start_ms: 500,
            end_ms: 1500,
            text: "a".into(),
            confidence: 0.9,
        }];
        rebase_segments(&mut segs, 0.0);
        assert_eq!(segs[0].start_ms, 500);
        assert_eq!(segs[0].end_ms, 1500);
    }
}
