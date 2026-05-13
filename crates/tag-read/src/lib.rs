//! Tag-read pipeline stage.
//!
//! For one book, walk its `book_files`, probe each file with
//! [`lofty`], and write back two kinds of output:
//!
//! 1. **Audio properties** → fixed columns on `book_files`
//!    (`duration_ms`, `bitrate_kbps`, `sample_rate_hz`, `channels`,
//!    `codec`).
//! 2. **Tag candidates** → `book_field_provenance` rows
//!    (`title`, `author`, `narrator`, `asin`, `isbn`, `language`,
//!    `publisher`). The merge step (slice 1C+) picks a winner.
//!
//! This stage never modifies the canonical `books` table directly —
//! that's `commit`'s job after merging candidates from every source
//! (tag-read, audnexus, audible, transcript).
//!
//! # Stage placement
//!
//! `tag-read` depends on `scan` having created the book +
//! `book_files` rows. It's the first real pipeline stage (scan is a
//! producer; see
//! `ab-scan` crate docs).

use async_trait::async_trait;

use ab_core::tunables::TagReadTunables;
use ab_core::{BookId, Error, Field, Result};
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};

/// Confidence written to `book_field_provenance` for tag-derived values.
///
/// Read from embedded MP4 atoms / ID3 frames. Tag-derived values are
/// more trustworthy than filename-derived (which is what `scan`
/// produces) but less than catalog-derived (Audnexus / Audible). The
/// merge step uses these to pick winners.
pub const TAG_CONFIDENCE: f64 = 0.7;

/// Provenance `source` string for values this stage writes.
pub const PROVENANCE_SOURCE: &str = "tag_file";

/// Typed identifier for this stage. Imported by dependents in
/// their `Stage::requires()` impls so a rename here surfaces
/// at compile time everywhere it's referenced.
pub const STAGE_ID: StageId = StageId::new("tag-read");

/// Stage that probes book files with lofty.
pub struct TagReadStage {
    tunables: TagReadTunables,
}

impl TagReadStage {
    /// Build a tag-read stage with the supplied tunables.
    pub const fn new(tunables: TagReadTunables) -> Self {
        Self { tunables }
    }
}

impl Default for TagReadStage {
    fn default() -> Self {
        Self::new(TagReadTunables::default())
    }
}

#[async_trait]
impl Stage for TagReadStage {
    fn name(&self) -> &'static str {
        STAGE_ID.as_str()
    }

    fn requires(&self) -> &'static [StageId] {
        &[]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        let files = fetch_book_files(&ctx.library, book_id).await?;
        if files.is_empty() {
            tracing::debug!(book = %book_id, "tag-read.no_files");
            return Ok(StageOutcome::Skipped);
        }

        for (file_id, file_path) in files {
            if ctx.cancel.is_cancelled() {
                return Err(Error::Invariant("tag-read cancelled"));
            }
            process_one_file(&ctx.library, book_id, file_id, &file_path, &self.tunables).await;
        }
        Ok(StageOutcome::Done)
    }
}

/// Fetch every active `book_files` row for a book. Returns
/// `(file_id, file_path)` pairs.
async fn fetch_book_files(
    library: &ab_db::LibraryDb,
    book_id: BookId,
) -> Result<Vec<(i64, String)>> {
    let id = book_id.0;
    // `file_id!` forces sqlx to treat the PK column as non-null —
    // SQLite reports `INTEGER PRIMARY KEY AUTOINCREMENT` as nullable
    // at the type level even though INSERT always materialises a
    // value. See sqlx docs on "column nullability overrides".
    let rows = sqlx::query!(
        r#"SELECT file_id AS "file_id!", file_path
           FROM book_files WHERE book_id = ? AND is_active = 1"#,
        id,
    )
    .fetch_all(library.pool())
    .await
    .map_err(|e| Error::Database(format!("tag-read fetch files: {e}")))?;
    Ok(rows.into_iter().map(|r| (r.file_id, r.file_path)).collect())
}

