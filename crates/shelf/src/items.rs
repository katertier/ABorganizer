//! `GET /api/items/{id}` — book detail in ABS-shaped JSON.
//!
//! Maps ABorganizer's `books` + `book_files` rows into the
//! ABS `LibraryItem` schema subset that the tested clients
//! (`Plappa`, `ShelfPlayer`) actually read.
//!
//! ## Field mapping
//!
//! | ABS field | ABorganizer source | Notes |
//! |---|---|---|
//! | `id` | `books.book_id` (stringified) | ABS uses string IDs |
//! | `libraryId` | `libraries::LIBRARY_ID` | single-library model |
//! | `mediaType` | `"book"` | constant |
//! | `media.metadata.title` | `books.title` | |
//! | `media.metadata.subtitle` | `books.subtitle` | optional |
//! | `media.metadata.authorName` | authors join | comma-joined |
//! | `media.metadata.narratorName` | narrators join | comma-joined |
//! | `media.metadata.description` | `books.description` | |
//! | `media.metadata.publisher` | publishers join | |
//! | `media.metadata.language` | `books.language` | BCP-47 |
//! | `media.metadata.isbn` | `books.isbn` | |
//! | `media.metadata.asin` | `books.asin` | |
//! | `media.duration` | `books.duration_ms / 1000` | seconds (float) |
//! | `media.audioFiles[]` | active `book_files` | one entry per file |
//! | `media.audioFiles[].ino` | `book_files.file_id` (stringified) | |
//! | `media.audioFiles[].metadata.filename` | basename of `file_path` | |
//! | `media.audioFiles[].metadata.path` | full `file_path` | for diagnostics |
//! | `media.audioFiles[].duration` | `book_files.duration_ms / 1000` | seconds |
//! | `media.audioFiles[].mimeType` | format → MIME | `m4b`→`audio/mp4` etc. |
//! | `media.audioFiles[].metadata.size` | `book_files.file_size` | bytes |
//!
//! What we **don't** emit yet (deferred to a follow-up slice):
//!
//! - Cover URL — ABorganizer doesn't yet expose covers over
//!   HTTP, so the field is omitted. C3 (cover-art write path)
//!   will add a `/api/items/{id}/cover` endpoint and populate
//!   this.
//! - Chapter array — `chapters` table exists; mapping is
//!   straightforward but the MVP slice keeps it out.
//! - Series / track number — `book_series` + `book_setting`
//!   data lands when the player UI actually consumes it.

use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;

use crate::error::ShelfError;
use crate::libraries::LIBRARY_ID;
use crate::state::ShelfState;

/// Top-level item entry. ABS clients deserialise into a richer
/// schema — we emit the subset their renderers actually read.
#[derive(Debug, Serialize)]
pub struct LibraryItem {
    pub id: String,
    #[serde(rename = "libraryId")]
    pub library_id: &'static str,
    #[serde(rename = "mediaType")]
    pub media_type: &'static str,
    pub media: ItemMedia,
}

