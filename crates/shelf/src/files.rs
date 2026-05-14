//! `GET /api/items/{id}/file/{ino}` — stream an audio file.
//!
//! ABS clients stream books by hitting this endpoint per audio
//! file (one per `book_files` row, indexed by `ino` = the
//! stringified `file_id`).
//!
//! ## Range support (slice C1b-range)
//!
//! Honors `Range: bytes=...` per RFC 7233 with these shapes:
//!
//! - `bytes=N-M` (bounded) — serve `[N, min(M, total-1)]`.
//! - `bytes=N-` (open) — serve `[N, total-1]`.
//! - `bytes=-N` (suffix) — serve the last `N` bytes
//!   (`[total-N, total-1]`).
//!
//! Multi-range requests are rejected (416). The audiobook
//! players we target (Plappa, `ShelfPlayer`) send single-range
//! requests; multi-range is a rare desktop-browser pattern and
//! returning multipart/byteranges has enough framing cost to
//! defer until a real use case lands.
//!
//! Invalid / unsatisfiable ranges return **416 Range Not
//! Satisfiable** with `Content-Range: bytes */<total>` so the
//! client can re-issue inside bounds.
//!
//! Absent `Range:` header → unchanged behavior: 200 OK with
//! the whole file streamed.
//!
//! Content-Type is derived from `book_files.format` via
//! [`crate::items::mime_for_format`]'s logic, duplicated here as
//! `mime_for_path_extension` to avoid pulling a private fn
//! across modules. Symmetric — if one changes, the other should
//! too.

use std::path::PathBuf;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode, header};
use tokio::fs::File;
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _, SeekFrom};
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
/// Honors `Range:` per the module-level docs. Absent header →
/// 200 OK + full file (unchanged from C1).
///
/// # Errors
///
/// - [`ShelfError::BadRequest`] — `id` or `ino` doesn't parse.
/// - [`ShelfError::NotFound`] — no matching active `book_files`
///   row for the (`book_id`, `file_id`) pair.
/// - [`ShelfError::FileSystem`] — file path didn't open, or
///   metadata / seek failed.
pub async fn stream_file(
    State(state): State<ShelfState>,
    Path((id, ino)): Path<(String, String)>,
    request_headers: HeaderMap,
) -> Result<Response<Body>, ShelfError> {
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

    // Total size: trust the row's `file_size` when present,
    // otherwise stat the file. Range responses MUST include
    // a definite total; an open response (no Range) can elide
    // Content-Length but we prefer to include it.
    let total = if let Some(n) = row.file_size.filter(|n| *n > 0) {
        u64::try_from(n).unwrap_or(0)
    } else {
        let meta = file
            .metadata()
            .await
            .map_err(|e| ShelfError::FileSystem(format!("stat {}: {e}", path.display())))?;
        meta.len()
    };

    let mime = mime_for_path_extension(&path);
    let content_type = HeaderValue::from_str(&mime)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));

    // Parse Range header, if any.
    let range_header = request_headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok());

    match range_header.map(|s| parse_range_header(s, total)) {
        None => Ok(full_file_response(file, total, content_type)),
        Some(RangeParse::Single(r)) => partial_response(file, r, total, content_type, &path).await,
        Some(RangeParse::Invalid) => Ok(range_not_satisfiable_response(total, content_type)),
    }
}

/// 200 OK + full-file body. Same shape as the pre-C1b-range
/// behaviour — Accept-Ranges advertises range support, and
/// Content-Length is set when total > 0.
fn full_file_response(file: File, total: u64, content_type: HeaderValue) -> Response<Body> {
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ACCEPT_RANGES, "bytes");
    if total > 0 {
        response = response.header(header::CONTENT_LENGTH, total.to_string());
    }
    response
        .body(Body::from_stream(ReaderStream::new(file)))
        .unwrap_or_else(|_| Response::new(Body::from("response build error")))
}

/// 206 Partial Content for a resolved range. Seeks the file +
/// wraps in a length-bounded reader so the body stream stops
/// at the right byte.
async fn partial_response(
    mut file: File,
    r: ResolvedRange,
    total: u64,
    content_type: HeaderValue,
    path: &std::path::Path,
) -> Result<Response<Body>, ShelfError> {
    file.seek(SeekFrom::Start(r.start)).await.map_err(|e| {
        ShelfError::FileSystem(format!("seek {} → {}: {e}", path.display(), r.start))
    })?;
    let len = r.end - r.start + 1;
    let bounded = file.take(len);
    let body = Body::from_stream(ReaderStream::new(bounded));
    let content_range = format!("bytes {}-{}/{total}", r.start, r.end);
    let response = Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, len.to_string())
        .header(header::CONTENT_RANGE, content_range)
        .body(body)
        .map_err(|e| {
            ShelfError::FileSystem(format!("response build for {}: {e}", path.display()))
        })?;
    Ok(response)
}

/// 416 Range Not Satisfiable with `Content-Range: bytes */N`
/// so the client can see the file's total size and re-issue
/// inside bounds. Per RFC 7233 § 4.4.
fn range_not_satisfiable_response(total: u64, content_type: HeaderValue) -> Response<Body> {
    Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_RANGE, format!("bytes */{total}"))
        .header(header::CONTENT_LENGTH, "0")
        .body(Body::empty())
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// Inclusive byte range to serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ResolvedRange {
    start: u64,
    end: u64,
}

/// Outcome of parsing a `Range:` header against a known total
/// size. Three states: malformed/unsatisfiable, satisfiable
/// single, or "not really a Range request" (which we treat as
/// no header for the 200 path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RangeParse {
    /// Valid single byte range, clamped to `[0, total-1]`.
    Single(ResolvedRange),
    /// Header is present but malformed or unsatisfiable. The
    /// handler returns 416.
    Invalid,
}

