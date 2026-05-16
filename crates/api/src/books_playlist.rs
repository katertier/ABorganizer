//! `GET /api/v1/books/{book_id}/playlist.m3u8` — per-book M3U8
//! playlist export.
//!
//! Emits a standard `#EXTM3U` playlist with one track per active
//! `book_files` row, ordered by `file_id` (the same order the
//! player uses for continuous playback). Each track carries
//! `#EXTINF:<seconds>,<title>` metadata + the file's absolute
//! filesystem path.
//!
//! ## Use cases
//!
//! * **VLC / mpv / IINA against the SMB-mounted library** — the
//!   operator's primary path. The playlist references files via
//!   their on-disk paths; any player that can read the same
//!   filesystem (directly or via SMB mount) plays the book
//!   end-to-end without daemon involvement.
//! * **AirPlay / Sonos sources** — many players accept M3U8 with
//!   filesystem paths over SMB and stream from there.
//! * **Export / backup** — operator can save the M3U8 alongside
//!   the audio files as a self-contained playlist that survives
//!   the daemon being offline.
//!
//! ## Output format
//!
//! ```text
//! #EXTM3U
//! #EXTINF:13725,The Way of Kings
//! /Volumes/Audiobooks/Library/Sanderson, Brandon/The Way of Kings/file_01.m4b
//! #EXTINF:18036,The Way of Kings
//! /Volumes/Audiobooks/Library/Sanderson, Brandon/The Way of Kings/file_02.m4b
//! ```
//!
//! Single-file books emit a 1-track playlist; multi-file books
//! emit one `#EXTINF` line per file in `file_id` order. Files
//! with `duration_ms IS NULL` emit `#EXTINF:-1,…` (M3U8 sentinel
//! for "unknown duration"), which every standards-compliant
//! player handles gracefully.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::IntoResponse;

use std::fmt::Write as _;

use crate::ApiError;
use crate::state::ApiState;

/// `GET /api/v1/books/{book_id}/playlist.m3u8`
///
/// Returns a `200 OK` (`Content-Type: application/vnd.apple.mpegurl`)
/// with the M3U8 body. `404 Not Found` when the book doesn't exist or
/// has no active files (a freshly-imported book mid-scan won't have
/// rows yet — same response so the player gets a clean "not ready"
/// signal).
///
/// # Errors
///
/// Database access failures surface as [`ApiError::Database`].
#[allow(clippy::missing_panics_doc)] // panic-free
pub async fn books_playlist_m3u8(
    State(state): State<ApiState>,
    Path(book_id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let title = sqlx::query_scalar!(
        r#"SELECT title AS "title!: String" FROM books WHERE book_id = ?"#,
        book_id,
    )
    .fetch_optional(state.inner.library.pool())
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "playlist title lookup: {e}"
        )))
    })?;

    let Some(title) = title else {
        return Err(ApiError::NotFound(format!("book {book_id}")));
    };

    let files = sqlx::query!(
        r#"SELECT file_path AS "file_path!: String",
                  duration_ms AS "duration_ms: i64"
             FROM book_files
            WHERE book_id = ?
              AND is_active = 1
            ORDER BY file_id"#,
        book_id,
    )
    .fetch_all(state.inner.library.pool())
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "playlist files lookup: {e}"
        )))
    })?;

    if files.is_empty() {
        // Book exists but has no active files — same as 404 from
        // the player's perspective: nothing to play.
        return Err(ApiError::NotFound(format!(
            "book {book_id} has no active files"
        )));
    }

    let m3u8 = build_m3u8(
        &title,
        files.iter().map(|r| (r.file_path.as_str(), r.duration_ms)),
    );

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/vnd.apple.mpegurl"),
    );
    // Suggest a filename for clients that "Save As" the response.
    // Slug-quote the title light-handedly; the strict slug from
    // ab-shelf isn't load-bearing for a Content-Disposition hint.
    let suggested = simple_filename_slug(&title);
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{suggested}.m3u8\"")) {
        headers.insert(header::CONTENT_DISPOSITION, v);
    }

    Ok((StatusCode::OK, headers, m3u8))
}

