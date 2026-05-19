//! `GET /api/v1/collections` list + `GET /api/v1/collections/{collection_id}`
//! detail. Reads from the `book_collections` + `book_collection_members`
//! tables landed in migration 043.
//!
//! Collections are box sets, publisher bundles, and operator-curated
//! groupings — distinct from series (which carry authoritative
//! ordering). The endpoints surface the same shape as `/publishers`:
//! paginated list with name + `book_count` + `audible_id`, single-row
//! detail with the full row + `book_count`.
//!
//! Books-in-collection (`/collections/{id}/books`) reuses the
//! `EntityBookSummary` types from `crate::entity_books` and orders
//! by `member.position` first (ordered box sets render in canonical
//! sequence) then by `release_date` / title (unordered bags get a
//! sensible fallback).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, response::Response};
use serde::{Deserialize, Serialize};

use crate::ApiError;
use crate::entity_books::{EntityBookSummary, EntityBooksQuery, EntityBooksResponse};
use crate::pagination::{clamp_limit, clamp_offset};
use crate::state::ApiState;

/// Collection detail JSON returned by the GET endpoint.
#[derive(Debug, Serialize)]
pub struct CollectionDetail {
    pub collection_id: i64,
    pub name: String,
    /// Normalised form; populated by future canonical-collection
    /// enrich work. `null` for scanner-detected / operator-curated
    /// collections that haven't been enriched yet.
    pub canonical_name: Option<String>,
    /// Audible collection ASIN. `null` when the collection isn't
    /// mirrored on Audible (operator-curated or scanner-detected
    /// bundles).
    pub audible_id: Option<String>,
    pub description: Option<String>,
    /// Free-text classification: `box_set`, `compilation`,
    /// `curated`, … See migration 043 header for the rationale
    /// against an enum today.
    pub kind: Option<String>,
    /// Member count — JOIN on `book_collection_members` (no
    /// CASCADE-aware filter needed; member rows go away with
    /// their parent collection or book).
    pub book_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// `GET /api/v1/collections/{collection_id}`
///
/// Returns `200 OK` with [`CollectionDetail`] JSON. `404 Not Found`
/// when no `book_collections` row exists at that id.
///
/// # Errors
///
/// Database access failures surface as [`ApiError::Internal`].
#[allow(clippy::missing_panics_doc)] // panic-free
pub async fn collections_get(
    State(state): State<ApiState>,
    Path(collection_id): Path<i64>,
) -> Result<Response, ApiError> {
    let row = sqlx::query!(
        r#"SELECT c.collection_id  AS "collection_id!: i64",
                  c.name           AS "name!: String",
                  c.canonical_name AS "canonical_name?: String",
                  c.audible_id     AS "audible_id?: String",
                  c.description    AS "description?: String",
                  c.kind           AS "kind?: String",
                  c.created_at     AS "created_at!: i64",
                  c.updated_at     AS "updated_at!: i64",
                  (SELECT COUNT(*) FROM book_collection_members
                    WHERE collection_id = c.collection_id)
                      AS "book_count!: i64"
             FROM book_collections c
            WHERE c.collection_id = ?"#,
        collection_id,
    )
    .fetch_optional(state.inner.library.pool())
    .await
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("collection lookup: {e}"))))?;

    let Some(r) = row else {
        return Err(ApiError::NotFound(format!("collection {collection_id}")));
    };

    let detail = CollectionDetail {
        collection_id: r.collection_id,
        name: r.name,
        canonical_name: r.canonical_name,
        audible_id: r.audible_id,
        description: r.description,
        kind: r.kind,
        book_count: r.book_count,
        created_at: r.created_at,
        updated_at: r.updated_at,
    };
    Ok((StatusCode::OK, Json(detail)).into_response())
}

/// Compact row in the paginated collections list. Drops `description`
/// (potentially long), `created_at`, `updated_at`.
#[derive(Debug, Serialize)]
pub struct CollectionListItem {
    pub collection_id: i64,
    pub name: String,
    pub canonical_name: Option<String>,
    pub audible_id: Option<String>,
    pub kind: Option<String>,
    pub book_count: i64,
}

