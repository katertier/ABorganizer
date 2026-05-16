//! `GET /api/v1/search?q=...` — cross-entity fuzzy search.
//!
//! Returns books / authors / narrators / series matching the
//! query, grouped per entity in the response shape so clients can
//! render them in separate UI sections.
//!
//! ## Backend per entity
//!
//! * **Books** — FTS5 via `books_fts` (migration 030; ADR-0036).
//!   Tokenizer `unicode61 remove_diacritics 2`; ranking via
//!   `bm25(books_fts)`; snippet via the `snippet()` function with
//!   `[match]`/`[/match]` delimiters on the matched column.
//!   Backfilled at migration time so existing rows are
//!   immediately searchable.
//! * **Authors / narrators / series** — plain `LIKE %q% COLLATE
//!   NOCASE` against the `name` column. FTS5 mirrors for these
//!   tables haven't shipped yet (follow-up to ADR-0036); the LIKE
//!   path is good enough for the typical "I know roughly how it's
//!   spelled" search and avoids a dummy stub that would lock the
//!   API shape to a non-FTS surface.
//!
//! When the future authors/narrators/series FTS lands, the SQL
//! behind each entity bucket swaps to FTS5 with snippet — the
//! public JSON shape stays compatible (snippet is already an
//! `Option<String>`).
//!
//! ## Query syntax
//!
//! Operator input is **sanitized** at the boundary: anything that
//! isn't alphanumeric, whitespace, hyphen, or apostrophe is
//! stripped before reaching FTS5 (avoids the operator
//! accidentally invoking `MATCH` operators like `NEAR` or column
//! filters). Each word is suffixed with `*` for prefix match so
//! `mistbo` finds `Mistborn`. Empty / all-stripped queries return
//! an empty response immediately (no 400, no work).

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, response::Response};
use serde::{Deserialize, Serialize};

use crate::ApiError;
use crate::pagination::{clamp_limit, clamp_offset};
use crate::state::ApiState;

/// One book result.
#[derive(Debug, Serialize)]
pub struct BookSearchHit {
    pub book_id: i64,
    pub title: String,
    pub subtitle: Option<String>,
    /// Snippet from the matched column with `[match]…[/match]`
    /// markers on the hit tokens. FTS5's `snippet()` picks
    /// whichever column produced the best score. `None` if FTS5
    /// returned an empty snippet (rare; happens when only a
    /// column header matched without surrounding context).
    pub snippet: Option<String>,
}

/// One author/narrator/series result. Same compact shape across
/// all three entity buckets so clients can render generically.
#[derive(Debug, Serialize)]
pub struct EntitySearchHit {
    pub id: i64,
    pub name: String,
    pub book_count: i64,
}

/// Per-entity result bucket.
#[derive(Debug, Serialize)]
pub struct SearchBucket<T> {
    pub hits: Vec<T>,
    /// Total matches across the whole library, NOT clamped by
    /// `limit`. Lets clients render "showing 10 of 47" without
    /// a second call.
    pub total: i64,
}

/// Response body for `GET /api/v1/search`.
#[derive(Debug, Serialize)]
pub struct SearchResponse {
    /// The query the server actually ran (post-sanitization).
    /// Differs from the operator's input when special characters
    /// were stripped; useful for "no results for X" debugging.
    pub query: String,
    pub books: SearchBucket<BookSearchHit>,
    pub authors: SearchBucket<EntitySearchHit>,
    pub narrators: SearchBucket<EntitySearchHit>,
    pub series: SearchBucket<EntitySearchHit>,
}

/// Query-string params.
#[derive(Debug, Deserialize, Default)]
pub struct SearchQuery {
    /// The search text.
    #[serde(default)]
    pub q: Option<String>,
    /// Per-entity hit cap. Defaults to
    /// [`crate::pagination::DEFAULT_LIMIT`] (50); clamped via the
    /// shared helper to [`crate::pagination::MAX_LIMIT`] (200).
    /// Negative / zero → 1.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Per-entity offset. Useful for "show more" pagination
    /// within a single entity bucket while the others stay on
    /// page 1. Default 0; negatives → 0.
    #[serde(default)]
    pub offset: Option<i64>,
}

