//! Canonical `QueryFilter` + executor for "list of books" queries (ADR-0031).
//!
//! Every caller that asks the daemon for books — the native API
//! `GET /books`, the planned `saved_queries` executor, the
//! conversational dispatcher's tool-call args, FTS5 search — runs
//! through this crate. One struct, one executor; new filter
//! dimensions land in one place.
//!
//! Slice B.3 ships the subset the existing `books_list` handler
//! needed plus the schema-already-supported dimensions (`language`,
//! `abridged`, `imported_after_unix` / `imported_before_unix`,
//! `min_duration_secs` / `max_duration_secs`, `include_deleted`).
//! Reading-status + rating filters (`reading_status`, `min_rating`)
//! arrive once B.5 / B.6's schema lands; companion filters
//! (`has_companions`, `companions_unpaired_nearby`) and the
//! `q_fuzzy` flag arrive with their owning slices (Phase C / B.16).
//!
//! ## SQL shape
//!
//! The executor builds the candidate-set query with LEFT JOINs on
//! `authors` / `book_series` / `series` and a subquery for the
//! comma-joined narrator list (because narrators are 1:N and a
//! `GROUP_CONCAT` in the projection breaks pagination). The previous
//! handler used per-row correlated subselects for every column;
//! the LEFT JOIN rewrite is the perf foundation for the 100k-book
//! target (#105).

#![forbid(unsafe_code)]
#![allow(missing_docs)] // scaffold; tightened in follow-up slices

use serde::{Deserialize, Serialize};
use sqlx::{QueryBuilder, Sqlite, SqlitePool};

/// One row of a `GET /books` response.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct BookListItem {
    pub book_id: i64,
    pub title: String,
    pub file_path: Option<String>,
    pub author: Option<String>,
    pub narrators: Option<String>,
    pub series: Option<String>,
}

/// Sort dimension + direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortDim {
    BookId,
    Title,
    ImportedAt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortDir {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SortSpec {
    pub dim: SortDim,
    pub dir: SortDir,
}

impl Default for SortSpec {
    fn default() -> Self {
        Self {
            dim: SortDim::BookId,
            dir: SortDir::Asc,
        }
    }
}

/// Canonical filter shape consumed by every books-list query.
///
/// `deny_unknown_fields` catches LLM-emitted hallucinated keys at
/// deserialise time so the executor never gets to silently ignore
/// them.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct QueryFilter {
    pub q: Option<String>,
    pub author: Option<String>,
    pub series: Option<String>,
    pub language: Option<Vec<String>>,
    pub min_duration_secs: Option<u64>,
    pub max_duration_secs: Option<u64>,
    pub abridged: Option<bool>,
    /// Filter to books imported after the given unix-seconds timestamp.
    pub imported_after_unix: Option<i64>,
    /// Filter to books imported before the given unix-seconds timestamp.
    pub imported_before_unix: Option<i64>,
    pub include_deleted: bool,
    pub sort: Option<SortSpec>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

/// Hard cap on `limit` accepted by the executor. Anything larger
/// is silently clamped.
pub const DEFAULT_LIMIT: u32 = 100;
pub const MAX_LIMIT: u32 = 500;

#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error("invalid filter: {0}")]
    Invalid(String),
}

const SELECT_HEAD: &str = "
    SELECT
        b.book_id                                                   AS book_id,
        b.title                                                     AS title,
        (SELECT file_path FROM book_files
           WHERE book_id = b.book_id LIMIT 1)                       AS file_path,
        COALESCE(
            (SELECT alias FROM author_aliases
               WHERE author_id = b.author_id AND is_prime = 1 LIMIT 1),
            a.name
        )                                                           AS author,
        (SELECT GROUP_CONCAT(
                    COALESCE(
                        (SELECT alias FROM narrator_aliases na
                           WHERE na.narrator_id = n.narrator_id AND is_prime = 1 LIMIT 1),
                        n.name
                    ), ', ')
           FROM book_narrator bn
           JOIN narrators n ON n.narrator_id = bn.narrator_id
           WHERE bn.book_id = b.book_id)                            AS narrators,
        COALESCE(
            (SELECT alias FROM series_aliases sa
               WHERE sa.series_id = s.series_id AND is_prime = 1 LIMIT 1),
            s.name
        )                                                           AS series
    FROM books b
    LEFT JOIN authors a       ON a.author_id = b.author_id
    LEFT JOIN book_series bs  ON bs.book_id  = b.book_id AND bs.is_primary = 1
    LEFT JOIN series s        ON s.series_id = bs.series_id
