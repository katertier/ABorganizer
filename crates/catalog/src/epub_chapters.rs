//! EPUB navigation-document → audio chapter rows.
//!
//! When a book has an `EPUB` companion file paired in
//! `book_companions`, this stage reads the `EPUB`'s nav doc (`EPUB 3`
//! `<nav epub:type="toc">` preferred, `EPUB 2` `NCX` `<navMap>`
//! fallback) and emits `chapters` rows with `source='epub'`.
//!
//! ## Time mapping
//!
//! `EPUB` `ToC` is a logical chapter list — there's no native time
//! information. We use 1:1 file alignment: if the audiobook has
//! exactly the same number of audio files as the `EPUB` has
//! top-level chapter entries, we map each title to one file,
//! computing `start_ms` from the cumulative file offset and
//! `end_ms` from the file's duration. This covers the very common
//! "one chapter per file" packaging.
//!
//! When `N_files` ≠ `N_titles`, we skip with a tracing log. The
//! operator's audiobook will still use embedded / cue / audnexus
//! chapters when available; `epub_toc` is the fallback layer only.
//! Splitting / merging titles to fit a different file count would
//! require either silence-detection input (planned for later) or
//! per-chapter duration metadata the EPUB doesn't carry.
//!
//! ## Pipeline placement
//!
//! Source precedence in `chapter_winner`:
//! `audnexus > embedded > cue > epub > transcript > silence`.
//! This stage runs after the companion-file pairing stages (the
//! C.* cluster) so `book_companions` is populated.

use async_trait::async_trait;

use ab_core::{BookId, Error, Result};
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};

/// Provenance-source tag written to `chapters.source` for rows
/// produced by this stage. Matches the existing precedence vocab
/// in `chapter_winner::SOURCE_PRECEDENCE`.
pub const CHAPTERS_SOURCE: &str = "epub";

/// Stage that derives chapter rows from a book's paired EPUB
/// companion file.
pub struct EpubChaptersStage;

impl EpubChaptersStage {
    /// Construct. No tunables — companion paths live in the DB
    /// and the parser is pure-function over the file bytes.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for EpubChaptersStage {
    fn default() -> Self {
        Self::new()
    }
}

/// Typed identifier for this stage. `chapter_winner` adds this to
/// its `requires()` so the precedence pick sees this source.
pub const STAGE_ID: StageId = StageId::new("read-epub-chapters");

#[async_trait]
impl Stage for EpubChaptersStage {
    fn name(&self) -> &'static str {
        STAGE_ID.as_str()
    }

    fn requires(&self) -> &'static [StageId] {
        // read-tags populates book_files.duration_ms which we need
        // for the per-file time math.
        &[ab_tag_read::STAGE_ID]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        let Some(companion_path) = fetch_epub_companion(&ctx.library, book_id).await? else {
            return Ok(StageOutcome::Skipped);
        };

        let titles = match ab_companion_extract::read_chapter_titles_from_path(
            std::path::Path::new(&companion_path),
        ) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    book = %book_id,
                    path = %companion_path,
                    error = %e,
                    "epub_chapters.parse_failed"
                );
                return Ok(StageOutcome::Skipped);
            }
        };
        if titles.is_empty() {
            tracing::debug!(
                book = %book_id,
                path = %companion_path,
                "epub_chapters.no_titles"
            );
            return Ok(StageOutcome::Skipped);
        }

        let files = fetch_book_files(&ctx.library, book_id).await?;
        if files.is_empty() {
            return Ok(StageOutcome::Skipped);
        }

        if files.len() != titles.len() {
            tracing::info!(
                book = %book_id,
                files = files.len(),
                titles = titles.len(),
                "epub_chapters.file_title_mismatch"
            );
            return Ok(StageOutcome::Skipped);
        }

        // Replace any prior epub-sourced rows for this book so a
        // re-run after an EPUB swap doesn't accumulate.
        clear_existing(&ctx.library, book_id).await?;

        let mut offset_ms: i64 = 0;
        for (idx, (title, file)) in titles.iter().zip(files.iter()).enumerate() {
            let idx_i = i64::try_from(idx).unwrap_or(i64::MAX);
            let start_ms = offset_ms;
            let end_ms = offset_ms.saturating_add(file.duration_ms.max(0));
            insert_chapter(
                &ctx.library,
                InsertRow {
                    book_id,
                    idx: idx_i,
                    start_ms,
                    end_ms,
                    title,
                },
            )
            .await?;
            offset_ms = end_ms;
        }

        tracing::info!(
            book = %book_id,
            count = titles.len(),
            "epub_chapters.emitted"
        );
        Ok(StageOutcome::Done)
    }
}