/// Response body for `GET /api/v1/collections`.
#[derive(Debug, Serialize)]
pub struct CollectionsListResponse {
    pub collections: Vec<CollectionListItem>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// Query-string params for the collections list endpoint.
#[derive(Debug, Deserialize, Default)]
pub struct CollectionsListQuery {
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
    /// Sort key. One of:
    ///
    /// * `name` — case-insensitive (default).
    /// * `book_count` — descending, then by name ascending.
    ///
    /// Unknown values produce `400 Bad Request`.
    #[serde(default)]
    pub sort: Option<String>,
    /// Case-insensitive substring filter on `name`. Empty string =
    /// no filter.
    #[serde(default)]
    pub q: Option<String>,
    /// Optional `kind` filter (`box_set`, `compilation`, …). Pass
    /// the literal value; empty string = no filter.
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CollectionsSort {
    Name,
    BookCountDesc,
}

impl CollectionsSort {
    fn parse(s: Option<&str>) -> Result<Self, ApiError> {
        match s {
            None | Some("" | "name") => Ok(Self::Name),
            Some("book_count") => Ok(Self::BookCountDesc),
            Some(other) => Err(ApiError::BadRequest(format!(
                "unknown sort {other:?}; expected one of name / book_count"
            ))),
        }
    }
}

/// `GET /api/v1/collections[?limit=&offset=&sort=&q=&kind=]`
///
/// Returns `200 OK` with [`CollectionsListResponse`] JSON. `400 Bad
/// Request` for an unknown `sort` value.
///
/// # Errors
///
/// Database access failures surface as [`ApiError::Internal`].
#[allow(clippy::missing_panics_doc, clippy::too_many_lines)] // panic-free; macro arms inflate line count
pub async fn collections_list(
    State(state): State<ApiState>,
    Query(params): Query<CollectionsListQuery>,
) -> Result<Response, ApiError> {
    let sort = CollectionsSort::parse(params.sort.as_deref())?;
    let limit = clamp_limit(params.limit);
    let offset = clamp_offset(params.offset);

    let q_filter = params
        .q
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| format!("%{s}%"));
    let kind_filter = params
        .kind
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let total: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!: i64"
             FROM book_collections
            WHERE (?1 IS NULL OR name LIKE ?1 COLLATE NOCASE)
              AND (?2 IS NULL OR kind = ?2)"#,
        q_filter,
        kind_filter,
    )
    .fetch_one(state.inner.library.pool())
    .await
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("collections count: {e}"))))?;

    let pool = state.inner.library.pool();
    let collections: Vec<CollectionListItem> = match sort {
        CollectionsSort::Name => sqlx::query!(
            r#"SELECT c.collection_id  AS "collection_id!: i64",
                      c.name           AS "name!: String",
                      c.canonical_name AS "canonical_name?: String",
                      c.audible_id     AS "audible_id?: String",
                      c.kind           AS "kind?: String",
                      (SELECT COUNT(*) FROM book_collection_members
                        WHERE collection_id = c.collection_id)
                          AS "book_count!: i64"
                 FROM book_collections c
                WHERE (?1 IS NULL OR c.name LIKE ?1 COLLATE NOCASE)
                  AND (?2 IS NULL OR c.kind = ?2)
                ORDER BY c.name COLLATE NOCASE
                LIMIT ?3 OFFSET ?4"#,
            q_filter,
            kind_filter,
            limit,
            offset,
        )
        .fetch_all(pool)
        .await
        .map(|rows| {
            rows.into_iter()
                .map(|r| CollectionListItem {
                    collection_id: r.collection_id,
                    name: r.name,
                    canonical_name: r.canonical_name,
                    audible_id: r.audible_id,
                    kind: r.kind,
                    book_count: r.book_count,
                })
                .collect()
        }),
        CollectionsSort::BookCountDesc => sqlx::query!(
            r#"SELECT c.collection_id  AS "collection_id!: i64",
                      c.name           AS "name!: String",
                      c.canonical_name AS "canonical_name?: String",
                      c.audible_id     AS "audible_id?: String",
                      c.kind           AS "kind?: String",
                      (SELECT COUNT(*) FROM book_collection_members
                        WHERE collection_id = c.collection_id)
                          AS "book_count!: i64"
                 FROM book_collections c
                WHERE (?1 IS NULL OR c.name LIKE ?1 COLLATE NOCASE)
                  AND (?2 IS NULL OR c.kind = ?2)
                ORDER BY (SELECT COUNT(*) FROM book_collection_members
                           WHERE collection_id = c.collection_id) DESC,
                         c.name COLLATE NOCASE
                LIMIT ?3 OFFSET ?4"#,
            q_filter,
            kind_filter,
            limit,
            offset,
        )
        .fetch_all(pool)
        .await
        .map(|rows| {
            rows.into_iter()
                .map(|r| CollectionListItem {
                    collection_id: r.collection_id,
                    name: r.name,
                    canonical_name: r.canonical_name,
                    audible_id: r.audible_id,
                    kind: r.kind,
                    book_count: r.book_count,
                })
                .collect()
        }),
    }
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("collections list: {e}"))))?;

    Ok((
        StatusCode::OK,
        Json(CollectionsListResponse {
            collections,
            total,
            limit,
            offset,
        }),
    )
        .into_response())
}

