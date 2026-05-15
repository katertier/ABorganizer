//! CUE sidecar chapter import.
//!
//! Reads chapter boundaries from `.cue` files that sit alongside
//! audio files in `book_files`. A CUE sheet is a plaintext index
//! produced by CD rippers (EAC, X Lossless Decoder, cuetools etc.)
//! that lists each track's title + start time. For audiobooks
//! ripped from CDs the per-track entries map cleanly onto chapter
//! boundaries, so a `.cue` sidecar is a high-quality structured
//! source — well above synthesized silence-derived chapters and
//! often above embedded chpl atoms when the audio was re-encoded
//! after the rip.
//!
//! # Source precedence
//!
//! Per `chapter_winner::SOURCE_PRECEDENCE` (already pre-architected
//! to expect this stage):
//!
//! 1. `audnexus` (publisher-curated, includes brand markers)
//! 2. `embedded` (chpl / chapter-track atoms)
//! 3. **`cue`** ← this stage
//! 4. `epub` (companion `EPUB` `ToC`; future)
//! 5. `transcript` / `silence` (synthesized; future)
//!
//! # CUE format
//!
//! The relevant subset of [the CUE format](https://en.wikipedia.org/wiki/Cue_sheet_(computing)):
//!
//! ```text
//! PERFORMER "Author Name"
//! TITLE "Book Title"
//! FILE "audiobook.flac" WAVE
//!   TRACK 01 AUDIO
//!     TITLE "Chapter 1: The Beginning"
//!     INDEX 01 00:00:00
//!   TRACK 02 AUDIO
//!     TITLE "Chapter 2: The Middle"
//!     INDEX 01 12:34:00
//! ```
//!
//! Time codes are `MM:SS:FF` where `FF` is CD frames (1 frame =
//! 1/75 s). The parser converts to milliseconds. Lines outside of
//! `TRACK ... AUDIO` blocks are ignored — we don't need
//! `PERFORMER` / `TITLE` here (tag-read owns book-level metadata).
//!
//! # Multi-file books
//!
//! Each `book_file` may carry its own `<stem>.cue` sibling. The
//! stage parses each sidecar individually and offsets the resulting
//! tracks by the cumulative duration of preceding files, mirroring
//! the merge logic in [`crate::embedded_chapters`]. Books that
//! split across N audio files with N CUE sidecars produce a single
//! continuous chapter list.

use std::path::Path;

use async_trait::async_trait;

use ab_core::{BookId, Error, Result};
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};

/// Provenance source tag for chapters this stage writes.
pub const CHAPTER_SOURCE: &str = "cue";

/// Typed identifier for this stage.
pub const STAGE_ID: StageId = StageId::new("read-cue-sidecar");

/// Stage that imports chapters from `.cue` sidecars next to
/// `book_files` audio files.
pub struct CueSidecarChaptersStage;

impl CueSidecarChaptersStage {
    /// Construct. No tunables — CUE reading is always-on; missing
    /// sidecars are not an error (most books don't have them).
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for CueSidecarChaptersStage {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Stage for CueSidecarChaptersStage {
    fn name(&self) -> &'static str {
        STAGE_ID.as_str()
    }

    fn requires(&self) -> &'static [StageId] {
        // read-tags populates `book_files.duration_ms`, which we
        // need to offset multi-file books' chapter positions and
        // to compute end_ms for the final track in each file.
        &[ab_tag_read::STAGE_ID]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        let files = fetch_book_files(&ctx.library, book_id).await?;
        if files.is_empty() {
            return Ok(StageOutcome::Skipped);
        }

        let mut per_file: Vec<Vec<CueTrack>> = Vec::with_capacity(files.len());
        for f in &files {
            let path = std::path::PathBuf::from(&f.file_path);
            let tracks = tokio::task::spawn_blocking(move || read_cue_for_file(&path))
                .await
                .map_err(|e| Error::stage("read-cue-sidecar", format!("join: {e}")))?;
            per_file.push(tracks);
        }

        let has_real_tracks = per_file.iter().any(|t| !t.is_empty());
        if !has_real_tracks {
            tracing::debug!(
                book = %book_id,
                files = files.len(),
                "cue.chapters.no_sidecars"
            );
            return Ok(StageOutcome::Skipped);
        }

        let merged = merge_per_file_tracks(&files, &per_file);
        write_cue_chapters(&ctx.library, book_id, &merged).await?;
        tracing::info!(
            book = %book_id,
            files = files.len(),
            chapter_count = merged.len(),
            "cue.chapters.done"
        );
        Ok(StageOutcome::Done)
    }
}

/// One row from `book_files` carrying just the fields this stage
/// needs. Mirrors the shape in [`crate::embedded_chapters`] so
/// the merge logic stays familiar across both stages.
struct FileEntry {
    file_path: String,
    duration_ms: i64,
}

