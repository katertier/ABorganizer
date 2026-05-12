//! Embedded MP4 chapter import.
//!
//! Reads chapter atoms from the book's `.m4b` / `.m4a` files via
//! `mp4ameta` and persists them to the `chapters` table at
//! `source = 'embedded'`. Covers the case Audnexus can't (books
//! with no ASIN, indie releases not in the Audible catalog,
//! offline-only audiobooks).
//!
//! # Two MP4 chapter atoms
//!
//! - **`chpl` (chapter list)**: a flat list of (start, title)
//!   entries. Audible-encoded M4Bs use this; `QuickTime`'s "chapter
//!   list" feature writes it.
//! - **Chapter track**: a text track parallel to the audio track
//!   with chapter labels at time codes. Older M4Bs + some
//!   open-source encoders use this.
//!
//! Books typically carry one of the two, not both. This stage
//! reads `chpl` first; if it's empty, falls back to the chapter
//! track.
//!
//! # Multi-file books
//!
//! Each `book_file` carries its own chapter atoms. Per-file
//! chapters get offset by the running sum of preceding files'
//! `duration_ms` so the book-level `chapters.start_ms` is
//! continuous. Files without chapters synthesize a single
//! "Part N" entry covering their full range (matches the
//! merge-chapters behaviour `ABtagger` established).
//!
//! # What this stage does NOT handle
//!
//! MP3 `CHAP` frames (`ID3v2` chapter atoms). `Lofty` supports those
//! but `mp4ameta` doesn't, so they'd need a separate path through
//! the same stage. Follow-up slice.

use async_trait::async_trait;

use ab_core::{BookId, Error, Result};
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};

/// Provenance source tag for chapters this stage writes.
pub const CHAPTER_SOURCE: &str = "embedded";

/// Stage that imports chapter atoms from MP4 audiobook files.
pub struct EmbeddedChaptersStage;

impl EmbeddedChaptersStage {
    /// Construct. No tunables — embedded chapter reading is
    /// always-on; failures per-file are logged and skipped.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for EmbeddedChaptersStage {
    fn default() -> Self {
        Self::new()
    }
}

/// Typed identifier for this stage.
pub const STAGE_ID: StageId = StageId::new("embedded-chapters");

#[async_trait]
impl Stage for EmbeddedChaptersStage {
    fn name(&self) -> &'static str {
        STAGE_ID.as_str()
    }

    fn requires(&self) -> &'static [StageId] {
        // tag-read populates `book_files.duration_ms`, which we
        // need to offset multi-file books' chapter positions and
        // to synthesize "Part N" entries for files with no
        // embedded chapters.
        &[ab_tag_read::STAGE_ID]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        let files = fetch_book_files(&ctx.library, book_id).await?;
        if files.is_empty() {
            return Ok(StageOutcome::Skipped);
        }

        // Run mp4ameta on each file off-runtime — it's sync I/O +
        // CPU for the atom parse, classic spawn_blocking territory.
        let mut per_file: Vec<Vec<(u64, String)>> = Vec::with_capacity(files.len());
        for f in &files {
            let path = std::path::PathBuf::from(&f.file_path);
            let chapters = tokio::task::spawn_blocking(move || read_chapters_from_file(&path))
                .await
                .map_err(|e| Error::stage("embedded-chapters", format!("join: {e}")))?;
            per_file.push(chapters);
        }

        // Don't write a single synthesized "whole book is one
        // chapter" entry — that's not useful information. Only
        // persist when at least one file contributed real chapter
        // atoms.
        let has_real_chapters = per_file.iter().any(|c| !c.is_empty());
        if !has_real_chapters {
            tracing::debug!(
                book = %book_id,
                files = files.len(),
                "embedded.chapters.no_real_atoms"
            );
            return Ok(StageOutcome::Skipped);
        }

        let merged = merge_per_file_chapters(&files, &per_file);
        write_embedded_chapters(&ctx.library, book_id, &merged).await?;
        tracing::info!(
            book = %book_id,
            files = files.len(),
            chapter_count = merged.len(),
            "embedded.chapters.done"
        );
        Ok(StageOutcome::Done)
    }
}