/// `GET /api/v1/collections/{collection_id}/books[?limit=&offset=]`
///
/// Returns `200 OK` with [`EntityBooksResponse`] JSON listing the
/// books joined to this collection via `book_collection_members`.
/// `404 Not Found` when the collection doesn't exist. Empty `books`
/// array when the collection exists but has no members yet
/// (legal — operator may have created the row before populating it).
///
/// Junction-table predicate like `narrators_books`. Each book
/// appears at most once per collection, but a book in multiple
/// collections appears in every other collection's bucket — same
/// convention as `book_narrator`.
///
/// Ordering: `position` ASC (NULLs last), then `release_date DESC`
/// (NULLs last), then `books.title COLLATE NOCASE`. The
/// `position`-first sort means ordered box sets render in canonical
/// sequence (volume 1, volume 2, …) while unordered "bag"
/// collections fall back to a sensible chronological / alphabetical
/// listing.
///
/// # Errors
///
/// Database access failures surface as [`ApiError::Internal`].
#[allow(clippy::missing_panics_doc)] // panic-free
pub async fn collections_books(
    State(state): State<ApiState>,
    Path(collection_id): Path<i64>,
    Query(params): Query<EntityBooksQuery>,
) -> Result<Response, ApiError> {
    let pool = state.inner.library.pool();
    let limit = clamp_limit(params.limit);
    let offset = clamp_offset(params.offset);

    let collection_exists = sqlx::query_scalar!(
        r#"SELECT 1 AS "n!: i64" FROM book_collections WHERE collection_id = ?"#,
        collection_id,
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "collection existence check: {e}"
        )))
    })?
    .is_some();

    if !collection_exists {
        return Err(ApiError::NotFound(format!("collection {collection_id}")));
    }

    let total: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!: i64"
             FROM book_collection_members m
             JOIN books b ON b.book_id = m.book_id
            WHERE m.collection_id = ?"#,
        collection_id,
    )
    .fetch_one(pool)
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "collection books count: {e}"
        )))
    })?;

    let rows = sqlx::query!(
        r#"SELECT b.book_id          AS "book_id!: i64",
                  b.title            AS "title!: String",
                  b.release_date     AS "release_date?: String",
                  b.duration_ms      AS "duration_ms?: i64",
                  b.reading_status   AS "reading_status!: String"
             FROM book_collection_members m
             JOIN books b ON b.book_id = m.book_id
            WHERE m.collection_id = ?
            ORDER BY m.position IS NULL,
                     m.position,
                     b.release_date IS NULL,
                     b.release_date DESC,
                     b.title COLLATE NOCASE
            LIMIT ? OFFSET ?"#,
        collection_id,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("collection books: {e}"))))?;

    let books: Vec<EntityBookSummary> = rows
        .into_iter()
        .map(|r| EntityBookSummary {
            book_id: r.book_id,
            title: r.title,
            release_date: r.release_date,
            duration_ms: r.duration_ms,
            reading_status: r.reading_status,
        })
        .collect();

    Ok((
        StatusCode::OK,
        Json(EntityBooksResponse {
            books,
            total,
            limit,
            offset,
        }),
    )
        .into_response())
}

/// One row in [`BookCollectionsResponse`]. Slim because the
/// book-detail page renders dozens of these in a "Also in these
/// collections" strip.
#[derive(Debug, Serialize)]
pub struct BookCollectionEntry {
    pub collection_id: i64,
    pub name: String,
    pub canonical_name: Option<String>,
    pub kind: Option<String>,
    /// Member ordinal in this collection (NULL = unordered bag,
    /// matches the `book_collection_members.position` semantics from
    /// migration 043).
    pub position: Option<i64>,
}

/// Response body for `GET /api/v1/books/{book_id}/collections`.
///
/// No pagination: a single book can only belong to a handful of
/// collections in practice; the natural cap is the number of
/// collections the operator + scanner have created. If the
/// catalogue grows enough to need pagination here, lift this to
/// the entity-list shape (total + limit + offset).
#[derive(Debug, Serialize)]
pub struct BookCollectionsResponse {
    pub collections: Vec<BookCollectionEntry>,
}

/// `GET /api/v1/books/{book_id}/collections`
///
/// Reverse lookup mirroring `/collections/{id}/books` (cycle 35).
/// Returns the list of collections this book belongs to, ordered
/// by collection name. Empty `collections` array when the book
/// exists but has no membership.
///
/// `404 Not Found` when no `books` row exists at that id.
///
/// # Errors
///
/// Database access failures surface as [`ApiError::Internal`].
#[allow(clippy::missing_panics_doc)] // panic-free
pub async fn books_collections(
    State(state): State<ApiState>,
    Path(book_id): Path<i64>,
) -> Result<Response, ApiError> {
    let pool = state.inner.library.pool();

    let book_exists = sqlx::query_scalar!(
        r#"SELECT 1 AS "n!: i64" FROM books WHERE book_id = ?"#,
        book_id,
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "book existence check: {e}"
        )))
    })?
    .is_some();
    if !book_exists {
        return Err(ApiError::NotFound(format!("book {book_id}")));
    }

    let rows = sqlx::query!(
        r#"SELECT c.collection_id  AS "collection_id!: i64",
                  c.name           AS "name!: String",
                  c.canonical_name AS "canonical_name?: String",
                  c.kind           AS "kind?: String",
                  m.position       AS "position?: i64"
             FROM book_collection_members m
             JOIN book_collections c ON c.collection_id = m.collection_id
            WHERE m.book_id = ?
            ORDER BY c.name COLLATE NOCASE"#,
        book_id,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("book collections: {e}"))))?;

    let collections: Vec<BookCollectionEntry> = rows
        .into_iter()
        .map(|r| BookCollectionEntry {
            collection_id: r.collection_id,
            name: r.name,
            canonical_name: r.canonical_name,
            kind: r.kind,
            position: r.position,
        })
        .collect();

    Ok((
        StatusCode::OK,
        Json(BookCollectionsResponse { collections }),
    )
        .into_response())
}

