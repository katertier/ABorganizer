//! `GET /api/v1/books/{book_id}/transcript.txt` — per-book
//! transcript export.
//!
//! Returns the cached transcript text written by the
//! `transcribe-*` pipeline stages. Default kind is the full
//! transcript (`cache_type = 'transcript_full'`); operator can
//! request a different cache slice via `?kind=`.
//!
//! ## Use cases
//!
//! * **Operator review** — read the text the audiologo / LLM
//!   extractors consume, useful for understanding why a book
//!   was tagged a certain way.
//! * **Export for downstream tooling** — pipe through `grep`,
//!   `aborg-tools`, or import into note systems.
//! * **Debugging missing-extract symptoms** — when DNA tags /
//!   summary / setting are missing, the transcript is the
//!   primary input; checking it directly often diagnoses the
//!   issue (silent regions, wrong-language detection, etc.).
//!
//! Returns `404 Not Found` when the book exists but the
//! requested transcript hasn't been produced yet — books that
//! arrived recently won't have `transcript_full` until the
//! Idle-priority full transcribe completes.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use serde::Deserialize;

use ab_core::CacheKey;

use crate::ApiError;
use crate::state::ApiState;

/// Query-string params for the transcript export endpoint.
#[derive(Debug, Deserialize, Default)]
pub struct TranscriptQuery {
    /// Which transcript slice to return. Defaults to `full`.
    ///
    /// Supported values map to [`CacheKey`] variants:
    ///
    /// * `full` → `TranscriptFull`
    /// * `head` → `TranscriptHead`
    /// * `tail` → `TranscriptTail`
    /// * `samples` → `TranscriptSamples`
    ///
    /// Unknown values produce `400 Bad Request`.
    #[serde(default)]
    pub kind: Option<String>,
}

/// `GET /api/v1/books/{book_id}/transcript.txt[?kind=…]`
///
/// Returns `200 OK` (`Content-Type: text/plain; charset=utf-8`)
/// with the raw transcript bytes. `404 Not Found` when the book
/// doesn't exist OR the requested transcript hasn't been
/// produced yet. `400 Bad Request` on an unknown `kind`.
///
/// # Errors
///
/// Database access failures surface as [`ApiError::Internal`].
#[allow(clippy::missing_panics_doc)] // panic-free
pub async fn books_transcript_get(
    State(state): State<ApiState>,
    Path(book_id): Path<i64>,
    Query(params): Query<TranscriptQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let kind = resolve_kind(params.kind.as_deref())?;
    let cache_type = kind.as_str();

    // 404 the book first so callers don't see a generic "no
    // transcript" when they really wanted "no book."
    let book_exists = sqlx::query_scalar!(
        r#"SELECT 1 AS "n!: i64" FROM books WHERE book_id = ?"#,
        book_id,
    )
    .fetch_optional(state.inner.library.pool())
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "transcript book lookup: {e}"
        )))
    })?
    .is_some();

    if !book_exists {
        return Err(ApiError::NotFound(format!("book {book_id}")));
    }

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
            "transcript content lookup: {e}"
        )))
    })?;

    let Some(content) = content else {
        return Err(ApiError::NotFound(format!(
            "book {book_id} has no {cache_type} cached yet"
        )));
    };

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    // Save-As hint — short suffix encodes the kind so the
    // operator's downloads stay distinguishable when they pull
    // multiple slices for the same book.
    let suffix = match kind {
        CacheKey::TranscriptHead => "head",
        CacheKey::TranscriptTail => "tail",
        CacheKey::TranscriptSamples => "samples",
        _ => "full",
    };
    if let Ok(v) = HeaderValue::from_str(&format!(
        "attachment; filename=\"book-{book_id}-transcript-{suffix}.txt\""
    )) {
        headers.insert(header::CONTENT_DISPOSITION, v);
    }

    Ok((StatusCode::OK, headers, content))
}

/// Map the `?kind=` string to a [`CacheKey`]. Defaults to
/// `TranscriptFull` when the param is absent. Returns
/// [`ApiError::BadRequest`] for an unknown value so callers
/// catch typos at the API boundary instead of getting a
/// confusing 404.
fn resolve_kind(raw: Option<&str>) -> Result<CacheKey, ApiError> {
    match raw.unwrap_or("full") {
        "full" => Ok(CacheKey::TranscriptFull),
        "head" => Ok(CacheKey::TranscriptHead),
        "tail" => Ok(CacheKey::TranscriptTail),
        "samples" => Ok(CacheKey::TranscriptSamples),
        other => Err(ApiError::BadRequest(format!(
            "unknown transcript kind {other:?}; expected one of full / head / tail / samples"
        ))),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn resolve_kind_defaults_to_full() {
        assert_eq!(resolve_kind(None).unwrap(), CacheKey::TranscriptFull);
    }

    #[test]
    fn resolve_kind_accepts_each_documented_slice() {
        assert_eq!(
            resolve_kind(Some("full")).unwrap(),
            CacheKey::TranscriptFull
        );
        assert_eq!(
            resolve_kind(Some("head")).unwrap(),
            CacheKey::TranscriptHead
        );
        assert_eq!(
            resolve_kind(Some("tail")).unwrap(),
            CacheKey::TranscriptTail
        );
        assert_eq!(
            resolve_kind(Some("samples")).unwrap(),
            CacheKey::TranscriptSamples
        );
    }

    #[test]
    fn resolve_kind_rejects_unknown_with_400() {
        match resolve_kind(Some("gibberish")) {
            Err(ApiError::BadRequest(msg)) => {
                assert!(msg.contains("gibberish"));
                assert!(msg.contains("full"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn resolve_kind_rejects_empty_string() {
        // Empty string is NOT the same as absent — the operator
        // passed `?kind=` deliberately; treat as a 400 so they
        // catch the typo.
        match resolve_kind(Some("")) {
            Err(ApiError::BadRequest(_)) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }
}
