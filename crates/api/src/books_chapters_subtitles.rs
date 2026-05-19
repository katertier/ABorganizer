//! `GET /api/v1/books/{book_id}/chapters.{srt,vtt}` — chapter
//! marks rendered as timed-text subtitles.
//!
//! Reads the `transcript_chapter_marks` cache row produced by
//! [`ab_transcript::chapter_marks_stage`] and emits one cue per
//! chapter — title as the cue text, `start_ms` / `end_ms` as the
//! cue timing. Two sibling endpoints share the same lookup +
//! payload shape; only the cue-block formatter differs (SRT uses
//! `,` decimal separator and no header; `WebVTT` uses `.` and a
//! `WEBVTT` magic line).
//!
//! ## Use cases
//!
//! - **Media players** — VLC, mpv, IINA, web players (HTML5
//!   `<track>`) and Plex / Jellyfin all consume SRT or `WebVTT` for
//!   chapter overlays.
//! - **Re-importable artefacts** — operator can hand-edit the
//!   file in any text editor and feed it back as a chapter source
//!   in a future slice.
//! - **Re-mux paths** — `ffmpeg -i in.m4b -i in.srt -map 0 -map 1
//!   -c copy out.mkv` lands chapter titles as a real subtitle
//!   track without re-encoding.
//!
//! `400 Bad Request` is reserved for future query-string
//! validation (no params yet). `404 Not Found` covers both
//! "no book" and "chapter marks not yet computed". `500 Internal
//! Server Error` for DB errors / cache-row JSON parse failures
//! (the latter only fires on a corrupt cache row that bypassed
//! the writer's schema — a real outage signal).

use std::fmt::Write as _;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::IntoResponse;

use ab_core::CacheKey;
use ab_transcript::chapter_marks_stage::ChapterMarksPayload;

use crate::ApiError;
use crate::state::ApiState;

/// `GET /api/v1/books/{book_id}/chapters.srt`
///
/// Returns `200 OK` with `Content-Type: application/x-subrip` and
/// the rendered `SubRip` cues. See [`render_srt`] for the format
/// shape.
///
/// # Errors
///
/// See module-level doc for the error matrix.
#[allow(clippy::missing_panics_doc)]
pub async fn books_chapters_srt(
    State(state): State<ApiState>,
    Path(book_id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let payload = load_chapter_marks(&state, book_id).await?;
    let body = render_srt(&payload);

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-subrip; charset=utf-8"),
    );
    if let Ok(v) = HeaderValue::from_str(&format!(
        "attachment; filename=\"book-{book_id}-chapters.srt\""
    )) {
        headers.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok((StatusCode::OK, headers, body))
}

/// `GET /api/v1/books/{book_id}/chapters.vtt`
///
/// Returns `200 OK` with `Content-Type: text/vtt` and the
/// rendered `WebVTT` cues. See [`render_vtt`] for the format shape.
///
/// # Errors
///
/// See module-level doc for the error matrix.
#[allow(clippy::missing_panics_doc)]
pub async fn books_chapters_vtt(
    State(state): State<ApiState>,
    Path(book_id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let payload = load_chapter_marks(&state, book_id).await?;
    let body = render_vtt(&payload);

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/vtt; charset=utf-8"),
    );
    if let Ok(v) = HeaderValue::from_str(&format!(
        "attachment; filename=\"book-{book_id}-chapters.vtt\""
    )) {
        headers.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok((StatusCode::OK, headers, body))
}

async fn load_chapter_marks(
    state: &ApiState,
    book_id: i64,
) -> Result<ChapterMarksPayload, ApiError> {
    let book_exists = sqlx::query_scalar!(
        r#"SELECT 1 AS "n!: i64" FROM books WHERE book_id = ?"#,
        book_id,
    )
    .fetch_optional(state.inner.library.pool())
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "chapters book lookup: {e}"
        )))
    })?
    .is_some();
    if !book_exists {
        return Err(ApiError::NotFound(format!("book {book_id}")));
    }

    let cache_type = CacheKey::TranscriptChapterMarks.as_str();
    let content = sqlx::query_scalar!(
        r#"SELECT content AS "content!: String"
             FROM ai_cache
            WHERE book_id = ? AND cache_type = ?"#,
        book_id,
        cache_type,
    )
    .fetch_optional(state.inner.library.pool())
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "chapters cache lookup: {e}"
        )))
    })?;

    let Some(content) = content else {
        return Err(ApiError::NotFound(format!(
            "book {book_id} has no chapter marks cached yet"
        )));
    };

    serde_json::from_str::<ChapterMarksPayload>(&content).map_err(|e| {
        tracing::warn!(book_id, error = %e, "chapters.cache_parse_failed");
        ApiError::Internal(ab_core::Error::Invariant(
            "ai_cache.transcript_chapter_marks row failed JSON parse: corrupt cache (bug or manual edit)",
        ))
    })
}