/// `op_kind` recorded in `operation_journal` for `POST /collections`
/// (ADR-0039). Stable string used by
/// [`crate::journal_replayers::CollectionCreateReplayer`] to claim
/// rows on crash recovery.
///
/// `pre_state = { intent: <body-fields> }`. `post_state =
/// { collection_id, name }`.
pub const OP_KIND_COLLECTION_CREATE: &str = "collection-create";

/// Body of `POST /api/v1/collections`.
///
/// `name` is required + non-empty after trim. The other four
/// fields are optional and land verbatim on the new
/// `book_collections` row.
#[derive(Debug, Deserialize)]
pub struct CollectionCreateRequest {
    pub name: String,
    #[serde(default)]
    pub canonical_name: Option<String>,
    #[serde(default)]
    pub audible_id: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Free-text classification (`box_set` / `compilation` / …).
    /// Validated by the doctor `collections-duplicate-audible-id`
    /// check + the UNIQUE constraint on `(name)`; this endpoint
    /// stores the verbatim value.
    #[serde(default)]
    pub kind: Option<String>,
}

/// Response from `POST /api/v1/collections`.
#[derive(Debug, Serialize)]
pub struct CollectionCreateResponse {
    pub collection_id: i64,
}

/// `POST /api/v1/collections` — operator-curated create.
///
/// Body: [`CollectionCreateRequest`]. Returns `201 Created` with
/// [`CollectionCreateResponse`] on success.
///
/// Validates `name` is non-empty after trim. `409 Conflict` when
/// the UNIQUE constraint on `name` (or partial UNIQUE on
/// `audible_id`) fires. `400 Bad Request` for an empty `name`.
///
/// Journal capture (ADR-0039): after the INSERT commits, records
/// an `operation_journal` row with `op_kind = collection-create`,
/// `target = { kind: "collection", id: <new_id> }`,
/// `pre_state.intent = <request body>`, `post_state =
/// { collection_id, name }`. A journal-write failure is logged
/// (warn) but does NOT undo the insert — the row is still created.
///
/// # Errors
///
/// Database access failures surface as [`ApiError::Internal`].
#[allow(clippy::missing_panics_doc, clippy::too_many_lines)] // panic-free; insert + 409 mapping + journal capture all inline for top-to-bottom readability
pub async fn collections_create(
    State(state): State<ApiState>,
    Json(req): Json<CollectionCreateRequest>,
) -> Result<Response, ApiError> {
    // 1. Validate + normalise inputs.
    let name = req.name.trim();
    if name.is_empty() {
        return Err(ApiError::BadRequest(
            "name is required and must not be empty".to_owned(),
        ));
    }
    let name = name.to_owned();
    let canonical_name = req
        .canonical_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let audible_id = req
        .audible_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let description = req
        .description
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let kind = req
        .kind
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let pool = state.inner.library.pool();

    // 2. INSERT — UNIQUE on name + partial UNIQUE on audible_id
    //    surface as `409 Conflict`.
    let result = sqlx::query!(
        r#"INSERT INTO book_collections
             (name, canonical_name, audible_id, description, kind)
           VALUES (?, ?, ?, ?, ?)
           RETURNING collection_id AS "collection_id!: i64""#,
        name,
        canonical_name,
        audible_id,
        description,
        kind,
    )
    .fetch_one(pool)
    .await;

    let collection_id = match result {
        Ok(r) => r.collection_id,
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("UNIQUE constraint failed") {
                let column = if msg.contains("book_collections.name") {
                    "name"
                } else if msg.contains("book_collections.audible_id") {
                    "audible_id"
                } else {
                    "unique"
                };
                return Err(ApiError::Conflict(format!(
                    "collection {column} already exists"
                )));
            }
            return Err(ApiError::Internal(ab_core::Error::Database(format!(
                "collections_create insert: {e}"
            ))));
        }
    };

    // 3. Journal capture (ADR-0039). target.id is the new row's
    //    collection_id; pre_state captures the request body
    //    verbatim so a future replay / undo has the full intent.
    //    A journal-write failure does NOT undo the insert.
    let intent = serde_json::json!({
        "name": name,
        "canonical_name": canonical_name,
        "audible_id": audible_id,
        "description": description,
        "kind": kind,
    });
    let entry = ab_journal::NewEntry {
        op_kind: OP_KIND_COLLECTION_CREATE,
        target: ab_journal::Target {
            kind: "collection".to_owned(),
            id: collection_id,
        },
        pre_state: serde_json::json!({ "intent": intent }),
        reversible: false,
        batch_id: None,
    };
    match crate::journal_capture::record_pending(pool, &entry).await {
        Ok(op_id) => {
            crate::journal_capture::mark_done_or_log(
                pool,
                op_id,
                &serde_json::json!({ "collection_id": collection_id, "name": name }),
                "api.collections_create",
            )
            .await;
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                collection_id,
                "api.collections_create.journal_skipped"
            );
        }
    }

    Ok((
        StatusCode::CREATED,
        Json(CollectionCreateResponse { collection_id }),
    )
        .into_response())
}

