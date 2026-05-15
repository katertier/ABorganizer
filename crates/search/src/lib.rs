//! FTS5 search over books (ADR-0036, slice B.16).
//!
//! Wraps the `books_fts` virtual table seeded by migration 030.
//! Tokenizer is `unicode61 remove_diacritics 2`, so search
//! against "café" and "cafe" both hit the same row.
//!
//! Slice B.16 ships the exact-FTS path. Trigram fuzzy matching
//! (operator-typo recovery) joins via its own adjacent table in
//! a follow-up slice; the [`search`] surface here returns
//! [`SearchHit::Fts`] only.

#![forbid(unsafe_code)]
#![allow(missing_docs)] // scaffold; tightened in follow-up slices

use serde::Serialize;
use sqlx::SqlitePool;

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub book_id: i64,
    pub title: String,
    /// FTS5 `rank` (0 = best). Lower scores beat higher scores.
    pub rank: f64,
    pub source: HitSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HitSource {
    Fts,
    Trigram,
}

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("empty query")]
    Empty,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

pub const DEFAULT_LIMIT: u32 = 50;
pub const MAX_LIMIT: u32 = 500;

/// Sanitise an operator-typed query into an FTS5 MATCH string.
///
/// FTS5 MATCH treats `"`, `-`, `:`, `^`, `(`, `)`, `*` as syntax.
/// The dispatcher and the web search bar emit free-form text; we
/// wrap each token in double quotes (FTS5 phrase quoting) so a
/// literal `kings?` stays a string-search rather than a syntax
/// error.
#[must_use]
pub fn build_match_expression(input: &str) -> Option<String> {
    let cleaned: Vec<String> = input.split_whitespace().filter_map(escape_token).collect();
    if cleaned.is_empty() {
        return None;
    }
    Some(cleaned.join(" "))
}

fn escape_token(tok: &str) -> Option<String> {
    let inner: String = tok.chars().filter(|c| *c != '"').collect();
    if inner.is_empty() {
        return None;
    }
    Some(format!("\"{inner}\""))
}

/// Run an FTS5 search across the `books_fts` mirror. Returns hits
/// ordered by rank (best first) capped at `limit`.
pub async fn search(
    pool: &SqlitePool,
    query: &str,
    limit: u32,
) -> Result<Vec<SearchHit>, SearchError> {
    let match_expr = build_match_expression(query).ok_or(SearchError::Empty)?;
    let limit_capped = limit.clamp(1, MAX_LIMIT);
    let limit_i64 = i64::from(limit_capped);

    let rows = sqlx::query!(
        r#"SELECT
            f.rowid AS "book_id!: i64",
            b.title AS "title!: String",
            f.rank  AS "rank!: f64"
         FROM books_fts f
         INNER JOIN books b ON b.book_id = f.rowid
         WHERE f.books_fts MATCH ?
           AND b.deleted_at IS NULL
         ORDER BY f.rank
         LIMIT ?"#,
        match_expr,
        limit_i64,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| SearchHit {
            book_id: r.book_id,
            title: r.title,
            rank: r.rank,
            source: HitSource::Fts,
        })
        .collect())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use ab_core::tunables::DbTunables;
    use ab_db::LibraryDb;
    use tempfile::TempDir;

    async fn db() -> (TempDir, LibraryDb) {
        let dir = TempDir::new().expect("tempdir");
        let lib = LibraryDb::open(&dir.path().join("library.db"), &DbTunables::default())
            .await
            .expect("open");
        (dir, lib)
    }

    async fn add_book(db: &LibraryDb, title: &str, description: Option<&str>) -> i64 {
        let id = sqlx::query!("INSERT INTO books (title) VALUES (?)", title)
            .execute(db.pool())
            .await
            .expect("insert")
            .last_insert_rowid();
        if let Some(d) = description {
            sqlx::query!("UPDATE books SET description = ? WHERE book_id = ?", d, id)
                .execute(db.pool())
                .await
                .expect("set desc");
        }
        id
    }

    #[tokio::test]
    async fn finds_by_title() {
        let (_d, db) = db().await;
        let a = add_book(&db, "The Way of Kings", None).await;
        let _ = add_book(&db, "Mistborn", None).await;
        let hits = search(db.pool(), "kings", 10).await.expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].book_id, a);
    }

    #[tokio::test]
    async fn finds_by_description() {
        let (_d, db) = db().await;
        let a = add_book(&db, "Anonymous", Some("A tale of dragons and wizards")).await;
        let _ = add_book(&db, "Other", Some("Spaceships and lasers")).await;
        let hits = search(db.pool(), "dragons", 10).await.expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].book_id, a);
    }

    #[tokio::test]
    async fn diacritics_collapse() {
        let (_d, db) = db().await;
        let a = add_book(&db, "Café Society", None).await;
        let hits = search(db.pool(), "cafe", 10).await.expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].book_id, a);
    }

    #[tokio::test]
    async fn updates_reindex() {
        let (_d, db) = db().await;
        let a = add_book(&db, "Provisional", None).await;
        // initially "kings" should miss
        let hits = search(db.pool(), "kings", 10).await.expect("search");
        assert_eq!(hits.len(), 0);
        sqlx::query!(
            "UPDATE books SET title = 'The Way of Kings' WHERE book_id = ?",
            a,
        )
        .execute(db.pool())
        .await
        .expect("rename");
        let hits = search(db.pool(), "kings", 10).await.expect("search");
        assert_eq!(hits.len(), 1);
    }

    #[tokio::test]
    async fn soft_deleted_excluded() {
        let (_d, db) = db().await;
        let a = add_book(&db, "Kings", None).await;
        sqlx::query!(
            "UPDATE books SET deleted_at = strftime('%s','now') WHERE book_id = ?",
            a,
        )
        .execute(db.pool())
        .await
        .expect("delete");
        let hits = search(db.pool(), "kings", 10).await.expect("search");
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn empty_query_returns_error() {
        let (_d, db) = db().await;
        let err = search(db.pool(), "   ", 10).await.expect_err("must error");
        assert!(matches!(err, SearchError::Empty));
    }

    #[test]
    fn match_expr_escapes_quotes() {
        assert_eq!(
            build_match_expression("Kings of \"Wyld\""),
            Some(r#""Kings" "of" "Wyld""#.to_owned())
        );
    }

    #[test]
    fn match_expr_handles_empty_input() {
        assert_eq!(build_match_expression("   "), None);
    }
}