/// Render the payload as `SubRip` cues.
///
/// One cue per chapter, ordered by `idx`. Timestamps use `SubRip`'s
/// `HH:MM:SS,mmm` format (comma decimal separator). Empty payload
/// yields an empty string — clients should treat that as "no
/// chapters" without surfacing an HTTP error, since the 200 path
/// is reserved for "we have something to return."
fn render_srt(payload: &ChapterMarksPayload) -> String {
    let mut out = String::new();
    for (n, mark) in payload.chapters.iter().enumerate() {
        // `SubRip` cue numbers are 1-based.
        let cue = n + 1;
        let start = format_srt_timestamp(mark.start_ms);
        let end = format_srt_timestamp(mark.end_ms);
        let title = sanitize_cue_text(&mark.title);
        let _ = write!(out, "{cue}\n{start} --> {end}\n{title}\n\n");
    }
    out
}

/// Render the payload as `WebVTT` cues.
///
/// Identical to SRT except: leading `WEBVTT` magic line + blank,
/// `.` decimal separator instead of `,`, and cue identifiers are
/// optional (we still emit them for parity with the SRT path).
fn render_vtt(payload: &ChapterMarksPayload) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for (n, mark) in payload.chapters.iter().enumerate() {
        let cue = n + 1;
        let start = format_vtt_timestamp(mark.start_ms);
        let end = format_vtt_timestamp(mark.end_ms);
        let title = sanitize_cue_text(&mark.title);
        let _ = write!(out, "{cue}\n{start} --> {end}\n{title}\n\n");
    }
    out
}

/// `HH:MM:SS,mmm` — `SubRip` canonical timestamp.
fn format_srt_timestamp(ms: i64) -> String {
    let (h, m, s, frac) = split_ms(ms);
    format!("{h:02}:{m:02}:{s:02},{frac:03}")
}

/// `HH:MM:SS.mmm` — `WebVTT` canonical timestamp.
fn format_vtt_timestamp(ms: i64) -> String {
    let (h, m, s, frac) = split_ms(ms);
    format!("{h:02}:{m:02}:{s:02}.{frac:03}")
}

/// Decompose a millisecond count into `(hours, minutes, seconds,
/// fractional_ms)`. Clamps negative inputs to zero — chapter
/// marks stored as `i64` can technically be negative on a corrupt
/// row, but emitting `-00:00:01,000` in a subtitle is worse than
/// silently flooring. `hours` saturates at `u32::MAX` for the
/// same reason: a corrupt 9_999_999-hour mark is better rendered
/// as a giant number than silently truncated.
fn split_ms(ms: i64) -> (u32, u32, u32, u32) {
    let ms: u64 = ms.max(0).try_into().unwrap_or(u64::MAX);
    let frac = u32::try_from(ms % 1000).unwrap_or(999);
    let total_secs = ms / 1000;
    let s = u32::try_from(total_secs % 60).unwrap_or(59);
    let total_mins = total_secs / 60;
    let m = u32::try_from(total_mins % 60).unwrap_or(59);
    let h = u32::try_from(total_mins / 60).unwrap_or(u32::MAX);
    (h, m, s, frac)
}