/// `op_kind` recorded in `operation_journal` for the `name` field
/// of `PATCH /collections/{id}` (ADR-0039).
///
/// Pre-state: `{ current: <prev_name>, intent: <new_name> }`.
/// The `name` field is `NOT NULL` so both sides are always
/// strings. Other field replayers (`canonical_name`, `audible_id`,
/// `description`, `kind`) carry `Option<String>` semantics — see
/// the per-field `OP_KIND_COLLECTION_*_SET` constants below.
pub const OP_KIND_COLLECTION_NAME_SET: &str = "collection-name-set";

/// `op_kind` for the `canonical_name` field of `PATCH /collections/{id}`.
/// Pre-state: `{ current: <prev|null>, intent: <new|null> }`.
pub const OP_KIND_COLLECTION_CANONICAL_NAME_SET: &str = "collection-canonical-name-set";

/// `op_kind` for the `audible_id` field of `PATCH /collections/{id}`.
///
/// Pre-state: `{ current: <prev|null>, intent: <new|null> }`. Note:
/// the partial UNIQUE index on `audible_id` means a replayer can
/// still fail with 409 if a different collection now claims the
/// target ASIN; the replayer reports `Skipped` in that case.
pub const OP_KIND_COLLECTION_AUDIBLE_ID_SET: &str = "collection-audible-id-set";

/// `op_kind` for the `description` field of `PATCH /collections/{id}`.
/// Pre-state: `{ current: <prev|null>, intent: <new|null> }`.
pub const OP_KIND_COLLECTION_DESCRIPTION_SET: &str = "collection-description-set";

/// `op_kind` for the `kind` field of `PATCH /collections/{id}`.
/// Pre-state: `{ current: <prev|null>, intent: <new|null> }`. Free
/// text today; pin to an enum once scanner + GUI usage settles.
pub const OP_KIND_COLLECTION_KIND_SET: &str = "collection-kind-set";

/// Body of `PATCH /api/v1/collections/{collection_id}`.
///
/// Every field is `Option<String>` — `None` (or absent) means
/// "leave untouched"; `Some(value)` means "update this field". An
/// empty body is rejected with `400 Bad Request`.
///
/// Empty-string values clear the corresponding nullable column
/// (`canonical_name`, `audible_id`, `description`, `kind`).
/// `name` is required to be non-empty after trim — passing empty
/// rejects with 400.
#[derive(Debug, Deserialize)]
pub struct CollectionPatchRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub canonical_name: Option<String>,
    #[serde(default)]
    pub audible_id: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
}

/// Response from `PATCH /api/v1/collections/{collection_id}`.
#[derive(Debug, Serialize)]
pub struct CollectionPatchResponse {
    pub collection_id: i64,
    /// Stable-order list of columns the request actually changed.
    pub updated: Vec<String>,
}