/// One row from `book_files` carrying just the fields this stage
/// needs.
struct FileEntry {
    file_path: String,
    /// Duration in milliseconds, or 0 if tag-read didn't read it
    /// (in which case multi-file offset becomes incorrect — we
    /// can't help that without re-probing). Files with `0` get
    /// no synthesized "Part" entry.
    duration_ms: i64,
}

/// Fetch active `book_files` in `file_id` order so multi-file
/// books get chapters in the correct physical sequence.
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
    .map_err(|e| Error::Database(format!("embedded fetch book_files: {e}")))?;
    Ok(rows
        .into_iter()
        .map(|r| FileEntry {
            file_path: r.file_path,
            duration_ms: r.duration_ms,
        })
        .collect())
}

/// Read `chpl` (chapter list) chapters from one MP4 file, with
/// fallback to the chapter-track. Returns `(start_ms, title)`
/// tuples in file-local time. Non-MP4 files and read errors
/// return an empty vector — they're not fatal.
fn read_chapters_from_file(path: &std::path::Path) -> Vec<(u64, String)> {
    let tag = match mp4ameta::Tag::read_from_path(path) {
        Ok(t) => t,
        Err(e) => {
            tracing::debug!(
                file = %path.display(),
                error = %e,
                "embedded.chapters.read_failed"
            );
            return Vec::new();
        }
    };
    let list = tag.chapter_list();
    if !list.is_empty() {
        return list
            .iter()
            .map(|c| (duration_to_millis_u64(c.start), c.title.clone()))
            .collect();
    }
    let track = tag.chapter_track();
    track
        .iter()
        .map(|c| (duration_to_millis_u64(c.start), c.title.clone()))
        .collect()
}

