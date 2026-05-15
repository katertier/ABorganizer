//! `extract-epub-name-dict` pipeline stage (ADR-0043 § C.4).
//!
//! For each book this stage:
//!
//! 1. Looks up the paired EPUB companion via `book_companions`
//!    (`book_id = ?`, `format = 'epub'`, `parse_tier =
//!    'text_extractable'`). If no row matches → `Skipped`.
//! 2. Reads the EPUB bytes from disk, subject to
//!    [`MAX_EPUB_BYTES`]. Files past the cap are logged + skipped
//!    so a single oversized companion doesn't poison the queue.
//! 3. Walks the spine + extracts the proper-noun name dictionary
//!    via [`ab_companion_extract::extract_name_dict_from_epub`].
//! 4. Writes a JSON `EpubNameDictPayload` to `ai_cache` keyed by
//!    `(book_id, CacheKey::EpubNameDict)` with the stage's
//!    `extractor_version` + the EPUB-declared language stamped
//!    on the `locale` column.
//! 5. Stamps `book_companions.parsed_at` so list views can
//!    surface "parsed" status without re-reading the cache.
//!
//! ## Idempotency
//!
//! Skips a book when an `ai_cache` row exists at the configured
//! `extractor_version`. Bump the version to force re-extract.
//! Manual invalidation: clear the row + the `parsed_at` column.
//!
//! ## Failure modes
//!
//! - No paired EPUB → `Skipped`.
//! - File too large → log warning + `Skipped`.
//! - File I/O error → log warning + `Skipped` (single companion
//!   failure shouldn't block other stages).
//! - EPUB walk error (corrupted ZIP, malformed OPF) → log warning
//!   + `Skipped`.
//! - Empty dictionary (no proper nouns above frequency floor) →
//!   write an empty payload + `Done` so the C.5 stage knows the
//!   companion was processed (and can short-circuit on empty
//!   dict per ADR § C.5 gating).
//! - DB write errors propagate as `Err`.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use ab_companion_extract::{NameEntry, extract_name_dict_from_epub};
use ab_core::{BookId, CacheKey, Error, Result};
use ab_db::LibraryDb;
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};

/// Typed identifier for this stage.
pub const STAGE_ID: StageId = StageId::new("extract-epub-name-dict");

/// Stage name written to `pipeline_progress`.
pub const STAGE_NAME: &str = STAGE_ID.as_str();

/// Schema-version stamp on every `ai_cache` row this stage writes.
/// Bump when the payload shape or extraction algorithm changes
/// materially; the next scan will see the old version + re-extract.
pub const EXTRACTOR_VERSION: &str = "c4-v1";

/// Hard cap on EPUB file size.
///
/// Defense-in-depth (B.2a posture): the workspace-wide JSON cap
/// is 32 `MiB`, but EPUB content includes embedded images +
/// fonts that legitimately push the container into tens of
/// `MiB` even for one book. 128 `MiB` is well above legitimate
/// growth (the largest audiobook EPUBs in the wild sit around
/// 40 `MiB`) and well below memory-pressure thresholds.
pub const MAX_EPUB_BYTES: u64 = 128 * 1024 * 1024;

/// JSON payload stored in `ai_cache.content`. The C.5 stage
/// reads this back via [`load_name_dict`].
#[derive(Debug, Serialize, Deserialize)]
pub struct EpubNameDictPayload {
    /// Proper-noun candidates, sorted descending by frequency
    /// (see `ab_companion_extract::extract_name_dict` contract).
    pub entries: Vec<NameEntry>,
    /// EPUB `dc:language`, lowercased. `None` when the OPF
    /// didn't declare one — the C.5 stage skips correction in
    /// that case (it can't verify language match).
    pub language: Option<String>,
}

/// The C.4 stage. Newtype because the constructor is `Default`-
/// only — there's nothing to configure per-instance yet.
#[derive(Default)]
pub struct ExtractEpubNameDictStage;

impl ExtractEpubNameDictStage {
    /// Construct the stage.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Stage for ExtractEpubNameDictStage {
    fn name(&self) -> &'static str {
        STAGE_NAME
    }

    fn requires(&self) -> &'static [StageId] {
        // No formal upstream — companions are discovered by the
        // scan stage (a non-Stage component, see ab-scan) and the
        // watchdog #125 idle target. Either path's job submission
        // can land this stage. If no paired EPUB exists yet, we
        // return Skipped and the watchdog re-submits when one
        // appears.
        &[]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        if cache_fresh(&ctx.library, book_id).await? {
            return Ok(StageOutcome::Skipped);
        }
        let Some(companion) = paired_epub_companion(&ctx.library, book_id).await? else {
            return Ok(StageOutcome::Skipped);
        };
        let Some(bytes) = read_epub_bytes(&companion.path).await? else {
            return Ok(StageOutcome::Skipped);
        };
        let (entries, language) = match extract_name_dict_from_epub(&bytes) {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(
                    book = %book_id,
                    path = %companion.path.display(),
                    error = %e,
                    "c4.epub_walk_failed",
                );
                return Ok(StageOutcome::Skipped);
            }
        };
        let payload = EpubNameDictPayload { entries, language };
        write_payload(&ctx.library, book_id, &payload, companion.companion_id).await?;
        Ok(StageOutcome::Done)
    }
}