/// `PATCH /api/v1/collections/{collection_id}` — operator-curated
/// edit.
///
/// Body: [`CollectionPatchRequest`]. Returns `200 OK` with
/// [`CollectionPatchResponse`]. `404 Not Found` when no row at
/// that id; `400 Bad Request` for empty body OR an empty `name`;
/// `409 Conflict` when the UNIQUE constraint on `(name)` (or the
/// partial UNIQUE on `audible_id`) fires.
///
/// Journal capture: every present field gets its own
/// `operation_journal` row with a per-field `op_kind`
/// (`collection-name-set`, `collection-canonical-name-set`,
/// `collection-audible-id-set`, `collection-description-set`,
/// `collection-kind-set`). Each has a matching `Replayer`
/// registered in the daemon (`CollectionRenameReplayer`,
/// `CollectionCanonicalNameReplayer`, …). A multi-field PATCH
/// captures multiple journal rows that finalize together on tx
/// commit/rollback.
///
/// # Errors
///
/// Database access failures surface as [`ApiError::Internal`].
#[allow(clippy::missing_panics_doc, clippy::too_many_lines)] // panic-free; per-field update flow inline for top-to-bottom readability
pub async fn collections_patch(
    State(state): State<ApiState>,
    Path(collection_id): Path<i64>,
    Json(req): Json<CollectionPatchRequest>,
) -> Result<Response, ApiError> {
    let pool = state.inner.library.pool();

    // 0. Existence check (clean 404, not a transaction-rolled chain).
    let exists = sqlx::query_scalar!(
        r#"SELECT 1 AS "n!: i64" FROM book_collections WHERE collection_id = ?"#,
        collection_id,
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "collections_patch exists: {e}"
        )))
    })?
    .is_some();
    if !exists {
        return Err(ApiError::NotFound(format!("collection {collection_id}")));
    }

    // 1. Refuse empty body. Same posture as books_patch.
    let any_update = req.name.is_some()
        || req.canonical_name.is_some()
        || req.audible_id.is_some()
        || req.description.is_some()
        || req.kind.is_some();
    if !any_update {
        return Err(ApiError::BadRequest(
            "no fields to update; supply at least one field".to_owned(),
        ));
    }

    // 2. Validate the name (if present): trim, reject empty.
    let new_name: Option<String> = match req.name {
        Some(ref n) => {
            let trimmed = n.trim();
            if trimmed.is_empty() {
                return Err(ApiError::BadRequest("name cannot be empty".to_owned()));
            }
            Some(trimmed.to_owned())
        }
        None => None,
    };

    // 3. Per-field journal capture (ADR-0039). Each field that's
    //    present in the request gets pre-read + a `pending` journal
    //    row before the tx; the uniform finalize loop after the tx
    //    flips every captured row to `done` or `failed` atomically.
    //    Mirrors the books_patch shape.
    //
    //    Nullable fields (canonical_name / audible_id / description
    //    / kind) record their `current` + `intent` as either a JSON
    //    string OR `null` so a replayer can round-trip the
    //    Option<String> semantics losslessly.
    let mut journals: Vec<PendingJournal> = Vec::new();
    if let Some(intent) = new_name.as_deref() {
        let current: String = sqlx::query_scalar!(
            r#"SELECT name AS "name!: String" FROM book_collections WHERE collection_id = ?"#,
            collection_id,
        )
        .fetch_one(pool)
        .await
        .map_err(|e| {
            ApiError::Internal(ab_core::Error::Database(format!(
                "collections_patch name pre-read: {e}"
            )))
        })?;
        let entry = ab_journal::NewEntry {
            op_kind: OP_KIND_COLLECTION_NAME_SET,
            target: ab_journal::Target {
                kind: "collection".to_owned(),
                id: collection_id,
            },
            pre_state: serde_json::json!({ "current": current, "intent": intent }),
            reversible: true,
            batch_id: None,
        };
        let op_id = crate::journal_capture::record_pending(pool, &entry).await?;
        journals.push(PendingJournal {
            op_id,
            field_name: "name",
            intent: serde_json::Value::String(intent.to_owned()),
            context: "api.collections_patch.name",
        });
    }
    if let Some(raw) = req.canonical_name.as_deref() {
        let intent_opt = collapse_blank_to_none(raw);
        let current: Option<String> = sqlx::query_scalar!(
            r#"SELECT canonical_name AS "canonical_name?: String" FROM book_collections WHERE collection_id = ?"#,
            collection_id,
        )
        .fetch_one(pool)
        .await
        .map_err(|e| {
            ApiError::Internal(ab_core::Error::Database(format!(
                "collections_patch canonical_name pre-read: {e}"
            )))
        })?;
        let entry = ab_journal::NewEntry {
            op_kind: OP_KIND_COLLECTION_CANONICAL_NAME_SET,
            target: ab_journal::Target {
                kind: "collection".to_owned(),
                id: collection_id,
            },
            pre_state: serde_json::json!({ "current": current, "intent": intent_opt }),
            reversible: true,
            batch_id: None,
        };
        let op_id = crate::journal_capture::record_pending(pool, &entry).await?;
        journals.push(PendingJournal {
            op_id,
            field_name: "canonical_name",
            intent: optional_string_value(intent_opt.as_deref()),
            context: "api.collections_patch.canonical_name",
        });
    }
    if let Some(raw) = req.audible_id.as_deref() {
        let intent_opt = collapse_blank_to_none(raw);
        let current: Option<String> = sqlx::query_scalar!(
            r#"SELECT audible_id AS "audible_id?: String" FROM book_collections WHERE collection_id = ?"#,
            collection_id,
        )
        .fetch_one(pool)
        .await
        .map_err(|e| {
            ApiError::Internal(ab_core::Error::Database(format!(
                "collections_patch audible_id pre-read: {e}"
            )))
        })?;
        let entry = ab_journal::NewEntry {
            op_kind: OP_KIND_COLLECTION_AUDIBLE_ID_SET,
            target: ab_journal::Target {
                kind: "collection".to_owned(),
                id: collection_id,
            },
            pre_state: serde_json::json!({ "current": current, "intent": intent_opt }),
            reversible: true,
            batch_id: None,
        };
        let op_id = crate::journal_capture::record_pending(pool, &entry).await?;
        journals.push(PendingJournal {
            op_id,
            field_name: "audible_id",
            intent: optional_string_value(intent_opt.as_deref()),
            context: "api.collections_patch.audible_id",
        });
    }
    if let Some(raw) = req.description.as_deref() {
        let intent_opt = collapse_blank_to_none(raw);
        let current: Option<String> = sqlx::query_scalar!(
            r#"SELECT description AS "description?: String" FROM book_collections WHERE collection_id = ?"#,
            collection_id,
        )
        .fetch_one(pool)
        .await
        .map_err(|e| {
            ApiError::Internal(ab_core::Error::Database(format!(
                "collections_patch description pre-read: {e}"
            )))
        })?;
        let entry = ab_journal::NewEntry {
            op_kind: OP_KIND_COLLECTION_DESCRIPTION_SET,
            target: ab_journal::Target {
                kind: "collection".to_owned(),
                id: collection_id,
            },
            pre_state: serde_json::json!({ "current": current, "intent": intent_opt }),
            reversible: true,
            batch_id: None,
        };
        let op_id = crate::journal_capture::record_pending(pool, &entry).await?;
        journals.push(PendingJournal {
            op_id,
            field_name: "description",
            intent: optional_string_value(intent_opt.as_deref()),
            context: "api.collections_patch.description",
        });
    }
    if let Some(raw) = req.kind.as_deref() {
        let intent_opt = collapse_blank_to_none(raw);
        let current: Option<String> = sqlx::query_scalar!(
            r#"SELECT kind AS "kind?: String" FROM book_collections WHERE collection_id = ?"#,
            collection_id,
        )
        .fetch_one(pool)
        .await
        .map_err(|e| {
            ApiError::Internal(ab_core::Error::Database(format!(
                "collections_patch kind pre-read: {e}"
            )))
        })?;
        let entry = ab_journal::NewEntry {
            op_kind: OP_KIND_COLLECTION_KIND_SET,
            target: ab_journal::Target {
                kind: "collection".to_owned(),
                id: collection_id,
            },
            pre_state: serde_json::json!({ "current": current, "intent": intent_opt }),
            reversible: true,
            batch_id: None,
        };
        let op_id = crate::journal_capture::record_pending(pool, &entry).await?;
        journals.push(PendingJournal {
            op_id,
            field_name: "kind",
            intent: optional_string_value(intent_opt.as_deref()),
            context: "api.collections_patch.kind",
        });
    }

    // 4. Apply the updates in a transaction.
    let mut updated: Vec<String> = Vec::new();
    let tx_result: Result<(), sqlx::Error> = async {
        let mut tx = pool.begin().await?;
        if let Some(v) = new_name.as_deref() {
            sqlx::query!(
                "UPDATE book_collections SET name = ? WHERE collection_id = ?",
                v,
                collection_id
            )
            .execute(&mut *tx)
            .await?;
            updated.push("name".to_owned());
        }
        if let Some(v) = req.canonical_name.as_deref() {
            let store = collapse_blank_to_none(v);
            sqlx::query!(
                "UPDATE book_collections SET canonical_name = ? WHERE collection_id = ?",
                store,
                collection_id
            )
            .execute(&mut *tx)
            .await?;
            updated.push("canonical_name".to_owned());
        }
        if let Some(v) = req.audible_id.as_deref() {
            let store = collapse_blank_to_none(v);
            sqlx::query!(
                "UPDATE book_collections SET audible_id = ? WHERE collection_id = ?",
                store,
                collection_id
            )
            .execute(&mut *tx)
            .await?;
            updated.push("audible_id".to_owned());
        }
        if let Some(v) = req.description.as_deref() {
            let store = collapse_blank_to_none(v);
            sqlx::query!(
                "UPDATE book_collections SET description = ? WHERE collection_id = ?",
                store,
                collection_id
            )
            .execute(&mut *tx)
            .await?;
            updated.push("description".to_owned());
        }
        if let Some(v) = req.kind.as_deref() {
            let store = collapse_blank_to_none(v);
            sqlx::query!(
                "UPDATE book_collections SET kind = ? WHERE collection_id = ?",
                store,
                collection_id
            )
            .execute(&mut *tx)
            .await?;
            updated.push("kind".to_owned());
        }
        // Stamp updated_at on every successful patch — no Option
        // branch needed because we've already proven at least one
        // field is being touched.
        sqlx::query!(
            "UPDATE book_collections SET updated_at = strftime('%s','now') \
             WHERE collection_id = ?",
            collection_id
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
    .await;

    // 5. Finalize each captured journal row (best-effort) and map
    //    UNIQUE-conflict failures to 409.
    finalize_pending_journals(pool, journals, &tx_result).await;

    match tx_result {
        Ok(()) => Ok((
            StatusCode::OK,
            Json(CollectionPatchResponse {
                collection_id,
                updated,
            }),
        )
            .into_response()),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("UNIQUE constraint failed") {
                let column = if msg.contains("book_collections.name") {
                    "name"
                } else if msg.contains("book_collections.audible_id") {
                    "audible_id"
                } else {
                    "unique"
                };
                return Err(ApiError::Conflict(format!(
                    "collection {column} already exists"
                )));
            }
            Err(ApiError::Internal(ab_core::Error::Database(format!(
                "collections_patch tx: {e}"
            ))))
        }
    }
}