/// Sanitize operator input for both FTS5 and LIKE backends.
///
/// Keeps alphanumerics + whitespace + hyphen + apostrophe. Trims
/// whitespace, collapses runs of whitespace to one space. Returns
/// `None` when nothing survives — clients see an empty-result
/// response, no 400.
fn sanitize_query(raw: &str) -> Option<String> {
    let kept: String = raw
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '\'' {
                c
            } else {
                ' '
            }
        })
        .collect();
    let trimmed = kept.split_whitespace().collect::<Vec<_>>().join(" ");
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Build the FTS5 MATCH expression from a sanitized query.
///
/// Each whitespace-separated token gets a `*` prefix-suffix for
/// "starts with" matching; tokens are joined with implicit AND
/// (FTS5's default). Empty input is the caller's responsibility
/// (use [`sanitize_query`] first).
fn build_fts_match(sanitized: &str) -> String {
    sanitized
        .split_whitespace()
        .map(|word| format!("\"{word}\"*"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Build the LIKE pattern from a sanitized query — wraps the
/// whole query in `%…%` for substring match.
fn build_like_pattern(sanitized: &str) -> String {
    format!("%{sanitized}%")
}

/// `GET /api/v1/search?q=<text>[&limit=&offset=]`
///
/// Returns `200 OK` with [`SearchResponse`] JSON. Empty query
/// (absent / whitespace-only / all-stripped) returns the empty
/// response shape — no 400.
///
/// # Errors
///
/// Database access failures surface as [`ApiError::Internal`].
#[allow(clippy::missing_panics_doc, clippy::too_many_lines)]
pub async fn search(
    State(state): State<ApiState>,
    Query(params): Query<SearchQuery>,
) -> Result<Response, ApiError> {
    let limit = clamp_limit(params.limit);
    let offset = clamp_offset(params.offset);

    let Some(query) = params.q.as_deref().and_then(sanitize_query) else {
        return Ok((
            StatusCode::OK,
            Json(SearchResponse {
                query: String::new(),
                books: SearchBucket {
                    hits: Vec::new(),
                    total: 0,
                },
                authors: SearchBucket {
                    hits: Vec::new(),
                    total: 0,
                },
                narrators: SearchBucket {
                    hits: Vec::new(),
                    total: 0,
                },
                series: SearchBucket {
                    hits: Vec::new(),
                    total: 0,
                },
            }),
        )
            .into_response());
    };

    let pool = state.inner.library.pool();
    let fts_match = build_fts_match(&query);
    let like_pattern = build_like_pattern(&query);

    // ── Books via FTS5 ────────────────────────────────────────
    let book_rows = sqlx::query!(
        r#"SELECT b.book_id AS "book_id!: i64",
                  b.title AS "title!: String",
                  b.subtitle AS "subtitle?: String",
                  snippet(books_fts, -1, '[match]', '[/match]', '…', 16)
                      AS "snippet?: String"
             FROM books_fts
             JOIN books b ON b.book_id = books_fts.rowid
            WHERE books_fts MATCH ?1
            ORDER BY bm25(books_fts), b.title COLLATE NOCASE
            LIMIT ?2 OFFSET ?3"#,
        fts_match,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("books search: {e}"))))?;

    let books_total: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!: i64"
             FROM books_fts
            WHERE books_fts MATCH ?1"#,
        fts_match,
    )
    .fetch_one(pool)
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!("books search count: {e}")))
    })?;

    let book_hits: Vec<BookSearchHit> = book_rows
        .into_iter()
        .map(|r| BookSearchHit {
            book_id: r.book_id,
            title: r.title,
            subtitle: r.subtitle,
            snippet: r.snippet,
        })
        .collect();

    // ── Authors via LIKE (FTS5 mirror is a follow-up slice) ───
    let author_rows = sqlx::query!(
        r#"SELECT a.author_id AS "author_id!: i64",
                  a.name AS "name!: String",
                  (SELECT COUNT(*) FROM books WHERE author_id = a.author_id)
                      AS "book_count!: i64"
             FROM authors a
            WHERE a.name LIKE ?1 COLLATE NOCASE
            ORDER BY a.name COLLATE NOCASE
            LIMIT ?2 OFFSET ?3"#,
        like_pattern,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("authors search: {e}"))))?;

    let authors_total: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!: i64"
             FROM authors WHERE name LIKE ?1 COLLATE NOCASE"#,
        like_pattern,
    )
    .fetch_one(pool)
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "authors search count: {e}"
        )))
    })?;

    let author_hits: Vec<EntitySearchHit> = author_rows
        .into_iter()
        .map(|r| EntitySearchHit {
            id: r.author_id,
            name: r.name,
            book_count: r.book_count,
        })
        .collect();

    // ── Narrators via LIKE ────────────────────────────────────
    let narrator_rows = sqlx::query!(
        r#"SELECT n.narrator_id AS "narrator_id!: i64",
                  n.name AS "name!: String",
                  (SELECT COUNT(*) FROM book_narrator WHERE narrator_id = n.narrator_id)
                      AS "book_count!: i64"
             FROM narrators n
            WHERE n.name LIKE ?1 COLLATE NOCASE
            ORDER BY n.name COLLATE NOCASE
            LIMIT ?2 OFFSET ?3"#,
        like_pattern,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("narrators search: {e}"))))?;

    let narrators_total: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!: i64"
             FROM narrators WHERE name LIKE ?1 COLLATE NOCASE"#,
        like_pattern,
    )
    .fetch_one(pool)
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "narrators search count: {e}"
        )))
    })?;

    let narrator_hits: Vec<EntitySearchHit> = narrator_rows
        .into_iter()
        .map(|r| EntitySearchHit {
            id: r.narrator_id,
            name: r.name,
            book_count: r.book_count,
        })
        .collect();

    // ── Series via LIKE ───────────────────────────────────────
    let series_rows = sqlx::query!(
        r#"SELECT s.series_id AS "series_id!: i64",
                  s.name AS "name!: String",
                  (SELECT COUNT(*) FROM book_series WHERE series_id = s.series_id)
                      AS "book_count!: i64"
             FROM series s
            WHERE s.name LIKE ?1 COLLATE NOCASE
            ORDER BY s.name COLLATE NOCASE
            LIMIT ?2 OFFSET ?3"#,
        like_pattern,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("series search: {e}"))))?;

    let series_total: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!: i64"
             FROM series WHERE name LIKE ?1 COLLATE NOCASE"#,
        like_pattern,
    )
    .fetch_one(pool)
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "series search count: {e}"
        )))
    })?;

    let series_hits: Vec<EntitySearchHit> = series_rows
        .into_iter()
        .map(|r| EntitySearchHit {
            id: r.series_id,
            name: r.name,
            book_count: r.book_count,
        })
        .collect();

    Ok((
        StatusCode::OK,
        Json(SearchResponse {
            query,
            books: SearchBucket {
                hits: book_hits,
                total: books_total,
            },
            authors: SearchBucket {
                hits: author_hits,
                total: authors_total,
            },
            narrators: SearchBucket {
                hits: narrator_hits,
                total: narrators_total,
            },
            series: SearchBucket {
                hits: series_hits,
                total: series_total,
            },
        }),
    )
        .into_response())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_special_chars() {
        assert_eq!(sanitize_query("hello").as_deref(), Some("hello"));
        assert_eq!(
            sanitize_query("hello world").as_deref(),
            Some("hello world")
        );
        // Punctuation becomes whitespace, then collapses.
        assert_eq!(
            sanitize_query("hello, world!").as_deref(),
            Some("hello world")
        );
        // FTS5 operators get stripped — the operator can't smuggle
        // a MATCH expression in via the q param.
        assert_eq!(
            sanitize_query("foo NEAR bar").as_deref(),
            Some("foo NEAR bar"),
            "alphanumeric-only tokens including 'NEAR' survive — but \
             they're treated as plain words by build_fts_match, which \
             wraps them in quotes"
        );
        assert_eq!(
            sanitize_query("foo: bar = baz").as_deref(),
            Some("foo bar baz")
        );
        // Quoted operators get stripped too.
        assert_eq!(
            sanitize_query("\"phrase search\"").as_deref(),
            Some("phrase search")
        );
        // Hyphen + apostrophe preserved (common in names).
        assert_eq!(
            sanitize_query("O'Brien-Smith").as_deref(),
            Some("O'Brien-Smith")
        );
    }

    #[test]
    fn sanitize_returns_none_for_empty_or_all_stripped() {
        assert!(sanitize_query("").is_none());
        assert!(sanitize_query("   ").is_none());
        // Only special chars → all become whitespace → empty.
        assert!(sanitize_query(",.!@#$").is_none());
    }

    #[test]
    fn fts_match_wraps_each_word_with_prefix() {
        assert_eq!(build_fts_match("hello"), r#""hello"*"#);
        assert_eq!(build_fts_match("hello world"), r#""hello"* "world"*"#);
        assert_eq!(
            build_fts_match("the lord of rings"),
            r#""the"* "lord"* "of"* "rings"*"#
        );
    }

    #[test]
    fn like_pattern_wraps_in_percent() {
        assert_eq!(build_like_pattern("hello"), "%hello%");
        assert_eq!(build_like_pattern("foo bar"), "%foo bar%");
    }

    #[test]
    fn search_response_serializes_with_per_entity_buckets() {
        let resp = SearchResponse {
            query: "mistbo".into(),
            books: SearchBucket {
                hits: vec![BookSearchHit {
                    book_id: 1,
                    title: "Mistborn: The Final Empire".into(),
                    subtitle: Some("Mistborn book 1".into()),
                    snippet: Some("[match]Mistbo[/match]rn: The Final Empire".into()),
                }],
                total: 7,
            },
            authors: SearchBucket {
                hits: Vec::new(),
                total: 0,
            },
            narrators: SearchBucket {
                hits: Vec::new(),
                total: 0,
            },
            series: SearchBucket {
                hits: vec![EntitySearchHit {
                    id: 3,
                    name: "Mistborn".into(),
                    book_count: 7,
                }],
                total: 1,
            },
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["query"], "mistbo");
        assert_eq!(json["books"]["total"], 7);
        assert_eq!(json["books"]["hits"][0]["book_id"], 1);
        assert!(
            json["books"]["hits"][0]["snippet"]
                .as_str()
                .expect("snippet str")
                .contains("[match]")
        );
        assert_eq!(json["authors"]["total"], 0);
        assert!(json["authors"]["hits"].as_array().unwrap().is_empty());
        assert_eq!(json["series"]["hits"][0]["id"], 3);
        assert_eq!(json["series"]["hits"][0]["book_count"], 7);
    }

    #[test]
    fn entity_hit_uses_generic_id_field_not_per_entity() {
        // The generic `id` field on EntitySearchHit lets clients
        // render authors/narrators/series with one rendering
        // function. The book entry uses `book_id` (different
        // shape — books have title + subtitle + snippet, the
        // others don't).
        let hit = EntitySearchHit {
            id: 42,
            name: "Brandon Sanderson".into(),
            book_count: 30,
        };
        let json = serde_json::to_value(&hit).unwrap();
        assert_eq!(json["id"], 42);
        assert!(json.get("author_id").is_none());
        assert!(json.get("narrator_id").is_none());
    }
}