#[derive(Debug, Serialize)]
pub struct ItemMedia {
    pub metadata: ItemMetadata,
    /// Total duration in **seconds** (ABS convention). `0.0`
    /// when our `books.duration_ms` is NULL — clients tolerate
    /// missing duration on items that haven't finished
    /// fingerprinting / probing.
    pub duration: f64,
    #[serde(rename = "audioFiles")]
    pub audio_files: Vec<AudioFile>,
    /// Server-relative URL for the embedded cover-art image,
    /// e.g. `"/api/items/42/cover"`. Populated only when the
    /// book has at least one active `book_files` row (the
    /// cover endpoint itself returns 404 when no picture is
    /// embedded; clients that follow the link gracefully
    /// degrade). Omitted when the book has no active files
    /// at all.
    #[serde(rename = "coverPath", skip_serializing_if = "Option::is_none")]
    pub cover_path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ItemMetadata {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(rename = "authorName", skip_serializing_if = "Option::is_none")]
    pub author_name: Option<String>,
    #[serde(rename = "narratorName", skip_serializing_if = "Option::is_none")]
    pub narrator_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub isbn: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asin: Option<String>,
    #[serde(rename = "publishedYear", skip_serializing_if = "Option::is_none")]
    pub published_year: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AudioFile {
    /// Stringified `book_files.file_id`. ABS clients use this
    /// in the `/api/items/{id}/file/{ino}` path segment to
    /// stream the file.
    pub ino: String,
    pub metadata: AudioFileMetadata,
    /// Duration of THIS file in seconds (multi-file books
    /// concatenate to `media.duration`).
    pub duration: f64,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
}

#[derive(Debug, Serialize)]
pub struct AudioFileMetadata {
    pub filename: String,
    /// Full on-disk path. Surfaced for client-side diagnostics;
    /// ABS clients display this in the troubleshooting UI but
    /// don't act on it.
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<i64>,
}

/// `GET /api/items/{id}`. Path `id` is the stringified
/// `books.book_id`.
///
/// # Errors
///
/// - [`ShelfError::BadRequest`] when `id` doesn't parse as an
///   `i64`.
/// - [`ShelfError::NotFound`] when no book row matches.
/// - [`ShelfError::Database`] on any SQL failure.
#[allow(
    clippy::too_many_lines,
    reason = "single coherent SQL → DTO mapping; splitting hurts readability more than it helps"
)]
pub async fn get_item(
    State(state): State<ShelfState>,
    Path(id): Path<String>,
) -> Result<Json<LibraryItem>, ShelfError> {
    let book_id: i64 = id
        .parse()
        .map_err(|_| ShelfError::BadRequest(format!("invalid item id: {id}")))?;

    let book = sqlx::query!(
        r#"SELECT b.book_id      AS "book_id!: i64",
                  b.title        AS "title!: String",
                  b.subtitle     AS "subtitle?: String",
                  b.description  AS "description?: String",
                  b.language     AS "language?: String",
                  b.isbn         AS "isbn?: String",
                  b.asin         AS "asin?: String",
                  b.duration_ms  AS "duration_ms?: i64",
                  b.release_date AS "release_date?: String",
                  a.name         AS "author?: String",
                  p.name         AS "publisher?: String"
             FROM books b
             LEFT JOIN authors a ON a.author_id = b.author_id
             LEFT JOIN publishers p ON p.publisher_id = b.publisher_id
            WHERE b.book_id = ?"#,
        book_id,
    )
    .fetch_optional(state.library().pool())
    .await
    .map_err(|e| ShelfError::Database(format!("item lookup: {e}")))?
    .ok_or_else(|| ShelfError::NotFound(format!("item {book_id}")))?;

    let narrators = sqlx::query!(
        r#"SELECT n.name AS "name!: String"
             FROM book_narrator bn
             JOIN narrators n ON n.narrator_id = bn.narrator_id
            WHERE bn.book_id = ?
            ORDER BY n.name"#,
        book_id,
    )
    .fetch_all(state.library().pool())
    .await
    .map_err(|e| ShelfError::Database(format!("narrator lookup: {e}")))?
    .into_iter()
    .map(|r| r.name)
    .collect::<Vec<_>>();

    let files = sqlx::query!(
        r#"SELECT file_id    AS "file_id!: i64",
                  file_path  AS "file_path!: String",
                  file_size  AS "file_size?: i64",
                  format     AS "format?: String",
                  duration_ms AS "duration_ms?: i64"
             FROM book_files
            WHERE book_id = ? AND is_active = 1
            ORDER BY file_id"#,
        book_id,
    )
    .fetch_all(state.library().pool())
    .await
    .map_err(|e| ShelfError::Database(format!("files lookup: {e}")))?;

    let audio_files: Vec<AudioFile> = files
        .into_iter()
        .map(|f| AudioFile {
            ino: f.file_id.to_string(),
            metadata: AudioFileMetadata {
                filename: std::path::Path::new(&f.file_path)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&f.file_path)
                    .to_owned(),
                path: f.file_path.clone(),
                size: f.file_size,
            },
            duration: ms_to_secs(f.duration_ms),
            mime_type: mime_for_format(f.format.as_deref()),
        })
        .collect();

    let metadata = ItemMetadata {
        title: book.title,
        subtitle: book.subtitle,
        author_name: book.author,
        narrator_name: if narrators.is_empty() {
            None
        } else {
            Some(narrators.join(", "))
        },
        description: book.description,
        publisher: book.publisher,
        language: book.language,
        isbn: book.isbn,
        asin: book.asin,
        // `release_date` is ISO `YYYY-MM-DD`; ABS's
        // `publishedYear` wants just the year. Truncate on the
        // first `-` if present.
        published_year: book
            .release_date
            .as_ref()
            .and_then(|d| d.split('-').next())
            .map(str::to_owned),
    };

    // `coverPath` is the relative URL ABS clients hit on this
    // daemon's port. We surface it only when the book has at
    // least one active file (the cover-read path goes through
    // the first file's embedded `Picture`); books with no
    // files have nothing to point at.
    let cover_path = (!audio_files.is_empty())
        .then(|| format!("/api/items/{book_id}/cover", book_id = book.book_id));

    Ok(Json(LibraryItem {
        id: book.book_id.to_string(),
        library_id: LIBRARY_ID,
        media_type: "book",
        media: ItemMedia {
            metadata,
            duration: ms_to_secs(book.duration_ms),
            audio_files,
            cover_path,
        },
    }))
}