struct FileEntry {
    duration_ms: i64,
}

async fn fetch_epub_companion(
    library: &ab_db::LibraryDb,
    book_id: BookId,
) -> Result<Option<String>> {
    let id = book_id.0;
    let row = sqlx::query!(
        r#"SELECT path AS "path!: String"
             FROM book_companions
            WHERE book_id = ? AND format = 'epub'
            ORDER BY companion_id
            LIMIT 1"#,
        id,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("epub_chapters fetch companion: {e}")))?;
    Ok(row.map(|r| r.path))
}

async fn fetch_book_files(library: &ab_db::LibraryDb, book_id: BookId) -> Result<Vec<FileEntry>> {
    let id = book_id.0;
    let rows = sqlx::query!(
        r#"SELECT COALESCE(duration_ms, 0) AS "duration_ms!: i64"
             FROM book_files
            WHERE book_id = ? AND is_active = 1
            ORDER BY file_id"#,
        id,
    )
    .fetch_all(library.pool())
    .await
    .map_err(|e| Error::Database(format!("epub_chapters fetch book_files: {e}")))?;
    Ok(rows
        .into_iter()
        .map(|r| FileEntry {
            duration_ms: r.duration_ms,
        })
        .collect())
}

async fn clear_existing(library: &ab_db::LibraryDb, book_id: BookId) -> Result<()> {
    let id = book_id.0;
    sqlx::query!(
        "DELETE FROM chapters WHERE book_id = ? AND source = ?",
        id,
        CHAPTERS_SOURCE,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("epub_chapters clear existing: {e}")))?;
    Ok(())
}

struct InsertRow<'a> {
    book_id: BookId,
    idx: i64,
    start_ms: i64,
    end_ms: i64,
    title: &'a str,
}