/// Map an `Option<&str>` into the JSON value the journal stores
/// for nullable-string intent / post-state fields. `Some(s)` →
/// JSON string; `None` → JSON null.
fn optional_string_value(v: Option<&str>) -> serde_json::Value {
    v.map_or(serde_json::Value::Null, |s| {
        serde_json::Value::String(s.to_owned())
    })
}

/// One pending `operation_journal` row captured by
/// [`collections_patch`] before the tx. Aggregated into a `Vec`
/// so the post-tx finalize pass marks every row `done` (on
/// commit) or `failed` (on rollback) atomically — no
/// partial-state where some fields land `done` and others stay
/// `pending`. Same shape as the `PendingJournal` in `router.rs`
/// for `PATCH /books`.
struct PendingJournal {
    op_id: i64,
    /// Column name (`"name"`, `"canonical_name"`, …). Used as
    /// the `post_state` JSON key on success so the rendered row
    /// reads `{ <field>: <intent> }`.
    field_name: &'static str,
    /// Intent value recorded in `pre_state.intent`. For `name`
    /// it's always a JSON string; for the nullable fields it's
    /// either a JSON string or `null`.
    intent: serde_json::Value,
    /// Tracing context string for `mark_done_or_log` /
    /// `mark_failed_or_log` (`"api.collections_patch.<field>"`).
    context: &'static str,
}