/// `book_files.format` (`m4b`, `m4a`, `mp3`, ...) → MIME type
/// for the streaming response's Content-Type.
fn mime_for_format(fmt: Option<&str>) -> String {
    let ext = fmt.unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "m4b" | "m4a" | "mp4" | "aac" => "audio/mp4".to_owned(),
        "mp3" => "audio/mpeg".to_owned(),
        "flac" => "audio/flac".to_owned(),
        "opus" | "ogg" => "audio/ogg".to_owned(),
        "wav" => "audio/wav".to_owned(),
        _ => mime_guess::from_ext(&ext)
            .first()
            .map_or_else(|| "application/octet-stream".to_owned(), |m| m.to_string()),
    }
}

/// Convert milliseconds to seconds as `f64` (ABS convention).
/// `None` / negative input → `0.0`.
fn ms_to_secs(ms: Option<i64>) -> f64 {
    #[allow(
        clippy::cast_precision_loss,
        reason = "audiobook durations fit f64 cleanly"
    )]
    match ms {
        Some(n) if n > 0 => n as f64 / 1000.0,
        _ => 0.0,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn ms_to_secs_handles_edge_cases() {
        assert!((ms_to_secs(Some(1500)) - 1.5).abs() < 1e-9);
        assert!((ms_to_secs(Some(0)) - 0.0).abs() < 1e-9);
        assert!((ms_to_secs(None) - 0.0).abs() < 1e-9);
        assert!((ms_to_secs(Some(-5)) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn mime_for_format_hardcoded_extensions() {
        assert_eq!(mime_for_format(Some("m4b")), "audio/mp4");
        assert_eq!(
            mime_for_format(Some("M4B")),
            "audio/mp4",
            "case-insensitive"
        );
        assert_eq!(mime_for_format(Some("mp3")), "audio/mpeg");
        assert_eq!(mime_for_format(Some("flac")), "audio/flac");
        assert_eq!(mime_for_format(Some("opus")), "audio/ogg");
    }

    #[test]
    fn mime_for_format_falls_back_to_mime_guess_or_octet_stream() {
        // Something mime_guess knows but we don't hardcode:
        // `.txt` → text/plain. Not audio, but exercises the fallback.
        let txt = mime_for_format(Some("txt"));
        assert!(txt.starts_with("text/plain"), "got {txt}");
        // Nonsense → octet-stream.
        let weird = mime_for_format(Some("zzznotrealcodec"));
        assert_eq!(weird, "application/octet-stream");
        // None → empty ext → octet-stream.
        assert_eq!(mime_for_format(None), "application/octet-stream");
    }
}
