//! `GET /api/v1/search?q=...` — cross-entity fuzzy search.
//!
//! Returns books / authors / narrators / series matching the
//! query, grouped per entity in the response shape so clients can
//! render them in separate UI sections.
//!
//! ## Backend per entity
//!
//! All four entities now use FTS5 (migration 030 for `books_fts`,
//! migration 040 for `authors_fts` / `narrators_fts` /
//! `series_fts`). Tokenizer `unicode61 remove_diacritics 2`;
//! ranking via `bm25(<fts>)`; snippet via the `snippet()` function
//! with `[match]…[/match]` delimiters.
//!
//! * **Books** index `title` + `subtitle` + `description`.
//! * **Authors / narrators / series** index `name` only. Bio (where
//!   present) is enrichment-late + often empty; `franchise_prefix`
//!   on series is a sort affix, not a search target. Both can be
//!   added later by rebuilding the FTS table — non-breaking.
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
    /// `true` when this row came from the trigram fallback
    /// (typo recovery via `books_trigram`); `false` for the
    /// primary `unicode61` path. Clients can render
    /// fuzzy rows under a "did you mean" header.
    pub fuzzy: bool,
}

/// One author/narrator/series result. Same compact shape across
/// all three entity buckets so clients can render generically.
#[derive(Debug, Serialize)]
pub struct EntitySearchHit {
    pub id: i64,
    pub name: String,
    pub book_count: i64,
    /// Snippet from the matched column with `[match]…[/match]`
    /// markers. For entity names that are 1-3 words this is
    /// mostly cosmetic ("Brandon [match]Sand[/match]erson"), but
    /// surfaces consistency with [`BookSearchHit::snippet`] so
    /// clients can render all four entity buckets with the same
    /// snippet-aware widget. `None` when FTS5 returned an empty
    /// snippet.
    pub snippet: Option<String>,
    /// `true` when this row came from the trigram fallback;
    /// `false` for the primary `unicode61` path. See
    /// [`BookSearchHit::fuzzy`].
    pub fuzzy: bool,
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

/// Sanitize operator input before it reaches FTS5.
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

/// Build the FTS5 MATCH expression for a trigram-tokenized index.
///
/// The trigram tokenizer treats the query as one phrase and
/// decomposes it into overlapping 3-grams. Wrapping in double
/// quotes keeps the whole sanitized query as a single phrase so
/// `"mistbron"` matches rows whose trigram set overlaps —
/// `"Mistborn"` shares 5 of 6 trigrams. No prefix-suffix needed;
/// the trigram tokenizer's nature is fuzzy.
fn build_trigram_match(sanitized: &str) -> String {
    format!("\"{sanitized}\"")
}

/// Minimum query length to bother with the trigram fallback.
///
/// Below 4 characters, the operator's input has at most 2 trigrams
/// (a 3-char query produces 1 trigram; a 4-char query produces 2).
/// Single-trigram matches are noisy — most rows in any non-trivial
/// catalog contain `"the"`, `"and"`, etc.
const TRIGRAM_MIN_LEN: usize = 4;

/// `GET /api/v1/search?q=<text>[&limit=&offset=]`
///
/// Returns `200 OK` with [`SearchResponse`] JSON. Empty query
/// (absent / whitespace-only / all-stripped) returns the empty
/// response shape — no 400.
///
/// # Errors
///
/// Database access failures surface as [`ApiError::Internal`].
#[allow(
    clippy::missing_panics_doc,
    clippy::too_many_lines,
    // Each entity follows a "fetch primary results; if empty,
    // fetch fuzzy fallback" pattern that's easier to read as
    // `let mut hits = primary; if hits.is_empty() { hits =
    // fuzzy; }` than as a one-shot `let hits = if … { fuzzy }
    // else { primary };` that hides the fact that the fuzzy
    // branch only fires conditionally. The nursery lint
    // suggests the latter; we override.
    clippy::useless_let_if_seq
)]
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
    let trigram_match = build_trigram_match(&query);
    let trigram_eligible = query.len() >= TRIGRAM_MIN_LEN;

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

    let mut book_hits: Vec<BookSearchHit> = book_rows
        .into_iter()
        .map(|r| BookSearchHit {
            book_id: r.book_id,
            title: r.title,
            subtitle: r.subtitle,
            snippet: r.snippet,
            fuzzy: false,
        })
        .collect();
    let mut books_total = books_total;

    // Trigram fallback for typo recovery — only fires when the
    // primary unicode61 path returned zero hits and the query is
    // long enough for trigrams to mean something.
    if book_hits.is_empty() && trigram_eligible {
        let fuzzy_rows = sqlx::query!(
            r#"SELECT b.book_id AS "book_id!: i64",
                      b.title AS "title!: String",
                      b.subtitle AS "subtitle?: String",
                      snippet(books_trigram, -1, '[match]', '[/match]', '…', 16)
                          AS "snippet?: String"
                 FROM books_trigram
                 JOIN books b ON b.book_id = books_trigram.rowid
                WHERE books_trigram MATCH ?1
                ORDER BY bm25(books_trigram), b.title COLLATE NOCASE
                LIMIT ?2 OFFSET ?3"#,
            trigram_match,
            limit,
            offset,
        )
        .fetch_all(pool)
        .await
        .map_err(|e| {
            ApiError::Internal(ab_core::Error::Database(format!(
                "books trigram search: {e}"
            )))
        })?;
        let fuzzy_total: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64"
                 FROM books_trigram WHERE books_trigram MATCH ?1"#,
            trigram_match,
        )
        .fetch_one(pool)
        .await
        .map_err(|e| {
            ApiError::Internal(ab_core::Error::Database(format!(
                "books trigram count: {e}"
            )))
        })?;
        book_hits = fuzzy_rows
            .into_iter()
            .map(|r| BookSearchHit {
                book_id: r.book_id,
                title: r.title,
                subtitle: r.subtitle,
                snippet: r.snippet,
                fuzzy: true,
            })
            .collect();
        books_total = fuzzy_total;
    }

    // ── Authors via FTS5 ──────────────────────────────────────
    let author_rows = sqlx::query!(
        r#"SELECT a.author_id AS "author_id!: i64",
                  a.name AS "name!: String",
                  (SELECT COUNT(*) FROM books WHERE author_id = a.author_id)
                      AS "book_count!: i64",
                  snippet(authors_fts, -1, '[match]', '[/match]', '…', 16)
                      AS "snippet?: String"
             FROM authors_fts
             JOIN authors a ON a.author_id = authors_fts.rowid
            WHERE authors_fts MATCH ?1
            ORDER BY bm25(authors_fts), a.name COLLATE NOCASE
            LIMIT ?2 OFFSET ?3"#,
        fts_match,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("authors search: {e}"))))?;

    let authors_total: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!: i64"
             FROM authors_fts WHERE authors_fts MATCH ?1"#,
        fts_match,
    )
    .fetch_one(pool)
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "authors search count: {e}"
        )))
    })?;

    let mut author_hits: Vec<EntitySearchHit> = author_rows
        .into_iter()
        .map(|r| EntitySearchHit {
            id: r.author_id,
            name: r.name,
            book_count: r.book_count,
            snippet: r.snippet,
            fuzzy: false,
        })
        .collect();
    let mut authors_total = authors_total;

    if author_hits.is_empty() && trigram_eligible {
        let fuzzy_rows = sqlx::query!(
            r#"SELECT a.author_id AS "author_id!: i64",
                      a.name AS "name!: String",
                      (SELECT COUNT(*) FROM books WHERE author_id = a.author_id)
                          AS "book_count!: i64",
                      snippet(authors_trigram, -1, '[match]', '[/match]', '…', 16)
                          AS "snippet?: String"
                 FROM authors_trigram
                 JOIN authors a ON a.author_id = authors_trigram.rowid
                WHERE authors_trigram MATCH ?1
                ORDER BY bm25(authors_trigram), a.name COLLATE NOCASE
                LIMIT ?2 OFFSET ?3"#,
            trigram_match,
            limit,
            offset,
        )
        .fetch_all(pool)
        .await
        .map_err(|e| {
            ApiError::Internal(ab_core::Error::Database(format!(
                "authors trigram search: {e}"
            )))
        })?;
        let fuzzy_total: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64"
                 FROM authors_trigram WHERE authors_trigram MATCH ?1"#,
            trigram_match,
        )
        .fetch_one(pool)
        .await
        .map_err(|e| {
            ApiError::Internal(ab_core::Error::Database(format!(
                "authors trigram count: {e}"
            )))
        })?;
        author_hits = fuzzy_rows
            .into_iter()
            .map(|r| EntitySearchHit {
                id: r.author_id,
                name: r.name,
                book_count: r.book_count,
                snippet: r.snippet,
                fuzzy: true,
            })
            .collect();
        authors_total = fuzzy_total;
    }

    // ── Narrators via FTS5 ────────────────────────────────────
    let narrator_rows = sqlx::query!(
        r#"SELECT n.narrator_id AS "narrator_id!: i64",
                  n.name AS "name!: String",
                  (SELECT COUNT(*) FROM book_narrator WHERE narrator_id = n.narrator_id)
                      AS "book_count!: i64",
                  snippet(narrators_fts, -1, '[match]', '[/match]', '…', 16)
                      AS "snippet?: String"
             FROM narrators_fts
             JOIN narrators n ON n.narrator_id = narrators_fts.rowid
            WHERE narrators_fts MATCH ?1
            ORDER BY bm25(narrators_fts), n.name COLLATE NOCASE
            LIMIT ?2 OFFSET ?3"#,
        fts_match,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("narrators search: {e}"))))?;

    let narrators_total: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!: i64"
             FROM narrators_fts WHERE narrators_fts MATCH ?1"#,
        fts_match,
    )
    .fetch_one(pool)
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "narrators search count: {e}"
        )))
    })?;

    let mut narrator_hits: Vec<EntitySearchHit> = narrator_rows
        .into_iter()
        .map(|r| EntitySearchHit {
            id: r.narrator_id,
            name: r.name,
            book_count: r.book_count,
            snippet: r.snippet,
            fuzzy: false,
        })
        .collect();
    let mut narrators_total = narrators_total;

    if narrator_hits.is_empty() && trigram_eligible {
        let fuzzy_rows = sqlx::query!(
            r#"SELECT n.narrator_id AS "narrator_id!: i64",
                      n.name AS "name!: String",
                      (SELECT COUNT(*) FROM book_narrator WHERE narrator_id = n.narrator_id)
                          AS "book_count!: i64",
                      snippet(narrators_trigram, -1, '[match]', '[/match]', '…', 16)
                          AS "snippet?: String"
                 FROM narrators_trigram
                 JOIN narrators n ON n.narrator_id = narrators_trigram.rowid
                WHERE narrators_trigram MATCH ?1
                ORDER BY bm25(narrators_trigram), n.name COLLATE NOCASE
                LIMIT ?2 OFFSET ?3"#,
            trigram_match,
            limit,
            offset,
        )
        .fetch_all(pool)
        .await
        .map_err(|e| {
            ApiError::Internal(ab_core::Error::Database(format!(
                "narrators trigram search: {e}"
            )))
        })?;
        let fuzzy_total: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64"
                 FROM narrators_trigram WHERE narrators_trigram MATCH ?1"#,
            trigram_match,
        )
        .fetch_one(pool)
        .await
        .map_err(|e| {
            ApiError::Internal(ab_core::Error::Database(format!(
                "narrators trigram count: {e}"
            )))
        })?;
        narrator_hits = fuzzy_rows
            .into_iter()
            .map(|r| EntitySearchHit {
                id: r.narrator_id,
                name: r.name,
                book_count: r.book_count,
                snippet: r.snippet,
                fuzzy: true,
            })
            .collect();
        narrators_total = fuzzy_total;
    }

    // ── Series via FTS5 ───────────────────────────────────────
    let series_rows = sqlx::query!(
        r#"SELECT s.series_id AS "series_id!: i64",
                  s.name AS "name!: String",
                  (SELECT COUNT(*) FROM book_series WHERE series_id = s.series_id)
                      AS "book_count!: i64",
                  snippet(series_fts, -1, '[match]', '[/match]', '…', 16)
                      AS "snippet?: String"
             FROM series_fts
             JOIN series s ON s.series_id = series_fts.rowid
            WHERE series_fts MATCH ?1
            ORDER BY bm25(series_fts), s.name COLLATE NOCASE
            LIMIT ?2 OFFSET ?3"#,
        fts_match,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("series search: {e}"))))?;

    let series_total: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!: i64"
             FROM series_fts WHERE series_fts MATCH ?1"#,
        fts_match,
    )
    .fetch_one(pool)
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "series search count: {e}"
        )))
    })?;

    let mut series_hits: Vec<EntitySearchHit> = series_rows
        .into_iter()
        .map(|r| EntitySearchHit {
            id: r.series_id,
            name: r.name,
            book_count: r.book_count,
            snippet: r.snippet,
            fuzzy: false,
        })
        .collect();
    let mut series_total = series_total;

    if series_hits.is_empty() && trigram_eligible {
        let fuzzy_rows = sqlx::query!(
            r#"SELECT s.series_id AS "series_id!: i64",
                      s.name AS "name!: String",
                      (SELECT COUNT(*) FROM book_series WHERE series_id = s.series_id)
                          AS "book_count!: i64",
                      snippet(series_trigram, -1, '[match]', '[/match]', '…', 16)
                          AS "snippet?: String"
                 FROM series_trigram
                 JOIN series s ON s.series_id = series_trigram.rowid
                WHERE series_trigram MATCH ?1
                ORDER BY bm25(series_trigram), s.name COLLATE NOCASE
                LIMIT ?2 OFFSET ?3"#,
            trigram_match,
            limit,
            offset,
        )
        .fetch_all(pool)
        .await
        .map_err(|e| {
            ApiError::Internal(ab_core::Error::Database(format!(
                "series trigram search: {e}"
            )))
        })?;
        let fuzzy_total: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64"
                 FROM series_trigram WHERE series_trigram MATCH ?1"#,
            trigram_match,
        )
        .fetch_one(pool)
        .await
        .map_err(|e| {
            ApiError::Internal(ab_core::Error::Database(format!(
                "series trigram count: {e}"
            )))
        })?;
        series_hits = fuzzy_rows
            .into_iter()
            .map(|r| EntitySearchHit {
                id: r.series_id,
                name: r.name,
                book_count: r.book_count,
                snippet: r.snippet,
                fuzzy: true,
            })
            .collect();
        series_total = fuzzy_total;
    }

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
    fn trigram_match_wraps_in_double_quotes_as_one_phrase() {
        // Trigram tokenizer takes the whole quoted phrase and
        // decomposes it into 3-grams internally; no per-word
        // wrapping needed (unlike build_fts_match for unicode61).
        assert_eq!(build_trigram_match("mistbron"), r#""mistbron""#);
        assert_eq!(build_trigram_match("hello world"), r#""hello world""#);
    }

    #[test]
    fn trigram_min_len_is_four_chars() {
        // At 3 chars input → 1 trigram (the whole word), noisy.
        // At 4 chars → 2 trigrams (overlapping), meaningfully
        // distinguishes "mist" from "best".
        assert_eq!(TRIGRAM_MIN_LEN, 4);
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
                    fuzzy: false,
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
                    snippet: Some("[match]Mistbo[/match]rn".into()),
                    fuzzy: false,
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
            snippet: None,
            fuzzy: false,
        };
        let json = serde_json::to_value(&hit).unwrap();
        assert_eq!(json["id"], 42);
        assert!(json.get("author_id").is_none());
        assert!(json.get("narrator_id").is_none());
        // Snippet present even when null — JSON shape stable.
        assert!(json.get("snippet").is_some());
        assert!(json["snippet"].is_null());
        // Fuzzy flag is present, defaulting to false for primary
        // unicode61 hits.
        assert_eq!(json["fuzzy"], false);
    }

    #[test]
    fn fuzzy_flag_distinguishes_trigram_hits_in_json() {
        let exact = BookSearchHit {
            book_id: 1,
            title: "Mistborn".into(),
            subtitle: None,
            snippet: None,
            fuzzy: false,
        };
        let fuzzy = BookSearchHit {
            book_id: 2,
            title: "Mistborn".into(),
            subtitle: None,
            snippet: None,
            fuzzy: true,
        };
        let exact_json = serde_json::to_value(&exact).unwrap();
        let fuzzy_json = serde_json::to_value(&fuzzy).unwrap();
        assert_eq!(exact_json["fuzzy"], false);
        assert_eq!(fuzzy_json["fuzzy"], true);
    }
}