async fn fetch_book_files(library: &ab_db::LibraryDb, book_id: BookId) -> Result<Vec<FileEntry>> {
    let id = book_id.0;
    let rows = sqlx::query!(
        r#"SELECT file_path, COALESCE(duration_ms, 0) AS "duration_ms!"
           FROM book_files
           WHERE book_id = ? AND is_active = 1
           ORDER BY file_id"#,
        id,
    )
    .fetch_all(library.pool())
    .await
    .map_err(|e| Error::Database(format!("cue fetch book_files: {e}")))?;
    Ok(rows
        .into_iter()
        .map(|r| FileEntry {
            file_path: r.file_path,
            duration_ms: r.duration_ms,
        })
        .collect())
}

/// One track read out of a CUE sidecar. `start_ms` is file-local.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CueTrack {
    start_ms: u64,
    title: String,
}

/// Locate the sidecar for `audio_file` and parse it. The sidecar
/// is the same path with the extension replaced by `.cue`.
///
/// Returns an empty vector when no sidecar exists or when the
/// sidecar parses to zero tracks — both are non-errors (most
/// books don't ship a CUE).
fn read_cue_for_file(audio_file: &Path) -> Vec<CueTrack> {
    let cue_path = audio_file.with_extension("cue");
    if !cue_path.exists() {
        return Vec::new();
    }
    let raw = match std::fs::read_to_string(&cue_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(
                file = %cue_path.display(),
                error = %e,
                "cue.sidecar.read_failed"
            );
            return Vec::new();
        }
    };
    parse_cue(&raw)
}

/// Parse a CUE sheet body into the tracks it lists.
///
/// Robust to:
/// - Leading whitespace (`  TRACK 01 AUDIO`, `    TITLE "..."`).
/// - Mixed CRLF / LF line endings.
/// - Missing TITLE on a track (synthesized as `Track N`).
/// - Missing INDEX 01 on a track (track is dropped — without a
///   start time it can't become a chapter).
///
/// Out of scope:
/// - Multiple FILE statements within one CUE (audiobook CUEs
///   either reference one file or split per-file with one CUE
///   each; we already handle the latter via per-file calls).
///   When a single CUE references multiple FILEs the tracks
///   under the second + subsequent FILE statements get their
///   timestamps treated as file-local to whichever file the
///   audio sibling matches — which is the wrong answer if the
///   operator does this. Flagged for future iteration; in
///   practice the per-file CUE shape covers what's in the
///   wild.
fn parse_cue(body: &str) -> Vec<CueTrack> {
    let mut tracks: Vec<CueTrack> = Vec::new();
    let mut current_title: Option<String> = None;
    let mut current_start_ms: Option<u64> = None;
    let mut current_index: u32 = 0;

    let flush =
        |idx: u32, title: Option<String>, start_ms: Option<u64>, out: &mut Vec<CueTrack>| {
            if let Some(start) = start_ms {
                out.push(CueTrack {
                    start_ms: start,
                    title: title.unwrap_or_else(|| format!("Track {idx}")),
                });
            }
        };

    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("TRACK ") {
            // Starting a new track — flush the previous one.
            if current_index != 0 {
                flush(
                    current_index,
                    current_title.take(),
                    current_start_ms.take(),
                    &mut tracks,
                );
            }
            // "TRACK NN AUDIO" → take the NN.
            current_index = rest
                .split_whitespace()
                .next()
                .and_then(|n| n.parse::<u32>().ok())
                .unwrap_or(current_index + 1);
        } else if let Some(rest) = trimmed.strip_prefix("TITLE ") {
            if current_index == 0 {
                // Header-level TITLE — book title. Owned by
                // tag-read, ignored here.
                continue;
            }
            current_title = Some(unquote(rest.trim()));
        } else if let Some(rest) = trimmed.strip_prefix("INDEX ") {
            // INDEX 01 mm:ss:ff is the start; INDEX 00 mm:ss:ff
            // is the "pregap" — we use INDEX 01 as the chapter
            // start (the audible content), per CD-DA convention.
            let mut parts = rest.split_whitespace();
            let index_id = parts
                .next()
                .and_then(|n| n.parse::<u32>().ok())
                .unwrap_or(0);
            if index_id != 1 {
                continue;
            }
            if let Some(ts) = parts.next()
                && let Some(ms) = parse_cue_time(ts)
            {
                current_start_ms = Some(ms);
            }
        }
    }
    // Flush the last track.
    if current_index != 0 {
        flush(current_index, current_title, current_start_ms, &mut tracks);
    }
    tracks
}