async fn insert_chapter(library: &ab_db::LibraryDb, row: InsertRow<'_>) -> Result<()> {
    let id = row.book_id.0;
    sqlx::query!(
        "INSERT INTO chapters (book_id, idx, start_ms, end_ms, title, source) \
         VALUES (?, ?, ?, ?, ?, ?)",
        id,
        row.idx,
        row.start_ms,
        row.end_ms,
        row.title,
        CHAPTERS_SOURCE,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("epub_chapters insert: {e}")))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use ab_core::tunables::DbTunables;
    use tempfile::TempDir;

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
            stage_name: "read-epub-chapters",
        }
    }

    async fn seed_book_with_files(ctx: &StageContext, durations: &[i64]) -> i64 {
        let book_id: i64 = sqlx::query_scalar(
            "INSERT INTO books (title, duration_ms, raw_duration_ms) \
             VALUES ('Test', ?, ?) RETURNING book_id",
        )
        .bind(durations.iter().sum::<i64>())
        .bind(durations.iter().sum::<i64>())
        .fetch_one(ctx.library.pool())
        .await
        .expect("insert book");
        for (i, &d) in durations.iter().enumerate() {
            sqlx::query(
                "INSERT INTO book_files (book_id, file_path, duration_ms) VALUES (?, ?, ?)",
            )
            .bind(book_id)
            .bind(format!("/test/{book_id}/{i}.m4b"))
            .bind(d)
            .execute(ctx.library.pool())
            .await
            .expect("insert file");
        }
        book_id
    }

    async fn seed_epub_companion(ctx: &StageContext, book_id: i64, path: &str) {
        sqlx::query(
            "INSERT INTO book_companions \
                (book_id, path, format, parse_tier, content_hash, bytes, discovered_at) \
              VALUES (?, ?, 'epub', 'text_extractable', 'h', 100, 0)",
        )
        .bind(book_id)
        .bind(path)
        .execute(ctx.library.pool())
        .await
        .expect("insert companion");
    }

    fn write_minimal_epub3_nav(dir: &std::path::Path, titles: &[&str]) -> std::path::PathBuf {
        use std::fmt::Write as _;
        use std::io::Write;
        use zip::ZipWriter;
        use zip::write::SimpleFileOptions;

        let path = dir.join("book.epub");
        let mut file = std::fs::File::create(&path).expect("create epub file");
        let mut zip = ZipWriter::new(&mut file);
        let opts = SimpleFileOptions::default();

        zip.start_file("mimetype", opts).unwrap();
        zip.write_all(b"application/epub+zip").unwrap();

        zip.start_file("META-INF/container.xml", opts).unwrap();
        zip.write_all(
            br#"<?xml version="1.0"?><container xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
<rootfiles><rootfile full-path="content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#,
        )
        .unwrap();

        zip.start_file("content.opf", opts).unwrap();
        zip.write_all(
            br#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0">
<manifest><item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/></manifest>
<spine/></package>"#,
        )
        .unwrap();

        let mut nav = String::from(
            r#"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<body><nav epub:type="toc"><ol>"#,
        );
        for (i, t) in titles.iter().enumerate() {
            let _ = write!(nav, "<li><a href=\"ch{i}.xhtml\">{t}</a></li>");
        }
        nav.push_str("</ol></nav></body></html>");
        zip.start_file("nav.xhtml", opts).unwrap();
        zip.write_all(nav.as_bytes()).unwrap();

        let _ = zip.finish().unwrap();
        path
    }

    #[tokio::test]
    async fn skips_when_no_epub_companion() {
        let tmp = TempDir::new().expect("tmp");
        let ctx = fresh_ctx(tmp.path()).await;
        let book_id = seed_book_with_files(&ctx, &[60_000]).await;
        let stage = EpubChaptersStage::new();
        let outcome = stage.run(&ctx, BookId(book_id)).await.expect("run");
        assert_eq!(outcome, StageOutcome::Skipped);
    }

    #[tokio::test]
    async fn one_to_one_alignment_emits_chapters_with_cumulative_offsets() {
        let tmp = TempDir::new().expect("tmp");
        let ctx = fresh_ctx(tmp.path()).await;
        let book_id = seed_book_with_files(&ctx, &[60_000, 90_000, 30_000]).await;
        let epub = write_minimal_epub3_nav(tmp.path(), &["One", "Two", "Three"]);
        seed_epub_companion(&ctx, book_id, epub.to_str().unwrap()).await;

        let stage = EpubChaptersStage::new();
        let outcome = stage.run(&ctx, BookId(book_id)).await.expect("run");
        assert_eq!(outcome, StageOutcome::Done);

        let rows: Vec<(i64, i64, i64, String)> = sqlx::query_as(
            "SELECT idx, start_ms, end_ms, title FROM chapters \
             WHERE book_id = ? AND source = 'epub' ORDER BY idx",
        )
        .bind(book_id)
        .fetch_all(ctx.library.pool())
        .await
        .expect("fetch chapters");
        assert_eq!(
            rows,
            vec![
                (0, 0, 60_000, "One".to_owned()),
                (1, 60_000, 150_000, "Two".to_owned()),
                (2, 150_000, 180_000, "Three".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn mismatched_file_and_title_counts_skips() {
        let tmp = TempDir::new().expect("tmp");
        let ctx = fresh_ctx(tmp.path()).await;
        let book_id = seed_book_with_files(&ctx, &[60_000, 60_000]).await;
        let epub = write_minimal_epub3_nav(tmp.path(), &["One", "Two", "Three"]);
        seed_epub_companion(&ctx, book_id, epub.to_str().unwrap()).await;

        let stage = EpubChaptersStage::new();
        let outcome = stage.run(&ctx, BookId(book_id)).await.expect("run");
        assert_eq!(outcome, StageOutcome::Skipped);

        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM chapters WHERE book_id = ? AND source = 'epub'",
        )
        .bind(book_id)
        .fetch_one(ctx.library.pool())
        .await
        .expect("count");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn rerun_replaces_prior_epub_rows() {
        let tmp = TempDir::new().expect("tmp");
        let ctx = fresh_ctx(tmp.path()).await;
        let book_id = seed_book_with_files(&ctx, &[60_000, 90_000]).await;
        let epub = write_minimal_epub3_nav(tmp.path(), &["A", "B"]);
        seed_epub_companion(&ctx, book_id, epub.to_str().unwrap()).await;

        let stage = EpubChaptersStage::new();
        stage.run(&ctx, BookId(book_id)).await.expect("first run");
        // Second run on the same inputs: idempotent, replaces in place.
        stage.run(&ctx, BookId(book_id)).await.expect("second run");

        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM chapters WHERE book_id = ? AND source = 'epub'",
        )
        .bind(book_id)
        .fetch_one(ctx.library.pool())
        .await
        .expect("count");
        assert_eq!(count, 2, "second run must not duplicate rows");
    }
}