/// Process one file. Errors are logged + swallowed so a single bad
/// file doesn't abort the whole book.
async fn process_one_file(
    library: &ab_db::LibraryDb,
    book_id: BookId,
    file_id: i64,
    file_path: &str,
    tunables: &TagReadTunables,
) {
    let probe = match probe_with_lofty(file_path) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(book = %book_id, file = file_path, error = %e, "tag-read.probe_failed");
            return;
        }
    };

    if let Err(e) = update_book_file_properties(library, file_id, &probe.properties).await {
        tracing::warn!(book = %book_id, file = file_path, error = %e, "tag-read.write_properties_failed");
    }

    if tunables.write_provenance {
        for candidate in &probe.candidates {
            // Series gets a dedicated candidate table
            // (`book_series_candidate`) because the shape needs
            // series_asin + position + is_primary alongside the
            // name — see ADR-0017 (slice C5.6). Every other
            // Field variant goes through the
            // `book_field_provenance` scalar path.
            let result = match candidate.field {
                Field::Series => write_series_candidate(library, book_id, &candidate.value).await,
                _ => write_provenance(library, book_id, candidate.field, &candidate.value).await,
            };
            if let Err(e) = result {
                tracing::warn!(
                    book = %book_id,
                    field = %candidate.field,
                    error = %e,
                    "tag-read.provenance_write_failed"
                );
            }
        }
    }
}

/// What lofty extracted from one file.
#[derive(Debug, Clone, Default)]
struct ProbeResult {
    properties: AudioProperties,
    candidates: Vec<TagCandidate>,
}

/// Audio properties pulled from `lofty::file::FileProperties`. All
/// optional; lofty returns absent values as `None` or `0`.
#[derive(Debug, Clone, Default)]
struct AudioProperties {
    duration_ms: Option<i64>,
    bitrate_kbps: Option<i64>,
    sample_rate_hz: Option<i64>,
    channels: Option<i64>,
    codec: Option<String>,
}

/// Single tag-derived field candidate.
#[derive(Debug, Clone)]
struct TagCandidate {
    field: Field,
    value: String,
}

fn probe_with_lofty(file_path: &str) -> std::result::Result<ProbeResult, String> {
    let tagged = lofty::read_from_path(file_path).map_err(|e| format!("lofty open: {e}"))?;

    let properties = audio_properties_of(&tagged);
    let candidates = tag_candidates_of(&tagged);

    Ok(ProbeResult {
        properties,
        candidates,
    })
}

fn audio_properties_of(tagged: &lofty::file::TaggedFile) -> AudioProperties {
    use lofty::file::{AudioFile, TaggedFileExt};

    let props = tagged.properties();
    // `duration().as_millis()` returns u128 — can exceed i64 in theory
    // (≈ 292 million years); fallible conversion appropriate.
    let duration_ms = i64::try_from(props.duration().as_millis()).ok();
    // The remaining accessors return Option<u32>/Option<u8> which all
    // fit losslessly in i64.
    let bitrate_kbps = props.audio_bitrate().map(i64::from);
    let sample_rate_hz = props.sample_rate().map(i64::from);
    let channels = props.channels().map(i64::from);
    let codec = Some(format!("{:?}", tagged.file_type()));

    AudioProperties {
        duration_ms,
        bitrate_kbps,
        sample_rate_hz,
        channels,
        codec,
    }
}