/// Companion row narrowed to the fields this stage needs.
struct PairedCompanion {
    companion_id: i64,
    path: PathBuf,
}

/// Returns true when an `ai_cache` row exists at the current
/// `EXTRACTOR_VERSION`.
async fn cache_fresh(library: &LibraryDb, book_id: BookId) -> Result<bool> {
    let id = book_id.0;
    let cache_str = CacheKey::EpubNameDict.as_str();
    let row = sqlx::query!(
        "SELECT extractor_version FROM ai_cache \
         WHERE book_id = ? AND cache_type = ?",
        id,
        cache_str,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("c4 cache lookup: {e}")))?;
    Ok(row.is_some_and(|r| r.extractor_version.as_deref() == Some(EXTRACTOR_VERSION)))
}

async fn paired_epub_companion(
    library: &LibraryDb,
    book_id: BookId,
) -> Result<Option<PairedCompanion>> {
    let id = book_id.0;
    // text_extractable is the only parse_tier that C.4 operates
    // on; ebook_opaque / comic / document don't have spine HTML.
    let row = sqlx::query!(
        r#"SELECT companion_id AS "companion_id!: i64", path
           FROM book_companions
           WHERE book_id = ? AND format = 'epub' AND parse_tier = 'text_extractable'
           ORDER BY companion_id LIMIT 1"#,
        id,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("c4 companion lookup: {e}")))?;
    Ok(row.map(|r| PairedCompanion {
        companion_id: r.companion_id,
        path: PathBuf::from(r.path),
    }))
}

/// Read the EPUB bytes from disk subject to [`MAX_EPUB_BYTES`].
/// Returns `Ok(None)` for the recoverable cases (file missing,
/// too big, unreadable) so the stage skips rather than fails.
async fn read_epub_bytes(path: &std::path::Path) -> Result<Option<Arc<Vec<u8>>>> {
    let meta = match tokio::fs::metadata(path).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "c4.metadata_failed");
            return Ok(None);
        }
    };
    if meta.len() > MAX_EPUB_BYTES {
        tracing::warn!(
            path = %path.display(),
            bytes = meta.len(),
            cap = MAX_EPUB_BYTES,
            "c4.skip_oversized",
        );
        return Ok(None);
    }
    match tokio::fs::read(path).await {
        Ok(bytes) => Ok(Some(Arc::new(bytes))),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "c4.read_failed");
            Ok(None)
        }
    }
}

/// Atomic write: `ai_cache` + `book_companions.parsed_at` in one
/// transaction so a crash mid-write doesn't leave the cache
/// row without the `parsed_at` stamp (or vice versa).
async fn write_payload(
    library: &LibraryDb,
    book_id: BookId,
    payload: &EpubNameDictPayload,
    companion_id: i64,
) -> Result<()> {
    let content = serde_json::to_vec(payload)
        .map_err(|e| Error::Database(format!("c4 payload encode: {e}")))?;
    let id = book_id.0;
    let cache_str = CacheKey::EpubNameDict.as_str();
    let language = payload.language.as_deref();

    let mut tx = library
        .pool()
        .begin()
        .await
        .map_err(|e| Error::Database(format!("c4 begin tx: {e}")))?;
    sqlx::query!(
        "INSERT INTO ai_cache \
         (book_id, cache_type, content, compressed, extractor_version, locale, created_at) \
         VALUES (?, ?, ?, 0, ?, ?, strftime('%s','now')) \
         ON CONFLICT(book_id, cache_type) DO UPDATE SET \
             content = excluded.content, \
             compressed = excluded.compressed, \
             extractor_version = excluded.extractor_version, \
             locale = excluded.locale, \
             created_at = excluded.created_at",
        id,
        cache_str,
        content,
        EXTRACTOR_VERSION,
        language,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| Error::Database(format!("c4 ai_cache write: {e}")))?;
    sqlx::query!(
        "UPDATE book_companions SET parsed_at = strftime('%s','now') WHERE companion_id = ?",
        companion_id,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| Error::Database(format!("c4 parsed_at stamp: {e}")))?;
    tx.commit()
        .await
        .map_err(|e| Error::Database(format!("c4 commit: {e}")))?;
    Ok(())
}