/// Parse a `Range:` header against the file's total size.
///
/// Accepts the three RFC 7233 shapes for `bytes=`:
/// - `bytes=N-M`
/// - `bytes=N-` (open end)
/// - `bytes=-N` (suffix)
///
/// Multi-range requests (e.g. `bytes=0-499,1000-1499`) are
/// rejected as Invalid — see module docs for rationale.
///
/// Non-`bytes=` units (e.g. `seconds=`) are Invalid: per RFC
/// 7233 § 3.1 the server MAY ignore unknown units, but
/// returning 416 surfaces the misuse to the client.
fn parse_range_header(value: &str, total: u64) -> RangeParse {
    let Some(spec) = value.trim().strip_prefix("bytes=") else {
        return RangeParse::Invalid;
    };
    // Multi-range rejected.
    if spec.contains(',') {
        return RangeParse::Invalid;
    }
    let spec = spec.trim();
    let Some((start_str, end_str)) = spec.split_once('-') else {
        return RangeParse::Invalid;
    };
    let start_str = start_str.trim();
    let end_str = end_str.trim();

    // Suffix: `bytes=-N`. Empty start, non-empty end = suffix length.
    if start_str.is_empty() {
        if end_str.is_empty() {
            return RangeParse::Invalid;
        }
        let Ok(suffix_len) = end_str.parse::<u64>() else {
            return RangeParse::Invalid;
        };
        if suffix_len == 0 || total == 0 {
            return RangeParse::Invalid;
        }
        let start = total.saturating_sub(suffix_len);
        return RangeParse::Single(ResolvedRange {
            start,
            end: total - 1,
        });
    }

    let Ok(start) = start_str.parse::<u64>() else {
        return RangeParse::Invalid;
    };
    if start >= total {
        return RangeParse::Invalid;
    }

    // Open: `bytes=N-`.
    if end_str.is_empty() {
        return RangeParse::Single(ResolvedRange {
            start,
            end: total - 1,
        });
    }

    // Bounded: `bytes=N-M`.
    let Ok(end_raw) = end_str.parse::<u64>() else {
        return RangeParse::Invalid;
    };
    if end_raw < start {
        return RangeParse::Invalid;
    }
    let end = end_raw.min(total - 1);
    RangeParse::Single(ResolvedRange { start, end })
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

    // ── parse_range_header (slice C1b-range) ────────────────

    #[test]
    fn range_bounded_within_total() {
        let r = parse_range_header("bytes=10-99", 1000);
        assert_eq!(r, RangeParse::Single(ResolvedRange { start: 10, end: 99 }));
    }

    #[test]
    fn range_bounded_end_clamped_to_total_minus_one() {
        // Asking past EOF clamps to the last byte.
        let r = parse_range_header("bytes=500-99999", 1000);
        assert_eq!(
            r,
            RangeParse::Single(ResolvedRange {
                start: 500,
                end: 999,
            })
        );
    }

    #[test]
    fn range_open_end_runs_to_eof() {
        let r = parse_range_header("bytes=500-", 1000);
        assert_eq!(
            r,
            RangeParse::Single(ResolvedRange {
                start: 500,
                end: 999,
            })
        );
    }

    #[test]
    fn range_suffix_returns_last_n() {
        let r = parse_range_header("bytes=-100", 1000);
        assert_eq!(
            r,
            RangeParse::Single(ResolvedRange {
                start: 900,
                end: 999,
            })
        );
    }

    #[test]
    fn range_suffix_larger_than_total_starts_at_zero() {
        let r = parse_range_header("bytes=-99999", 1000);
        assert_eq!(r, RangeParse::Single(ResolvedRange { start: 0, end: 999 }));
    }

    #[test]
    fn range_start_at_eof_is_invalid() {
        // RFC: a `start` >= total is unsatisfiable.
        let r = parse_range_header("bytes=1000-", 1000);
        assert_eq!(r, RangeParse::Invalid);
    }

    #[test]
    fn range_inverted_is_invalid() {
        let r = parse_range_header("bytes=500-100", 1000);
        assert_eq!(r, RangeParse::Invalid);
    }

    #[test]
    fn range_multi_is_invalid_in_mvp() {
        // Multipart/byteranges is RFC-permitted but we defer
        // until a real client surfaces the use case.
        let r = parse_range_header("bytes=0-99,500-599", 1000);
        assert_eq!(r, RangeParse::Invalid);
    }

    #[test]
    fn range_non_bytes_unit_is_invalid() {
        let r = parse_range_header("seconds=0-10", 1000);
        assert_eq!(r, RangeParse::Invalid);
    }

    #[test]
    fn range_garbage_is_invalid() {
        assert_eq!(parse_range_header("hello", 1000), RangeParse::Invalid);
        assert_eq!(parse_range_header("bytes=", 1000), RangeParse::Invalid);
        assert_eq!(
            parse_range_header("bytes=foo-bar", 1000),
            RangeParse::Invalid
        );
        assert_eq!(parse_range_header("bytes=-", 1000), RangeParse::Invalid);
        assert_eq!(parse_range_header("bytes=-0", 1000), RangeParse::Invalid);
    }

    #[test]
    fn range_against_empty_file_is_invalid() {
        // No bytes to serve, so every range is unsatisfiable.
        assert_eq!(parse_range_header("bytes=0-", 0), RangeParse::Invalid);
        assert_eq!(parse_range_header("bytes=-10", 0), RangeParse::Invalid);
    }

    #[test]
    fn range_whitespace_tolerated() {
        let r = parse_range_header("  bytes= 10 - 99 ", 1000);
        assert_eq!(r, RangeParse::Single(ResolvedRange { start: 10, end: 99 }));
    }
}