fn tag_candidates_of(tagged: &lofty::file::TaggedFile) -> Vec<TagCandidate> {
    use lofty::file::TaggedFileExt;
    use lofty::tag::{Accessor, ItemKey};

    let Some(tag) = tagged.primary_tag() else {
        return Vec::new();
    };

    let mut out = Vec::with_capacity(8);
    if let Some(title) = tag.title() {
        push_candidate(&mut out, Field::Title, &title);
    }
    if let Some(artist) = tag.artist() {
        push_candidate(&mut out, Field::Author, &artist);
    }
    if let Some(album) = tag.album() {
        push_candidate(&mut out, Field::Series, &album);
    }
    // Lofty 0.24: `get_string` takes `ItemKey` by value; the
    // catch-all `ItemKey::Unknown(String)` variant was removed.
    // Free-form tag keys go through `ItemKey::from_key(TagType, &str)`
    // which returns `Option<ItemKey>`; that adds enough machinery that
    // we keep it for the next slice. For slice 1B, the typed accessors
    // cover the common fields.
    if let Some(language) = tag.get_string(ItemKey::Language) {
        // Normalise via the central language-code table so this
        // candidate is comparable to Audnexus / Audible / NL
        // detector outputs (otherwise tag-read might write
        // "eng", Audnexus writes "English", and consensus
        // treats them as different values).
        if let Some(canonical) = ab_core::language_code::normalize(language) {
            push_candidate(&mut out, Field::Language, &canonical);
        } else {
            tracing::warn!(
                raw = %language,
                "tag_read.language.unparseable"
            );
        }
    }
    if let Some(genre) = tag.get_string(ItemKey::Genre) {
        // Same normalize-on-write pattern as language: route
        // through the central `genre_code` table so tag-read
        // ("Sci-Fi"), Audnexus ("Science Fiction"), and any
        // future source converge on the canonical slug
        // ("science-fiction"). Multi-value genre tags split on
        // common separators (`,` / `;` / `/`) — MP4 / ID3
        // sometimes pack two genres into one string.
        for raw in split_multi_value(genre) {
            let raw = raw.trim();
            if raw.is_empty() {
                continue;
            }
            if let Some(canonical) = ab_core::genre_code::normalize(raw) {
                push_candidate(&mut out, Field::Genre, &canonical);
            } else {
                tracing::warn!(raw = %raw, "tag_read.genre.unparseable");
            }
        }
    }
    if let Some(publisher) = tag.get_string(ItemKey::Publisher) {
        push_candidate(&mut out, Field::Publisher, publisher);
    }
    if let Some(asin) = tag.get_string(ItemKey::CatalogNumber) {
        push_candidate(&mut out, Field::Asin, asin);
    }
    if let Some(isbn) = tag.get_string(ItemKey::Isrc) {
        push_candidate(&mut out, Field::Isbn, isbn);
    }
    out
}

fn push_candidate(out: &mut Vec<TagCandidate>, field: Field, value: &str) {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return;
    }
    out.push(TagCandidate {
        field,
        value: trimmed.to_owned(),
    });
}

/// Split a multi-value tag string on common audiobook
/// separators. MP4 / ID3 sometimes pack genre lists into one
/// `;`- or `/`- or `,`-separated string; some encoders use the
/// `0`-byte separator the spec permits, but lofty returns those
/// pre-split. We handle the in-string case.
///
/// Returns an iterator of substrings (not trimmed — caller
/// trims).
fn split_multi_value(value: &str) -> impl Iterator<Item = &str> {
    value.split([';', '/', ',', '|'])
}

async fn update_book_file_properties(
    library: &ab_db::LibraryDb,
    file_id: i64,
    props: &AudioProperties,
) -> Result<()> {
    let codec = props.codec.as_deref();
    sqlx::query!(
        "UPDATE book_files \
         SET duration_ms = ?, bitrate_kbps = ?, sample_rate_hz = ?, channels = ?, codec = ?, \
             checked_at = strftime('%s','now') \
         WHERE file_id = ?",
        props.duration_ms,
        props.bitrate_kbps,
        props.sample_rate_hz,
        props.channels,
        codec,
        file_id,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("update book_file: {e}")))?;
    Ok(())
}