/// Strip newline characters from a cue body so a single chapter
/// title can't accidentally inject a cue separator. Audiobook
/// chapter titles are short single-line strings in practice; this
/// is a defence against a malformed `chapters.title` row.
fn sanitize_cue_text(raw: &str) -> String {
    raw.replace(['\r', '\n'], " ")
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use ab_transcript::chapter_marks_stage::ChapterMark;

    fn sample_payload() -> ChapterMarksPayload {
        ChapterMarksPayload {
            book_id: 7,
            total_duration_ms: 3_725_500,
            chapters: vec![
                ChapterMark {
                    idx: 0,
                    title: "Prologue".to_owned(),
                    start_ms: 0,
                    end_ms: 30_500,
                    start_char: 0,
                    end_char: 100,
                },
                ChapterMark {
                    idx: 1,
                    title: "Chapter 1: The Beginning".to_owned(),
                    start_ms: 30_500,
                    end_ms: 3_725_500,
                    start_char: 100,
                    end_char: 50_000,
                },
            ],
        }
    }

    #[test]
    fn srt_timestamp_zero_is_zero() {
        assert_eq!(format_srt_timestamp(0), "00:00:00,000");
    }

    #[test]
    fn srt_timestamp_carries_into_minutes_and_hours() {
        // 1h 2m 3s 456ms = 3_723_456 ms
        assert_eq!(format_srt_timestamp(3_723_456), "01:02:03,456");
    }

    #[test]
    fn vtt_timestamp_uses_period_separator() {
        assert_eq!(format_vtt_timestamp(3_723_456), "01:02:03.456");
    }

    #[test]
    fn negative_timestamp_floors_to_zero() {
        // Defensive — a corrupt cache row shouldn't produce a
        // negative HH:MM:SS in the output stream.
        assert_eq!(format_srt_timestamp(-500), "00:00:00,000");
    }

    #[test]
    fn srt_render_has_indexed_cues_with_comma_separator() {
        let body = render_srt(&sample_payload());
        let expected = "\
1
00:00:00,000 --> 00:00:30,500
Prologue

2
00:00:30,500 --> 01:02:05,500
Chapter 1: The Beginning

";
        assert_eq!(body, expected);
    }

    #[test]
    fn vtt_render_has_webvtt_header_and_period_separator() {
        let body = render_vtt(&sample_payload());
        let expected = "\
WEBVTT

1
00:00:00.000 --> 00:00:30.500
Prologue

2
00:00:30.500 --> 01:02:05.500
Chapter 1: The Beginning

";
        assert_eq!(body, expected);
    }

    #[test]
    fn empty_chapters_render_to_empty_srt_body() {
        let payload = ChapterMarksPayload {
            book_id: 1,
            total_duration_ms: 0,
            chapters: vec![],
        };
        assert_eq!(render_srt(&payload), "");
    }

    #[test]
    fn empty_chapters_render_to_header_only_vtt() {
        let payload = ChapterMarksPayload {
            book_id: 1,
            total_duration_ms: 0,
            chapters: vec![],
        };
        assert_eq!(render_vtt(&payload), "WEBVTT\n\n");
    }

    #[test]
    fn cue_text_strips_newlines_to_keep_cue_blocks_intact() {
        let body = render_srt(&ChapterMarksPayload {
            book_id: 1,
            total_duration_ms: 1_000,
            chapters: vec![ChapterMark {
                idx: 0,
                title: "Bad\nTitle\rWith\r\nLines".to_owned(),
                start_ms: 0,
                end_ms: 1_000,
                start_char: 0,
                end_char: 10,
            }],
        });
        // Title appears on a single line; the cue body line count
        // is exactly 1 (title) between timing line and the
        // trailing blank line.
        let cue_body_lines = body
            .lines()
            .skip(2) // cue number + timing
            .take_while(|l| !l.is_empty())
            .count();
        assert_eq!(cue_body_lines, 1, "title must collapse to one line");
        assert!(!body.contains('\r'));
    }
}