/// Build an M3U8 body from `(file_path, duration_ms)` pairs.
///
/// Each track gets one `#EXTINF` line followed by the absolute
/// filesystem path. `duration_ms == None` becomes
/// `#EXTINF:-1,…` (M3U8 unknown-duration sentinel).
fn build_m3u8<'a>(title: &str, files: impl IntoIterator<Item = (&'a str, Option<i64>)>) -> String {
    let mut out = String::with_capacity(256);
    out.push_str("#EXTM3U\n");
    for (path, duration_ms) in files {
        let seconds = duration_ms.map_or(-1, |ms| (ms / 1000).max(0));
        let _ = writeln!(out, "#EXTINF:{seconds},{title}");
        out.push_str(path);
        out.push('\n');
    }
    out
}

/// Sanitise `title` to a safe filename component. Replaces
/// non-ASCII-alphanumeric runs with `-`, trims edges, caps at
/// 80 chars. Used for the `Content-Disposition: filename=…`
/// hint; not the file's on-disk name.
fn simple_filename_slug(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut last_dash = true;
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("book");
    }
    if out.len() > 80 {
        out.truncate(80);
        while out.ends_with('-') {
            out.pop();
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn build_m3u8_single_file_with_duration() {
        let m3u8 = build_m3u8(
            "The Way of Kings",
            [(
                "/Volumes/Audiobooks/Library/Sanderson, Brandon/file.m4b",
                Some(13_725_000),
            )],
        );
        assert!(m3u8.starts_with("#EXTM3U\n"));
        assert!(m3u8.contains("#EXTINF:13725,The Way of Kings\n"));
        assert!(m3u8.contains("/Volumes/Audiobooks/Library/Sanderson, Brandon/file.m4b"));
    }

    #[test]
    fn build_m3u8_unknown_duration_emits_minus_one() {
        let m3u8 = build_m3u8("Book", [("/path/file.m4b", None)]);
        assert!(m3u8.contains("#EXTINF:-1,Book\n"));
    }

    #[test]
    fn build_m3u8_multi_file_preserves_order() {
        let m3u8 = build_m3u8(
            "Multi-Part",
            [
                ("/a/01.m4b", Some(60_000)),
                ("/a/02.m4b", Some(120_000)),
                ("/a/03.m4b", Some(90_500)),
            ],
        );
        let lines: Vec<&str> = m3u8.lines().collect();
        // Header + 2 lines per track.
        assert_eq!(lines.len(), 1 + 3 * 2);
        assert_eq!(lines[1], "#EXTINF:60,Multi-Part");
        assert_eq!(lines[2], "/a/01.m4b");
        assert_eq!(lines[3], "#EXTINF:120,Multi-Part");
        assert_eq!(lines[5], "#EXTINF:90,Multi-Part");
    }

    #[test]
    fn build_m3u8_empty_iterator_returns_header_only() {
        let m3u8 = build_m3u8("Empty", std::iter::empty());
        assert_eq!(m3u8, "#EXTM3U\n");
    }

    #[test]
    fn simple_filename_slug_basic() {
        assert_eq!(simple_filename_slug("The Way of Kings"), "The-Way-of-Kings");
    }

    #[test]
    fn simple_filename_slug_punctuation_collapses() {
        assert_eq!(
            simple_filename_slug("Sanderson, Brandon — Mistborn!"),
            "Sanderson-Brandon-Mistborn"
        );
    }

    #[test]
    fn simple_filename_slug_empty_falls_back_to_book() {
        assert_eq!(simple_filename_slug(""), "book");
        assert_eq!(simple_filename_slug("!@#$%"), "book");
    }

    #[test]
    fn simple_filename_slug_caps_long() {
        let s = simple_filename_slug(&"x".repeat(200));
        assert!(s.len() <= 80);
    }
}