";

/// Apply every WHERE clause that's a pure function of the filter.
/// Pulled out of `execute` / `count` so both share the same shape.
fn apply_where<'a>(qb: &mut QueryBuilder<'a, Sqlite>, f: &'a QueryFilter) {
    qb.push(" WHERE 1 = 1 ");

    if !f.include_deleted {
        qb.push(" AND b.deleted_at IS NULL ");
    }
    if let Some(needle) = f.q.as_deref().filter(|s| !s.is_empty()) {
        qb.push(" AND b.title LIKE ")
            .push_bind(format!("%{needle}%"));
    }
    if let Some(needle) = f.author.as_deref().filter(|s| !s.is_empty()) {
        qb.push(
            " AND COALESCE(\
                (SELECT alias FROM author_aliases \
                   WHERE author_id = b.author_id AND is_prime = 1 LIMIT 1),\
                a.name) LIKE ",
        )
        .push_bind(format!("%{needle}%"));
    }
    if let Some(needle) = f.series.as_deref().filter(|s| !s.is_empty()) {
        qb.push(
            " AND COALESCE(\
                (SELECT alias FROM series_aliases sa \
                   WHERE sa.series_id = s.series_id AND is_prime = 1 LIMIT 1),\
                s.name) LIKE ",
        )
        .push_bind(format!("%{needle}%"));
    }
    if let Some(langs) = f.language.as_ref().filter(|v| !v.is_empty()) {
        qb.push(" AND b.language IN (");
        let mut sep = qb.separated(", ");
        for lang in langs {
            sep.push_bind(lang);
        }
        sep.push_unseparated(") ");
    }
    if let Some(secs) = f.min_duration_secs {
        let ms = i64::try_from(secs.saturating_mul(1000)).unwrap_or(i64::MAX);
        qb.push(" AND b.duration_ms IS NOT NULL AND b.duration_ms >= ")
            .push_bind(ms);
    }
    if let Some(secs) = f.max_duration_secs {
        let ms = i64::try_from(secs.saturating_mul(1000)).unwrap_or(i64::MAX);
        qb.push(" AND b.duration_ms IS NOT NULL AND b.duration_ms <= ")
            .push_bind(ms);
    }
    if let Some(abridged) = f.abridged {
        qb.push(" AND b.abridged = ").push_bind(i64::from(abridged));
    }
    if let Some(unix) = f.imported_after_unix {
        qb.push(" AND b.created_at >= ").push_bind(unix);
    }
    if let Some(unix) = f.imported_before_unix {
        qb.push(" AND b.created_at <= ").push_bind(unix);
    }
}

/// Run `f` against `pool` and return matching rows.
pub async fn execute(pool: &SqlitePool, f: &QueryFilter) -> Result<Vec<BookListItem>, QueryError> {
    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(SELECT_HEAD);
    apply_where(&mut qb, f);

    let sort = f.sort.unwrap_or_default();
    let (col, dir) = sort_clause(sort);
    qb.push(" ORDER BY ").push(col).push(" ").push(dir);

    let limit = f.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    let offset = f.offset.unwrap_or(0);
    qb.push(" LIMIT ")
        .push_bind(i64::from(limit))
        .push(" OFFSET ")
        .push_bind(i64::from(offset));

    Ok(qb.build_query_as::<BookListItem>().fetch_all(pool).await?)
}

/// Count rows matching `f` (ignores pagination).
pub async fn count(pool: &SqlitePool, f: &QueryFilter) -> Result<u64, QueryError> {
    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
        "SELECT COUNT(*) AS n FROM books b \
         LEFT JOIN authors a       ON a.author_id = b.author_id \
         LEFT JOIN book_series bs  ON bs.book_id  = b.book_id AND bs.is_primary = 1 \
         LEFT JOIN series s        ON s.series_id = bs.series_id ",
    );
    apply_where(&mut qb, f);
    let n: i64 = qb.build_query_scalar().fetch_one(pool).await?;
    Ok(u64::try_from(n).unwrap_or(0))
}