/// Read back a previously-written payload for use by the C.5 stage.
///
/// Also consumed by `aborg book retry`. Returns `None` when no
/// row exists or the JSON is malformed; the C.5 stage treats
/// both as "no dict available, skip correction."
pub async fn load_name_dict(
    library: &LibraryDb,
    book_id: BookId,
) -> Result<Option<EpubNameDictPayload>> {
    let id = book_id.0;
    let cache_str = CacheKey::EpubNameDict.as_str();
    let row = sqlx::query!(
        "SELECT content FROM ai_cache WHERE book_id = ? AND cache_type = ?",
        id,
        cache_str,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("c4 load: {e}")))?;
    let Some(row) = row else { return Ok(None) };
    let Some(bytes) = row.content else {
        return Ok(None);
    };
    match ab_core::cache::deserialize_cache_content::<EpubNameDictPayload>(&bytes) {
        Ok(p) => Ok(Some(p)),
        Err(e) => {
            tracing::warn!(book = %book_id, error = %e, "c4.payload_parse_failed");
            Ok(None)
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::io::{Cursor, Write};

    use ab_core::tunables::DbTunables;
    use tempfile::TempDir;
    use zip::{ZipWriter, write::SimpleFileOptions};

    use super::*;

    async fn fresh_library() -> (LibraryDb, TempDir) {
        let tmp = TempDir::new().expect("tmpdir");
        let lib = LibraryDb::open(&tmp.path().join("library.db"), &DbTunables::default())
            .await
            .expect("open db");
        (lib, tmp)
    }

    async fn insert_book(library: &LibraryDb, title: &str) -> i64 {
        sqlx::query_scalar!(
            "INSERT INTO books (title) VALUES (?) RETURNING book_id AS \"book_id!: i64\"",
            title,
        )
        .fetch_one(library.pool())
        .await
        .unwrap()
    }

    async fn insert_companion(
        library: &LibraryDb,
        book_id: i64,
        path: &str,
        format: &str,
        parse_tier: &str,
    ) -> i64 {
        sqlx::query_scalar!(
            "INSERT INTO book_companions \
             (book_id, path, format, parse_tier, content_hash, bytes, discovered_at) \
             VALUES (?, ?, ?, ?, 'deadbeef', 0, 0) \
             RETURNING companion_id AS \"companion_id!: i64\"",
            book_id,
            path,
            format,
            parse_tier,
        )
        .fetch_one(library.pool())
        .await
        .unwrap()
    }

    fn build_minimal_epub(opf_path: &str, opf_body: &str, chapters: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let cursor = Cursor::new(&mut buf);
            let mut zip = ZipWriter::new(cursor);
            let opts =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            zip.start_file("mimetype", opts).unwrap();
            zip.write_all(b"application/epub+zip").unwrap();
            zip.start_file("META-INF/container.xml", opts).unwrap();
            let container = format!(
                r#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="{opf_path}" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#
            );
            zip.write_all(container.as_bytes()).unwrap();
            zip.start_file(opf_path, opts).unwrap();
            zip.write_all(opf_body.as_bytes()).unwrap();
            let opf_dir = opf_path.rsplit_once('/').map_or("", |(d, _)| d);
            for (href, body) in chapters {
                let full = if opf_dir.is_empty() {
                    (*href).to_owned()
                } else {
                    format!("{opf_dir}/{href}")
                };
                zip.start_file(full, opts).unwrap();
                zip.write_all(body.as_bytes()).unwrap();
            }
            zip.finish().unwrap();
        }
        buf
    }

    const OPF: &str = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Test</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="c1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine><itemref idref="c1"/></spine>
</package>"#;

    fn write_test_epub(tmp: &TempDir, body: &str) -> PathBuf {
        let path = tmp.path().join("companion.epub");
        let bytes = build_minimal_epub("OEBPS/content.opf", OPF, &[("ch1.xhtml", body)]);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[tokio::test]
    async fn skip_when_no_paired_epub() {
        let (lib, tmp) = fresh_library().await;
        let book_id = insert_book(&lib, "test").await;
        let stage = ExtractEpubNameDictStage::new();
        let ctx = StageContext {
            library: lib.clone(),
            ephemeral: ab_db::EphemeralDb::open(
                &tmp.path().join("ephemeral.db"),
                &DbTunables::default(),
            )
            .await
            .expect("ephemeral"),
            cancel: tokio_util::sync::CancellationToken::new(),
            stage_name: STAGE_NAME,
        };
        let outcome = stage.run(&ctx, BookId(book_id)).await.unwrap();
        assert_eq!(outcome, StageOutcome::Skipped);
    }

    #[tokio::test]
    async fn writes_cache_for_paired_epub_with_dict() {
        let (lib, tmp) = fresh_library().await;
        let book_id = insert_book(&lib, "test").await;
        let body = "<body>He saw Kaladin again. Later, Kaladin spoke. \
                    The truth was that Kaladin knew.</body>";
        let path = write_test_epub(&tmp, body);
        let companion_id = insert_companion(
            &lib,
            book_id,
            path.to_str().unwrap(),
            "epub",
            "text_extractable",
        )
        .await;
        let stage = ExtractEpubNameDictStage::new();
        let ctx = StageContext {
            library: lib.clone(),
            ephemeral: ab_db::EphemeralDb::open(
                &tmp.path().join("ephemeral.db"),
                &DbTunables::default(),
            )
            .await
            .expect("ephemeral"),
            cancel: tokio_util::sync::CancellationToken::new(),
            stage_name: STAGE_NAME,
        };
        let outcome = stage.run(&ctx, BookId(book_id)).await.unwrap();
        assert_eq!(outcome, StageOutcome::Done);

        let payload = load_name_dict(&lib, BookId(book_id))
            .await
            .unwrap()
            .expect("payload present");
        assert_eq!(payload.language.as_deref(), Some("en"));
        assert!(
            payload.entries.iter().any(|e| e.surface == "Kaladin"),
            "expected Kaladin in dict: {:?}",
            payload.entries,
        );

        // parsed_at stamped.
        let parsed_at: Option<i64> = sqlx::query_scalar!(
            "SELECT parsed_at FROM book_companions WHERE companion_id = ?",
            companion_id,
        )
        .fetch_one(lib.pool())
        .await
        .unwrap();
        assert!(parsed_at.is_some(), "parsed_at must be set");
    }

    #[tokio::test]
    async fn second_run_is_skipped_by_extractor_version_check() {
        let (lib, tmp) = fresh_library().await;
        let book_id = insert_book(&lib, "test").await;
        let body = "<body>Kaladin Kaladin Kaladin.</body>";
        let path = write_test_epub(&tmp, body);
        insert_companion(
            &lib,
            book_id,
            path.to_str().unwrap(),
            "epub",
            "text_extractable",
        )
        .await;
        let stage = ExtractEpubNameDictStage::new();
        let ctx = StageContext {
            library: lib.clone(),
            ephemeral: ab_db::EphemeralDb::open(
                &tmp.path().join("ephemeral.db"),
                &DbTunables::default(),
            )
            .await
            .expect("ephemeral"),
            cancel: tokio_util::sync::CancellationToken::new(),
            stage_name: STAGE_NAME,
        };
        let first = stage.run(&ctx, BookId(book_id)).await.unwrap();
        assert_eq!(first, StageOutcome::Done);
        let second = stage.run(&ctx, BookId(book_id)).await.unwrap();
        assert_eq!(second, StageOutcome::Skipped);
    }

    #[tokio::test]
    async fn skip_when_companion_path_missing() {
        let (lib, tmp) = fresh_library().await;
        let book_id = insert_book(&lib, "test").await;
        insert_companion(
            &lib,
            book_id,
            "/nonexistent/epub.epub",
            "epub",
            "text_extractable",
        )
        .await;
        let stage = ExtractEpubNameDictStage::new();
        let ctx = StageContext {
            library: lib.clone(),
            ephemeral: ab_db::EphemeralDb::open(
                &tmp.path().join("ephemeral.db"),
                &DbTunables::default(),
            )
            .await
            .expect("ephemeral"),
            cancel: tokio_util::sync::CancellationToken::new(),
            stage_name: STAGE_NAME,
        };
        let outcome = stage.run(&ctx, BookId(book_id)).await.unwrap();
        assert_eq!(outcome, StageOutcome::Skipped);
    }

    #[tokio::test]
    async fn opaque_format_companion_is_ignored() {
        let (lib, tmp) = fresh_library().await;
        let book_id = insert_book(&lib, "test").await;
        let path = write_test_epub(&tmp, "<body>Kaladin Kaladin Kaladin.</body>");
        insert_companion(
            &lib,
            book_id,
            path.to_str().unwrap(),
            "mobi",
            "ebook_opaque",
        )
        .await;
        let stage = ExtractEpubNameDictStage::new();
        let ctx = StageContext {
            library: lib.clone(),
            ephemeral: ab_db::EphemeralDb::open(
                &tmp.path().join("ephemeral.db"),
                &DbTunables::default(),
            )
            .await
            .expect("ephemeral"),
            cancel: tokio_util::sync::CancellationToken::new(),
            stage_name: STAGE_NAME,
        };
        let outcome = stage.run(&ctx, BookId(book_id)).await.unwrap();
        assert_eq!(outcome, StageOutcome::Skipped);
    }
}
