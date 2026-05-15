//! Saved queries — unified persistence + execution (ADR-0034).
//!
//! One table (`saved_queries`), one executor delegating to
//! [`ab_query::execute`]. The `kind` discriminator drives
//! UI presentation; the underlying [`ab_query::QueryFilter`] shape
//! is identical across kinds.

#![forbid(unsafe_code)]
#![allow(missing_docs)] // scaffold; tightened with follow-up slices

use ab_query::{BookListItem, QueryFilter, SortSpec};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SavedQueryKind {
    SeriesView,
    SmartFilter,
    DashboardTile,
    RecentlyAdded,
    SimilarBooks,
    System,
}

impl SavedQueryKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SeriesView => "series_view",
            Self::SmartFilter => "smart_filter",
            Self::DashboardTile => "dashboard_tile",
            Self::RecentlyAdded => "recently_added",
            Self::SimilarBooks => "similar_books",
            Self::System => "system",
        }
    }

    pub fn parse(s: &str) -> Result<Self, SavedQueryError> {
        Ok(match s {
            "series_view" => Self::SeriesView,
            "smart_filter" => Self::SmartFilter,
            "dashboard_tile" => Self::DashboardTile,
            "recently_added" => Self::RecentlyAdded,
            "similar_books" => Self::SimilarBooks,
            "system" => Self::System,
            other => return Err(SavedQueryError::InvalidKind(other.to_owned())),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerKind {
    System,
    User,
}

impl OwnerKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SavedQuery {
    pub query_id: i64,
    pub kind: SavedQueryKind,
    pub name: String,
    pub description: Option<String>,
    pub query_json: String,
    pub sort_json: Option<String>,
    pub pin_position: Option<i64>,
    pub owner_kind: OwnerKind,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateRequest {
    pub kind: SavedQueryKind,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub query: QueryFilter,
    #[serde(default)]
    pub sort: Option<SortSpec>,
    #[serde(default)]
    pub pin_position: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub query: Option<QueryFilter>,
    #[serde(default)]
    pub sort: Option<SortSpec>,
    #[serde(default)]
    pub pin_position: Option<Option<i64>>, // double-option lets caller clear
}

#[derive(Debug, thiserror::Error)]
pub enum SavedQueryError {
    #[error("saved query {0} not found")]
    NotFound(i64),
    #[error("invalid kind: {0}")]
    InvalidKind(String),
    #[error("invalid query JSON: {0}")]
    InvalidQuery(String),
    #[error("cannot mutate system-owned query {0}")]
    SystemReadOnly(i64),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Query(#[from] ab_query::QueryError),
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
}

/// Insert a new saved query. Per-kind validation lives in
/// follow-up slices (the ADR notes `smart_filter` with `series_id`
/// is non-sensical, but those rules ship with the UI).
pub async fn create(pool: &SqlitePool, req: &CreateRequest) -> Result<i64, SavedQueryError> {
    let kind_str = req.kind.as_str();
    let query_json = serde_json::to_string(&req.query)?;
    let sort_json = req.sort.map(|s| serde_json::to_string(&s)).transpose()?;
    let id = sqlx::query!(
        "INSERT INTO saved_queries
            (kind, name, description, query_json, sort_json, pin_position)
         VALUES (?, ?, ?, ?, ?, ?)",
        kind_str,
        req.name,
        req.description,
        query_json,
        sort_json,
        req.pin_position,
    )
    .execute(pool)
    .await?
    .last_insert_rowid();
    Ok(id)
}

async fn fetch_row(pool: &SqlitePool, query_id: i64) -> Result<SavedQuery, SavedQueryError> {
    let row = sqlx::query!(
        r#"SELECT
            kind          AS "kind!: String",
            name          AS "name!: String",
            description   AS "description: String",
            query_json    AS "query_json!: String",
            sort_json     AS "sort_json: String",
            pin_position  AS "pin_position: i64",
            owner_kind    AS "owner_kind!: String",
            created_at    AS "created_at!: i64",
            updated_at    AS "updated_at!: i64"
         FROM saved_queries WHERE query_id = ?"#,
        query_id,
    )
    .fetch_optional(pool)
    .await?
    .ok_or(SavedQueryError::NotFound(query_id))?;
    let kind = SavedQueryKind::parse(&row.kind)?;
    let owner_kind = match row.owner_kind.as_str() {
        "system" => OwnerKind::System,
        _ => OwnerKind::User,
    };
    Ok(SavedQuery {
        query_id,
        kind,
        name: row.name,
        description: row.description,
        query_json: row.query_json,
        sort_json: row.sort_json,
        pin_position: row.pin_position,
        owner_kind,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

/// Read one saved query by id.
pub async fn get(pool: &SqlitePool, query_id: i64) -> Result<SavedQuery, SavedQueryError> {
    fetch_row(pool, query_id).await
}

/// List saved queries, optionally filtered by `kind`.
pub async fn list(
    pool: &SqlitePool,
    kind: Option<SavedQueryKind>,
) -> Result<Vec<SavedQuery>, SavedQueryError> {
    // The kind filter uses `kind = ? OR ? IS NULL` against the
    // bound string; pass NULL to disable. One query, one macro
    // expansion, one Record type — Rust can't unify two
    // sqlx::query!() anon types in if/else arms (slice B.12
    // discovery).
    let kind_str: Option<&'static str> = kind.map(SavedQueryKind::as_str);
    let rows = sqlx::query!(
        r#"SELECT
            query_id      AS "query_id!: i64",
            kind          AS "kind!: String",
            name          AS "name!: String",
            description   AS "description: String",
            query_json    AS "query_json!: String",
            sort_json     AS "sort_json: String",
            pin_position  AS "pin_position: i64",
            owner_kind    AS "owner_kind!: String",
            created_at    AS "created_at!: i64",
            updated_at    AS "updated_at!: i64"
         FROM saved_queries
         WHERE ? IS NULL OR kind = ?
         ORDER BY pin_position IS NULL, pin_position, created_at"#,
        kind_str,
        kind_str,
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|r| {
            let kind = SavedQueryKind::parse(&r.kind)?;
            let owner_kind = match r.owner_kind.as_str() {
                "system" => OwnerKind::System,
                _ => OwnerKind::User,
            };
            Ok(SavedQuery {
                query_id: r.query_id,
                kind,
                name: r.name,
                description: r.description,
                query_json: r.query_json,
                sort_json: r.sort_json,
                pin_position: r.pin_position,
                owner_kind,
                created_at: r.created_at,
                updated_at: r.updated_at,
            })
        })
        .collect()
}

/// Partial update. `pin_position = Some(None)` clears the pin;
/// `pin_position = None` leaves it untouched. System-owned rows
/// reject the update.
pub async fn update(
    pool: &SqlitePool,
    query_id: i64,
    req: &UpdateRequest,
) -> Result<(), SavedQueryError> {
    let current = fetch_row(pool, query_id).await?;
    if matches!(current.owner_kind, OwnerKind::System) {
        return Err(SavedQueryError::SystemReadOnly(query_id));
    }

    let mut tx = pool.begin().await?;

    if let Some(name) = &req.name {
        sqlx::query!(
            "UPDATE saved_queries SET name = ?, updated_at = strftime('%s','now') \
             WHERE query_id = ?",
            name,
            query_id,
        )
        .execute(&mut *tx)
        .await?;
    }
    if let Some(desc) = &req.description {
        sqlx::query!(
            "UPDATE saved_queries SET description = ?, updated_at = strftime('%s','now') \
             WHERE query_id = ?",
            desc,
            query_id,
        )
        .execute(&mut *tx)
        .await?;
    }
    if let Some(filter) = &req.query {
        let json = serde_json::to_string(filter)?;
        sqlx::query!(
            "UPDATE saved_queries SET query_json = ?, updated_at = strftime('%s','now') \
             WHERE query_id = ?",
            json,
            query_id,
        )
        .execute(&mut *tx)
        .await?;
    }
    if let Some(sort) = req.sort {
        let json = serde_json::to_string(&sort)?;
        sqlx::query!(
            "UPDATE saved_queries SET sort_json = ?, updated_at = strftime('%s','now') \
             WHERE query_id = ?",
            json,
            query_id,
        )
        .execute(&mut *tx)
        .await?;
    }
    if let Some(pin) = req.pin_position {
        // Outer Some signals "update was requested"; inner value
        // is the new pin (None clears).
        sqlx::query!(
            "UPDATE saved_queries SET pin_position = ?, updated_at = strftime('%s','now') \
             WHERE query_id = ?",
            pin,
            query_id,
        )
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

/// Delete a saved query. System-owned rows reject the delete.
pub async fn delete(pool: &SqlitePool, query_id: i64) -> Result<(), SavedQueryError> {
    let current = fetch_row(pool, query_id).await?;
    if matches!(current.owner_kind, OwnerKind::System) {
        return Err(SavedQueryError::SystemReadOnly(query_id));
    }
    sqlx::query!("DELETE FROM saved_queries WHERE query_id = ?", query_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Execute a saved query: load + parse + apply optional sort
/// override + delegate to [`ab_query::execute`].
pub async fn execute(
    pool: &SqlitePool,
    query_id: i64,
) -> Result<Vec<BookListItem>, SavedQueryError> {
    let row = fetch_row(pool, query_id).await?;
    let mut filter: QueryFilter = serde_json::from_str(&row.query_json)
        .map_err(|e| SavedQueryError::InvalidQuery(e.to_string()))?;
    if let Some(sort_json) = row.sort_json {
        let sort: SortSpec = serde_json::from_str(&sort_json)
            .map_err(|e| SavedQueryError::InvalidQuery(e.to_string()))?;
        filter.sort = Some(sort);
    }
    let rows = ab_query::execute(pool, &filter).await?;
    Ok(rows)
}

/// Count rows that would be returned by [`execute`].
pub async fn count(pool: &SqlitePool, query_id: i64) -> Result<u64, SavedQueryError> {
    let row = fetch_row(pool, query_id).await?;
    let filter: QueryFilter = serde_json::from_str(&row.query_json)
        .map_err(|e| SavedQueryError::InvalidQuery(e.to_string()))?;
    let n = ab_query::count(pool, &filter).await?;
    Ok(n)
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
    async fn round_trip_create_get_execute() {
        let (_d, db) = db().await;
        let _ = add_book(&db, "Kings of Wyld").await;
        let _ = add_book(&db, "Mistborn").await;

        let id = create(
            db.pool(),
            &CreateRequest {
                kind: SavedQueryKind::SmartFilter,
                name: "Kings hits".into(),
                description: Some("title LIKE kings".into()),
                query: QueryFilter {
                    q: Some("Kings".into()),
                    ..Default::default()
                },
                sort: None,
                pin_position: None,
            },
        )
        .await
        .expect("create");

        let row = get(db.pool(), id).await.expect("get");
        assert_eq!(row.name, "Kings hits");
        assert_eq!(row.kind, SavedQueryKind::SmartFilter);

        let items = execute(db.pool(), id).await.expect("execute");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Kings of Wyld");
    }

    #[tokio::test]
    async fn list_filters_by_kind() {
        let (_d, db) = db().await;
        let _ = create(
            db.pool(),
            &CreateRequest {
                kind: SavedQueryKind::DashboardTile,
                name: "Continue Listening".into(),
                description: None,
                query: QueryFilter::default(),
                sort: None,
                pin_position: Some(0),
            },
        )
        .await
        .expect("create tile");
        let _ = create(
            db.pool(),
            &CreateRequest {
                kind: SavedQueryKind::SmartFilter,
                name: "Fantasy".into(),
                description: None,
                query: QueryFilter::default(),
                sort: None,
                pin_position: None,
            },
        )
        .await
        .expect("create filter");

        let tiles = list(db.pool(), Some(SavedQueryKind::DashboardTile))
            .await
            .expect("list tiles");
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].name, "Continue Listening");

        let all = list(db.pool(), None).await.expect("list all");
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn system_rows_reject_mutation() {
        let (_d, db) = db().await;
        // Insert with owner_kind='system' via raw SQL — the public
        // create API only mints user rows.
        let json = "{}";
        let id = sqlx::query!(
            "INSERT INTO saved_queries
                (kind, name, query_json, owner_kind)
             VALUES ('system', 'all books', ?, 'system')",
            json,
        )
        .execute(db.pool())
        .await
        .expect("insert system")
        .last_insert_rowid();

        let err = delete(db.pool(), id).await.expect_err("must refuse");
        assert!(matches!(err, SavedQueryError::SystemReadOnly(_)));
    }

    #[tokio::test]
    async fn update_partial_fields() {
        let (_d, db) = db().await;
        let id = create(
            db.pool(),
            &CreateRequest {
                kind: SavedQueryKind::SmartFilter,
                name: "old name".into(),
                description: None,
                query: QueryFilter::default(),
                sort: None,
                pin_position: None,
            },
        )
        .await
        .expect("create");
        update(
            db.pool(),
            id,
            &UpdateRequest {
                name: Some("new name".into()),
                description: None,
                query: None,
                sort: None,
                pin_position: None,
            },
        )
        .await
        .expect("update");
        let row = get(db.pool(), id).await.expect("get");
        assert_eq!(row.name, "new name");
    }
}