const fn sort_clause(s: SortSpec) -> (&'static str, &'static str) {
    let col = match s.dim {
        SortDim::BookId => "b.book_id",
        SortDim::Title => "b.title",
        SortDim::ImportedAt => "b.created_at",
    };
    let dir = match s.dir {
        SortDir::Asc => "ASC",
        SortDir::Desc => "DESC",
    };
    (col, dir)
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

    async fn add_book(db: &LibraryDb, title: &str) -> i64 {
        sqlx::query!("INSERT INTO books (title) VALUES (?)", title)
            .execute(db.pool())
            .await
            .expect("insert")
            .last_insert_rowid()
    }

    #[tokio::test]
    async fn empty_filter_returns_all() {
        let (_d, db) = db().await;
        for t in ["Alpha", "Bravo", "Charlie"] {
            let _ = add_book(&db, t).await;
        }
        let f = QueryFilter::default();
        let rows = execute(db.pool(), &f).await.expect("exec");
        assert_eq!(rows.len(), 3);
        let n = count(db.pool(), &f).await.expect("count");
        assert_eq!(n, 3);
    }

    #[tokio::test]
    async fn q_filters_by_title_substring() {
        let (_d, db) = db().await;
        let _ = add_book(&db, "The Way of Kings").await;
        let _ = add_book(&db, "Mistborn").await;
        let f = QueryFilter {
            q: Some("Kings".into()),
            ..Default::default()
        };
        let rows = execute(db.pool(), &f).await.expect("exec");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "The Way of Kings");
    }

    #[tokio::test]
    async fn language_filter() {
        let (_d, db) = db().await;
        let a = add_book(&db, "A").await;
        let _b = add_book(&db, "B").await;
        sqlx::query!("UPDATE books SET language = 'en' WHERE book_id = ?", a)
            .execute(db.pool())
            .await
            .expect("set lang");
        let f = QueryFilter {
            language: Some(vec!["en".into()]),
            ..Default::default()
        };
        let rows = execute(db.pool(), &f).await.expect("exec");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].book_id, a);
    }

    #[tokio::test]
    async fn limit_offset_paginate() {
        let (_d, db) = db().await;
        for i in 0..10 {
            let _ = add_book(&db, &format!("Book {i:02}")).await;
        }
        let f = QueryFilter {
            limit: Some(3),
            offset: Some(2),
            ..Default::default()
        };
        let rows = execute(db.pool(), &f).await.expect("exec");
        assert_eq!(rows.len(), 3);
    }

    #[tokio::test]
    async fn sort_title_asc() {
        let (_d, db) = db().await;
        let _ = add_book(&db, "Charlie").await;
        let _ = add_book(&db, "Alpha").await;
        let _ = add_book(&db, "Bravo").await;
        let f = QueryFilter {
            sort: Some(SortSpec {
                dim: SortDim::Title,
                dir: SortDir::Asc,
            }),
            ..Default::default()
        };
        let rows = execute(db.pool(), &f).await.expect("exec");
        assert_eq!(
            rows.iter().map(|r| r.title.as_str()).collect::<Vec<_>>(),
            vec!["Alpha", "Bravo", "Charlie"]
        );
    }

    #[tokio::test]
    async fn include_deleted_round_trip() {
        let (_d, db) = db().await;
        let a = add_book(&db, "Active").await;
        let g = add_book(&db, "Gone").await;
        sqlx::query!(
            "UPDATE books SET deleted_at = strftime('%s','now') WHERE book_id = ?",
            g,
        )
        .execute(db.pool())
        .await
        .expect("delete");
        let active_only = execute(db.pool(), &QueryFilter::default())
            .await
            .expect("exec");
        assert_eq!(active_only.len(), 1);
        assert_eq!(active_only[0].book_id, a);

        let f = QueryFilter {
            include_deleted: true,
            ..Default::default()
        };
        let all = execute(db.pool(), &f).await.expect("exec");
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn filter_round_trips_through_serde() {
        let f = QueryFilter {
            q: Some("kings".into()),
            language: Some(vec!["en".into(), "de".into()]),
            abridged: Some(false),
            sort: Some(SortSpec {
                dim: SortDim::Title,
                dir: SortDir::Desc,
            }),
            limit: Some(50),
            ..Default::default()
        };
        let json = serde_json::to_string(&f).expect("ser");
        let back: QueryFilter = serde_json::from_str(&json).expect("de");
        assert_eq!(f, back);
    }

    #[test]
    fn unknown_fields_rejected() {
        let json = r#"{"q":"kings","bogus":42}"#;
        let err: Result<QueryFilter, _> = serde_json::from_str(json);
        assert!(err.is_err());
    }
}