/// Strip surrounding quotes from a CUE string field. CUE strings
/// can be `"quoted"` or bare; both are valid.
fn unquote(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        trimmed[1..trimmed.len() - 1].to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Parse a CUE timestamp `MM:SS:FF` into milliseconds.
///
/// `MM` is minutes (0–99 typical), `SS` is seconds (0–59), `FF`
/// is CD frames (0–74, 75 frames per second).
fn parse_cue_time(ts: &str) -> Option<u64> {
    let parts: Vec<&str> = ts.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let minutes: u64 = parts[0].parse().ok()?;
    let seconds: u64 = parts[1].parse().ok()?;
    let frames: u64 = parts[2].parse().ok()?;
    if seconds >= 60 || frames >= 75 {
        return None;
    }
    // 1 frame = 1000/75 ms ≈ 13.333 ms; multiply first to stay in
    // integer math and round to nearest.
    let frame_ms = (frames * 1000 + 37) / 75;
    Some(minutes * 60_000 + seconds * 1000 + frame_ms)
}

/// Book-level merged chapter after offsetting per-file tracks.
struct MergedChapter {
    start_ms: i64,
    end_ms: i64,
    title: String,
}

/// Merge per-file CUE tracks into a book-level chapter list with
/// cumulative-duration offsets. Mirrors [`crate::embedded_chapters`]'s
/// merge logic; the comments there cover the corner cases.
fn merge_per_file_tracks(files: &[FileEntry], per_file: &[Vec<CueTrack>]) -> Vec<MergedChapter> {
    debug_assert_eq!(files.len(), per_file.len());
    let mut out: Vec<MergedChapter> = Vec::new();
    let mut offset_ms: i64 = 0;

    for (idx, (file, tracks)) in files.iter().zip(per_file.iter()).enumerate() {
        if tracks.is_empty() {
            // No CUE for this file — synthesize a single "Part N"
            // entry so book-level coverage stays continuous (matches
            // embedded_chapters' behaviour exactly).
            if file.duration_ms > 0 {
                out.push(MergedChapter {
                    start_ms: offset_ms,
                    end_ms: offset_ms.saturating_add(file.duration_ms),
                    title: format!("Part {}", idx + 1),
                });
            }
        } else {
            for (i, track) in tracks.iter().enumerate() {
                let start_i64 = i64::try_from(track.start_ms).unwrap_or(i64::MAX);
                let end_local = tracks.get(i + 1).map_or_else(
                    || start_i64.max(file.duration_ms),
                    |next| i64::try_from(next.start_ms).unwrap_or(i64::MAX),
                );
                out.push(MergedChapter {
                    start_ms: offset_ms.saturating_add(start_i64),
                    end_ms: offset_ms.saturating_add(end_local),
                    title: if track.title.trim().is_empty() {
                        format!("Chapter {}", out.len() + 1)
                    } else {
                        track.title.clone()
                    },
                });
            }
        }
        offset_ms = offset_ms.saturating_add(file.duration_ms);
    }
    out
}

/// Clear existing `source = 'cue'` rows for this book and insert
/// the merged set. Mirrors [`crate::embedded_chapters`]'s
/// idempotency: the chapter winner stage re-runs per book and
/// every source stage is allowed to be re-entrant.
async fn write_cue_chapters(
    library: &ab_db::LibraryDb,
    book_id: BookId,
    chapters: &[MergedChapter],
) -> Result<()> {
    let id = book_id.0;
    let mut tx = library
        .pool()
        .begin()
        .await
        .map_err(|e| Error::Database(format!("cue tx begin: {e}")))?;

    sqlx::query!(
        "DELETE FROM chapters WHERE book_id = ? AND source = ?",
        id,
        CHAPTER_SOURCE,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| Error::Database(format!("cue clear: {e}")))?;

    for (idx, ch) in chapters.iter().enumerate() {
        let idx_i64 = i64::try_from(idx).unwrap_or(i64::MAX);
        sqlx::query!(
            "INSERT INTO chapters (book_id, idx, start_ms, end_ms, title, source) \
             VALUES (?, ?, ?, ?, ?, ?)",
            id,
            idx_i64,
            ch.start_ms,
            ch.end_ms,
            ch.title,
            CHAPTER_SOURCE,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| Error::Database(format!("cue insert idx={idx}: {e}")))?;
    }

    tx.commit()
        .await
        .map_err(|e| Error::Database(format!("cue tx commit: {e}")))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_cue_time_basic() {
        assert_eq!(parse_cue_time("00:00:00"), Some(0));
        assert_eq!(parse_cue_time("00:01:00"), Some(1000));
        assert_eq!(parse_cue_time("01:00:00"), Some(60_000));
        // 75 frames = 1 second.
        assert_eq!(parse_cue_time("00:00:75"), None); // out of range
        // 37 frames ≈ 493 ms (rounded from 493.33).
        assert_eq!(parse_cue_time("00:00:37"), Some(493));
    }

    #[test]
    fn parse_cue_time_rejects_malformed() {
        assert_eq!(parse_cue_time(""), None);
        assert_eq!(parse_cue_time("00:00"), None);
        assert_eq!(parse_cue_time("xx:yy:zz"), None);
        assert_eq!(parse_cue_time("00:60:00"), None);
    }

    #[test]
    fn parse_minimal_cue() {
        let body = r#"PERFORMER "Author"
TITLE "Book"
FILE "audio.flac" WAVE
  TRACK 01 AUDIO
    TITLE "Chapter 1"
    INDEX 01 00:00:00
  TRACK 02 AUDIO
    TITLE "Chapter 2"
    INDEX 01 12:34:00
  TRACK 03 AUDIO
    TITLE "Chapter 3"
    INDEX 01 25:00:00
"#;
        let tracks = parse_cue(body);
        assert_eq!(tracks.len(), 3);
        assert_eq!(tracks[0].title, "Chapter 1");
        assert_eq!(tracks[0].start_ms, 0);
        assert_eq!(tracks[1].title, "Chapter 2");
        assert_eq!(tracks[1].start_ms, 754_000); // 12*60+34
        assert_eq!(tracks[2].start_ms, 1_500_000); // 25*60
    }

    #[test]
    fn track_without_title_synthesizes_name() {
        let body = "FILE \"a.flac\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n";
        let tracks = parse_cue(body);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].title, "Track 1");
    }

    #[test]
    fn track_without_index_01_is_dropped() {
        // INDEX 00 is pregap, not a chapter start.
        let body = "TRACK 01 AUDIO\n  TITLE \"X\"\n  INDEX 00 00:00:00\n";
        let tracks = parse_cue(body);
        assert!(tracks.is_empty());
    }

    #[test]
    fn merge_single_file_cue() {
        let files = vec![FileEntry {
            file_path: "/x/a.flac".into(),
            duration_ms: 1_800_000, // 30 min
        }];
        let per_file = vec![vec![
            CueTrack {
                start_ms: 0,
                title: "C1".into(),
            },
            CueTrack {
                start_ms: 600_000,
                title: "C2".into(),
            },
        ]];
        let out = merge_per_file_tracks(&files, &per_file);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].start_ms, 0);
        assert_eq!(out[0].end_ms, 600_000);
        assert_eq!(out[1].start_ms, 600_000);
        assert_eq!(out[1].end_ms, 1_800_000);
    }

    #[test]
    fn merge_two_files_offsets_second_file() {
        let files = vec![
            FileEntry {
                file_path: "/x/a.flac".into(),
                duration_ms: 1_000_000,
            },
            FileEntry {
                file_path: "/x/b.flac".into(),
                duration_ms: 2_000_000,
            },
        ];
        let per_file = vec![
            vec![CueTrack {
                start_ms: 0,
                title: "A".into(),
            }],
            vec![CueTrack {
                start_ms: 0,
                title: "B".into(),
            }],
        ];
        let out = merge_per_file_tracks(&files, &per_file);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].start_ms, 0);
        assert_eq!(out[0].end_ms, 1_000_000);
        assert_eq!(out[1].start_ms, 1_000_000);
        assert_eq!(out[1].end_ms, 3_000_000);
    }

    #[test]
    fn merge_file_without_cue_synthesizes_part() {
        let files = vec![
            FileEntry {
                file_path: "/x/a.flac".into(),
                duration_ms: 500_000,
            },
            FileEntry {
                file_path: "/x/b.flac".into(),
                duration_ms: 700_000,
            },
        ];
        let per_file = vec![
            vec![CueTrack {
                start_ms: 0,
                title: "C1".into(),
            }],
            vec![], // no .cue for b.flac
        ];
        let out = merge_per_file_tracks(&files, &per_file);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].title, "C1");
        assert_eq!(out[1].title, "Part 2");
        assert_eq!(out[1].start_ms, 500_000);
        assert_eq!(out[1].end_ms, 1_200_000);
    }

    #[test]
    fn read_cue_for_file_missing_sidecar_returns_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let audio = dir.path().join("nope.flac");
        std::fs::write(&audio, b"").expect("write");
        let tracks = read_cue_for_file(&audio);
        assert!(tracks.is_empty());
    }

    #[test]
    fn read_cue_for_file_finds_sidecar() {
        let dir = tempfile::tempdir().expect("tempdir");
        let audio = dir.path().join("book.flac");
        let cue = dir.path().join("book.cue");
        std::fs::write(&audio, b"").expect("write audio");
        std::fs::write(
            &cue,
            "FILE \"book.flac\" WAVE\n  TRACK 01 AUDIO\n    TITLE \"Hi\"\n    INDEX 01 00:00:00\n",
        )
        .expect("write cue");
        let tracks = read_cue_for_file(&audio);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].title, "Hi");
    }
}