/// Write a series candidate row sourced from the audio file's
/// album tag. Tag-read can't supply a `series_asin` (the album
/// tag is name-only) or a numeric `position` (the album tag is
/// just a string); identity-resolve fills in the rest via
/// case-insensitive name match against `series` (and any
/// `series.audible_id` if a higher-confidence source like
/// Audnexus seeded the row).
///
/// `is_primary` defaults to `1` — the album tag is conventionally
/// the book's primary series. Future sources (filename heuristics)
/// might write `0`.
async fn write_series_candidate(
    library: &ab_db::LibraryDb,
    book_id: BookId,
    series_name: &str,
) -> Result<()> {
    let id = book_id.0;
    let name = series_name.trim();
    if name.is_empty() {
        return Ok(());
    }
    sqlx::query!(
        "INSERT INTO book_series_candidate \
         (book_id, source, series_name, series_asin, position, is_primary, confidence) \
         VALUES (?, ?, ?, NULL, NULL, 1, ?)",
        id,
        PROVENANCE_SOURCE,
        name,
        TAG_CONFIDENCE,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("insert series candidate: {e}")))?;
    Ok(())
}

async fn write_provenance(
    library: &ab_db::LibraryDb,
    book_id: BookId,
    field: Field,
    value: &str,
) -> Result<()> {
    let id = book_id.0;
    let field_str = field.as_str();
    let stage_str = STAGE_ID.as_str();
    sqlx::query!(
        "INSERT INTO book_field_provenance \
         (book_id, field, value, source, stage, confidence, is_winner) \
         VALUES (?, ?, ?, ?, ?, ?, 0)",
        id,
        field_str,
        value,
        PROVENANCE_SOURCE,
        stage_str,
        TAG_CONFIDENCE,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("insert provenance: {e}")))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::fs;

    use ab_core::tunables::DbTunables;
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use super::*;

    async fn fresh_library(dir: &std::path::Path) -> ab_db::LibraryDb {
        let path = dir.join("library.db");
        ab_db::LibraryDb::open(&path, &DbTunables::default())
            .await
            .expect("open library")
    }

    async fn fresh_ephemeral(dir: &std::path::Path) -> ab_db::EphemeralDb {
        let path = dir.join("ephemeral.db");
        ab_db::EphemeralDb::open(&path, &DbTunables::default())
            .await
            .expect("open ephemeral")
    }

    #[tokio::test]
    async fn missing_files_returns_skipped() {
        let tmp = TempDir::new().expect("tmpdir");
        let library = fresh_library(tmp.path()).await;
        let ephemeral = fresh_ephemeral(tmp.path()).await;
        let ctx = StageContext {
            library,
            ephemeral,
            cancel: CancellationToken::new(),
            stage_name: "tag-read",
        };
        let stage = TagReadStage::default();
        let outcome = stage
            .run(&ctx, BookId(9999))
            .await
            .expect("stage runs without panic");
        assert_eq!(outcome, StageOutcome::Skipped);
    }

    #[tokio::test]
    async fn corrupt_file_logs_but_does_not_abort() {
        let tmp = TempDir::new().expect("tmpdir");
        let library = fresh_library(tmp.path()).await;
        let ephemeral = fresh_ephemeral(tmp.path()).await;

        // Seed a book + a "file" that lofty can't parse.
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'Test')")
            .execute(library.pool())
            .await
            .expect("insert book");
        let fake_path = tmp.path().join("corrupt.mp3");
        fs::write(&fake_path, b"not actually an mp3").expect("write fake file");
        sqlx::query(
            "INSERT INTO book_files (file_id, book_id, file_path, is_active) \
             VALUES (1, 1, ?, 1)",
        )
        .bind(fake_path.to_string_lossy().as_ref())
        .execute(library.pool())
        .await
        .expect("insert book_file");

        let ctx = StageContext {
            library,
            ephemeral,
            cancel: CancellationToken::new(),
            stage_name: "tag-read",
        };
        let stage = TagReadStage::default();
        let outcome = stage
            .run(&ctx, BookId(1))
            .await
            .expect("stage handles bad file gracefully");
        // Done — the loop completed; the individual file's failure
        // was logged but didn't abort.
        assert_eq!(outcome, StageOutcome::Done);
    }
}
