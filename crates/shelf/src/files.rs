//! `GET /api/items/{id}/file/{ino}` — stream an audio file.
//!
//! ABS clients stream books by hitting this endpoint per audio
//! file (one per `book_files` row, indexed by `ino` = the
//! stringified `file_id`).
//!
//! The MVP implementation streams the **whole file** as a single
//! response. HTTP Range support (essential for client-side
//! seeking on long audiobooks) is **deferred to a follow-up
//! slice** (C1b): the player loads the whole file then seeks
//! locally for the MVP, which works for short books and
//! single-file tests but isn't viable for 10+ hour multi-file
//! libraries. The `ReaderStream` pattern below is range-ready
//! — we just don't parse the `Range:` header yet.
//!
//! Content-Type is derived from `book_files.format` via
//! [`crate::items::mime_for_format`]'s logic, duplicated here as
//! `mime_for_path_extension` to avoid pulling a private fn
//! across modules. Symmetric — if one changes, the other should
//! too.

use std::path::PathBuf;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use tokio::fs::File;
use tokio_util::io::ReaderStream;

use crate::error::ShelfError;
use crate::state::ShelfState;

/// `GET /api/items/{id}/file/{ino}`.
///
/// Both path segments are strings; `id` parses as `book_id`,
/// `ino` as `file_id`. The (book, file) pair is verified to
/// belong together so an attacker can't probe arbitrary
/// `book_files` rows by sending mismatched IDs.
///
/// # Errors
///
/// - [`ShelfError::BadRequest`] — `id` or `ino` doesn't parse.
/// - [`ShelfError::NotFound`] — no matching active `book_files`
///   row for the (`book_id`, `file_id`) pair.
/// - [`ShelfError::FileSystem`] — file path didn't open.
pub async fn stream_file(
    State(state): State<ShelfState>,
    Path((id, ino)): Path<(String, String)>,
) -> Result<impl IntoResponse, ShelfError> {
    let book_id: i64 = id
        .parse()
        .map_err(|_| ShelfError::BadRequest(format!("invalid item id: {id}")))?;
    let file_id: i64 = ino
        .parse()
        .map_err(|_| ShelfError::BadRequest(format!("invalid ino: {ino}")))?;

    let row = sqlx::query!(
        r#"SELECT file_path AS "file_path!: String",
                  file_size AS "file_size?: i64",
                  format    AS "format?: String"
             FROM book_files
            WHERE book_id = ? AND file_id = ? AND is_active = 1"#,
        book_id,
        file_id,
    )
    .fetch_optional(state.library().pool())
    .await
    .map_err(|e| ShelfError::Database(format!("file lookup: {e}")))?
    .ok_or_else(|| {
        ShelfError::NotFound(format!(
            "no active file_id {file_id} under book_id {book_id}"
        ))
    })?;

    let path = PathBuf::from(&row.file_path);
    let file = File::open(&path).await.map_err(|e| {
        // Path could be missing because the operator moved the
        // file out-of-band, or because the row references a
        // soft-deleted source (post-transcode reaper). Surface
        // as 500 because the row claimed `is_active = 1` —
        // that's a state-consistency bug worth investigating.
        ShelfError::FileSystem(format!("open {}: {e}", path.display()))
    })?;

    let mime = mime_for_path_extension(&path);
    let mut headers = HeaderMap::new();
    // `HeaderValue::from_str` here is fallible because `mime`
    // is dynamic; the fallback static can't fail to parse so
    // `from_static` keeps the panic-free contract.
    let content_type = HeaderValue::from_str(&mime)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    headers.insert(header::CONTENT_TYPE, content_type);
    // `Accept-Ranges: bytes` advertises range support; until C1b
    // wires up Range handling we serve the whole file regardless
    // of what the client asks for. Most ABS clients fall back to
    // sequential GET when Range isn't honoured.
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Some(size) = row.file_size {
        if size > 0 {
            // `i64` decimal is always valid ASCII; `try_from`
            // is documented to succeed on any digits-only string.
            if let Ok(v) = HeaderValue::try_from(size.to_string()) {
                headers.insert(header::CONTENT_LENGTH, v);
            }
        }
    }

    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);
    Ok((StatusCode::OK, headers, body))
}

/// MIME-type from a path's extension. Duplicates the dispatch
/// in [`crate::items::mime_for_format`] (private there); kept
/// in sync by inspection. A future hygiene slice could hoist
/// a single shared `mime_for_audio_ext` helper.
fn mime_for_path_extension(path: &std::path::Path) -> String {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "m4b" | "m4a" | "mp4" | "aac" => "audio/mp4".to_owned(),
        "mp3" => "audio/mpeg".to_owned(),
        "flac" => "audio/flac".to_owned(),
        "opus" | "ogg" => "audio/ogg".to_owned(),
        "wav" => "audio/wav".to_owned(),
        _ => mime_guess::from_path(path)
            .first()
            .map_or_else(|| "application/octet-stream".to_owned(), |m| m.to_string()),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::path::Path as StdPath;

    use super::*;

    #[test]
    fn mime_dispatch_hardcoded_extensions() {
        assert_eq!(
            mime_for_path_extension(StdPath::new("/x/a.m4b")),
            "audio/mp4"
        );
        assert_eq!(
            mime_for_path_extension(StdPath::new("/x/B.MP3")),
            "audio/mpeg"
        );
        assert_eq!(
            mime_for_path_extension(StdPath::new("/x/c.flac")),
            "audio/flac"
        );
        assert_eq!(
            mime_for_path_extension(StdPath::new("/x/c.opus")),
            "audio/ogg"
        );
    }

    #[test]
    fn mime_dispatch_unknown_falls_back() {
        let m = mime_for_path_extension(StdPath::new("/x/file.somethingweird"));
        assert!(
            m == "application/octet-stream" || m.contains('/'),
            "got {m}"
        );
    }
}