/// `Duration::as_millis()` returns u128; clamp to u64 because no
/// audiobook reaches the u64 millisecond limit (~584 million
/// years) — and we wouldn't have memory to hold the tag anyway.
fn duration_to_millis_u64(d: std::time::Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Merge per-file chapter sets into a single book-level sequence.
/// Applies the cumulative-duration offset; synthesizes "Part N"
/// for files that had no chapters of their own (so the player
/// has continuous coverage when SOME file in the book did).
fn merge_per_file_chapters(
    files: &[FileEntry],
    per_file: &[Vec<(u64, String)>],
) -> Vec<MergedChapter> {
    debug_assert_eq!(files.len(), per_file.len());
    let mut out: Vec<MergedChapter> = Vec::new();
    let mut offset_ms: i64 = 0;

    for (idx, (file, chapters)) in files.iter().zip(per_file.iter()).enumerate() {
        if chapters.is_empty() {
            // Synthesize a "Part N" covering this file's range.
            // Only when we know the duration — otherwise the
            // entry would have end == start which is useless.
            if file.duration_ms > 0 {
                out.push(MergedChapter {
                    start_ms: offset_ms,
                    end_ms: offset_ms.saturating_add(file.duration_ms),
                    title: format!("Part {}", idx + 1),
                });
            }
        } else {
            // Real chapters: end_ms is the next chapter's start,
            // or the file's end for the last chapter in the file.
            for (i, (start_ms, title)) in chapters.iter().enumerate() {
                let start_i64 = i64::try_from(*start_ms).unwrap_or(i64::MAX);
                let end_local = chapters.get(i + 1).map_or_else(
                    || start_i64.max(file.duration_ms),
                    |next| i64::try_from(next.0).unwrap_or(i64::MAX),
                );
                out.push(MergedChapter {
                    start_ms: offset_ms.saturating_add(start_i64),
                    end_ms: offset_ms.saturating_add(end_local),
                    title: if title.trim().is_empty() {
                        format!("Chapter {}", out.len() + 1)
                    } else {
                        title.clone()
                    },
                });
            }
        }
        offset_ms = offset_ms.saturating_add(file.duration_ms);
    }
    out
}

/// One merged chapter — book-level (post-offset).
struct MergedChapter {
    start_ms: i64,
    end_ms: i64,
    title: String,
}

/// Clear existing `source = 'embedded'` rows for this book and
/// insert the merged set. Mirrors audnexus-chapters' idempotency.
async fn write_embedded_chapters(
    library: &ab_db::LibraryDb,
    book_id: BookId,
    chapters: &[MergedChapter],
) -> Result<()> {
    let id = book_id.0;
    let mut tx = library
        .pool()
        .begin()
        .await
        .map_err(|e| Error::Database(format!("embedded tx begin: {e}")))?;

    sqlx::query!(
        "DELETE FROM chapters WHERE book_id = ? AND source = ?",
        id,
        CHAPTER_SOURCE,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| Error::Database(format!("embedded clear: {e}")))?;

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
        .map_err(|e| Error::Database(format!("embedded insert idx={idx}: {e}")))?;
    }

    tx.commit()
        .await
        .map_err(|e| Error::Database(format!("embedded tx commit: {e}")))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn merge_empty_files_returns_empty() {
        let files: Vec<FileEntry> = Vec::new();
        let per_file: Vec<Vec<(u64, String)>> = Vec::new();
        let out = merge_per_file_chapters(&files, &per_file);
        assert!(out.is_empty());
    }

    #[test]
    fn merge_single_file_with_chapters_uses_file_local_offsets() {
        let files = vec![FileEntry {
            file_path: "a.m4b".into(),
            duration_ms: 100_000,
        }];
        let per_file = vec![vec![
            (0, "Intro".into()),
            (10_000, "Chapter 1".into()),
            (60_000, "Chapter 2".into()),
        ]];
        let out = merge_per_file_chapters(&files, &per_file);
        assert_eq!(out.len(), 3);
        assert_eq!((out[0].start_ms, out[0].end_ms), (0, 10_000));
        assert_eq!(out[0].title, "Intro");
        assert_eq!((out[1].start_ms, out[1].end_ms), (10_000, 60_000));
        // Last chapter ends at file duration.
        assert_eq!((out[2].start_ms, out[2].end_ms), (60_000, 100_000));
    }

    #[test]
    fn merge_multi_file_offsets_by_cumulative_duration() {
        let files = vec![
            FileEntry {
                file_path: "01.m4b".into(),
                duration_ms: 60_000,
            },
            FileEntry {
                file_path: "02.m4b".into(),
                duration_ms: 90_000,
            },
        ];
        let per_file = vec![
            vec![
                (0, "Part 1 Chapter 1".into()),
                (30_000, "Part 1 Chapter 2".into()),
            ],
            vec![(0, "Part 2 Chapter 1".into())],
        ];
        let out = merge_per_file_chapters(&files, &per_file);
        assert_eq!(out.len(), 3);
        assert_eq!((out[0].start_ms, out[0].end_ms), (0, 30_000));
        assert_eq!((out[1].start_ms, out[1].end_ms), (30_000, 60_000));
        // Second file: offset = 60_000.
        assert_eq!((out[2].start_ms, out[2].end_ms), (60_000, 60_000 + 90_000));
        assert_eq!(out[2].title, "Part 2 Chapter 1");
    }

    #[test]
    fn merge_synthesizes_part_n_for_files_without_chapters() {
        // File 1 has chapters; file 2 has none → synthesized
        // "Part 2" so coverage stays continuous.
        let files = vec![
            FileEntry {
                file_path: "01.m4b".into(),
                duration_ms: 60_000,
            },
            FileEntry {
                file_path: "02.m4b".into(),
                duration_ms: 90_000,
            },
        ];
        let per_file = vec![vec![(0, "Intro".into())], vec![]];
        let out = merge_per_file_chapters(&files, &per_file);
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].start_ms, out[0].end_ms), (0, 60_000));
        assert_eq!(out[0].title, "Intro");
        assert_eq!((out[1].start_ms, out[1].end_ms), (60_000, 150_000));
        assert_eq!(out[1].title, "Part 2");
    }

    #[test]
    fn merge_empty_title_falls_back_to_chapter_n() {
        let files = vec![FileEntry {
            file_path: "a.m4b".into(),
            duration_ms: 60_000,
        }];
        let per_file = vec![vec![(0_u64, String::new()), (30_000_u64, "  ".to_owned())]];
        let out = merge_per_file_chapters(&files, &per_file);
        assert_eq!(out[0].title, "Chapter 1");
        assert_eq!(out[1].title, "Chapter 2");
    }

    #[test]
    fn duration_to_millis_clamps_overflow() {
        let huge = std::time::Duration::from_secs(u64::MAX / 1000);
        let _ = duration_to_millis_u64(huge); // doesn't panic
    }
}