/// Flip every captured pending journal row to `done` (on commit)
/// or `failed` (on rollback). Best-effort per
/// [`crate::journal_capture`] semantics — a row left `pending`
/// here gets reaped by the next daemon-startup recovery pass.
async fn finalize_pending_journals(
    pool: &sqlx::SqlitePool,
    journals: Vec<PendingJournal>,
    tx_result: &Result<(), sqlx::Error>,
) {
    for j in journals {
        match tx_result {
            Ok(()) => {
                let mut post = serde_json::Map::new();
                post.insert(j.field_name.to_owned(), j.intent);
                crate::journal_capture::mark_done_or_log(
                    pool,
                    j.op_id,
                    &serde_json::Value::Object(post),
                    j.context,
                )
                .await;
            }
            Err(e) => {
                crate::journal_capture::mark_failed_or_log(
                    pool,
                    j.op_id,
                    &format!("collections_patch commit failed: {e}"),
                    j.context,
                )
                .await;
            }
        }
    }
}

/// Trim + treat empty string as NULL for the nullable string
/// columns. The non-nullable `name` column has its own validation
/// path; this helper only applies to `canonical_name`, `audible_id`,
/// `description`, `kind`.
fn collapse_blank_to_none(v: &str) -> Option<String> {
    let t = v.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_owned())
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn collection_detail_serializes_with_expected_keys() {
        let d = CollectionDetail {
            collection_id: 9,
            name: "The Stormlight Box Set".into(),
            canonical_name: Some("stormlight-box-set".into()),
            audible_id: Some("B0BOXSET01".into()),
            description: Some("Books 1-4 of the cosmere sequence.".into()),
            kind: Some("box_set".into()),
            book_count: 4,
            created_at: 1_700_000_000,
            updated_at: 1_770_000_000,
        };
        let json = serde_json::to_value(&d).unwrap();
        assert_eq!(json["collection_id"], 9);
        assert_eq!(json["name"], "The Stormlight Box Set");
        assert_eq!(json["canonical_name"], "stormlight-box-set");
        assert_eq!(json["audible_id"], "B0BOXSET01");
        assert_eq!(json["kind"], "box_set");
        assert_eq!(json["book_count"], 4);
    }

    #[test]
    fn collection_detail_preserves_null_optional_fields() {
        let d = CollectionDetail {
            collection_id: 1,
            name: "Unnamed".into(),
            canonical_name: None,
            audible_id: None,
            description: None,
            kind: None,
            book_count: 0,
            created_at: 0,
            updated_at: 0,
        };
        let json = serde_json::to_value(&d).unwrap();
        for key in ["canonical_name", "audible_id", "description", "kind"] {
            assert!(json.get(key).is_some());
            assert!(json[key].is_null());
        }
    }

    #[test]
    fn sort_parses_documented_keys() {
        assert_eq!(CollectionsSort::parse(None).unwrap(), CollectionsSort::Name);
        assert_eq!(
            CollectionsSort::parse(Some("")).unwrap(),
            CollectionsSort::Name
        );
        assert_eq!(
            CollectionsSort::parse(Some("name")).unwrap(),
            CollectionsSort::Name
        );
        assert_eq!(
            CollectionsSort::parse(Some("book_count")).unwrap(),
            CollectionsSort::BookCountDesc
        );
    }

    #[test]
    fn sort_rejects_unknown_with_400() {
        match CollectionsSort::parse(Some("kind")) {
            Err(ApiError::BadRequest(msg)) => {
                assert!(msg.contains("kind"));
                assert!(msg.contains("book_count"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn collections_list_response_serializes_with_pagination_keys() {
        let r = CollectionsListResponse {
            collections: vec![CollectionListItem {
                collection_id: 9,
                name: "The Stormlight Box Set".into(),
                canonical_name: Some("stormlight-box-set".into()),
                audible_id: Some("B0BOXSET01".into()),
                kind: Some("box_set".into()),
                book_count: 4,
            }],
            total: 42,
            limit: 50,
            offset: 0,
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["total"], 42);
        assert_eq!(json["limit"], 50);
        assert_eq!(json["offset"], 0);
        let items = json["collections"]
            .as_array()
            .expect("collections is array");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["collection_id"], 9);
        assert_eq!(items[0]["kind"], "box_set");
        assert_eq!(items[0]["book_count"], 4);
    }
}
