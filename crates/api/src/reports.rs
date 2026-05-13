//! Library-comparison reports (slice 10E + 10F, Cluster 5).
//!
//! Both endpoints in this module read an authoritative source
//! (Audible's catalog) and compare it against the local DB to
//! surface gaps + upcoming releases. The shared shape:
//!
//! 1. Fetch a list of books from Audible for a given identity
//!    (today: author).
//! 2. Mark each as `owned` (already in our `books` table — match
//!    on `asin`) or `new` / `pre-order` (not in our DB).
//! 3. Filter / sort and return.
//!
//! Network failures bubble as `Internal`; an empty result is a
//! legitimate response (200 with `[]`).
//!
//! A future follow-up will feed the "books in Audible's list but
//! not in our DB" deltas back into the H.3 disambiguation surface
//! (BACKLOG.md § Cluster 5). For 10E + 10F the surface is
//! report-only.

use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};

use ab_catalog::AudibleClient;
use ab_core::Tunables;

use crate::error::ApiError;
use crate::state::ApiState;

/// Status of a candidate book relative to the local library.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BookStatus {
    /// Already in our `books` table (matched by ASIN).
    Owned,
    /// Not in our DB; `release_date` is in the past.
    New,
    /// Not in our DB; `release_date` is in the future.
    PreOrder,
}

/// One row in the gaps / upcoming responses. Same shape so the
/// CLI can render both with the same code path.
#[derive(Debug, Clone, Serialize)]
pub struct BookCandidate {
    pub asin: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    pub status: BookStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_min: Option<u32>,
    pub authors: Vec<String>,
    pub narrators: Vec<String>,
}

/// Query for `GET /report/gaps`.
#[derive(Deserialize)]
pub struct GapsQuery {
    /// Author row ID. Required.
    pub author: i64,
    /// Cap on Audible result pages (50 books per page).
    /// Default 5 = up to 250 books.
    #[serde(default)]
    pub max_pages: Option<u32>,
}

#[derive(Serialize)]
pub struct GapsResponse {
    pub author_id: i64,
    pub author_name: String,
    pub owned_count: u64,
    pub gap_count: u64,
    pub books: Vec<BookCandidate>,
}

/// `GET /api/v1/report/gaps?author=<id>&max_pages=<N>` —
/// "every book Audible thinks this author wrote, marked as
/// owned vs. gap."
pub async fn report_gaps(
    State(state): State<ApiState>,
    Query(q): Query<GapsQuery>,
) -> Result<Json<GapsResponse>, ApiError> {
    let author_row = sqlx::query!(
        "SELECT name AS \"name!\", audible_id FROM authors WHERE author_id = ?",
        q.author,
    )
    .fetch_optional(state.inner.library.pool())
    .await
    .map_err(|e| ab_core::Error::Database(format!("report.gaps author lookup: {e}")))?;
    let Some(author_row) = author_row else {
        return Err(ApiError::NotFound(format!("author {} not found", q.author)));
    };

    let tunables = Tunables::default();
    let client = AudibleClient::new(&tunables.http_client);
    let max_pages = q.max_pages.unwrap_or(5);
    let products = client
        .list_books_by_author(&author_row.name, max_pages)
        .await?;

    let books = annotate_against_library(&state, products).await?;
    let owned_count = books
        .iter()
        .filter(|b| matches!(b.status, BookStatus::Owned))
        .count() as u64;
    let gap_count = books
        .iter()
        .filter(|b| !matches!(b.status, BookStatus::Owned))
        .count() as u64;
    Ok(Json(GapsResponse {
        author_id: q.author,
        author_name: author_row.name,
        owned_count,
        gap_count,
        books,
    }))
}

/// Query for `GET /upcoming`.
#[derive(Deserialize)]
pub struct UpcomingQuery {
    /// Window in days from today. Default 180; max 730 (~2y).
    #[serde(default)]
    pub days: Option<u32>,
    /// Restrict to one author. Absent → iterate every author in
    /// the library (`SELECT author_id FROM authors`).
    #[serde(default)]
    pub author: Option<i64>,
    /// Cap on Audible result pages per author. Default 3.
    #[serde(default)]
    pub max_pages: Option<u32>,
}

#[derive(Serialize)]
pub struct UpcomingResponse {
    pub days_window: u32,
    pub authors_checked: u64,
    pub books: Vec<BookCandidate>,
}

/// `GET /api/v1/upcoming?days=N` — upcoming releases by every
/// library author (or one if `author=<id>` provided).
pub async fn report_upcoming(
    State(state): State<ApiState>,
    Query(q): Query<UpcomingQuery>,
) -> Result<Json<UpcomingResponse>, ApiError> {
    let days = q.days.unwrap_or(180).min(730);
    let max_pages = q.max_pages.unwrap_or(3);
    let cutoff = compute_cutoff_date(days);
    let today = compute_today_date();

    let author_ids: Vec<(i64, String)> = if let Some(aid) = q.author {
        let row: Option<(i64, String)> =
            sqlx::query_as("SELECT author_id, name FROM authors WHERE author_id = ?")
                .bind(aid)
                .fetch_optional(state.inner.library.pool())
                .await
                .map_err(|e| ab_core::Error::Database(format!("upcoming author lookup: {e}")))?;
        row.map(|r| vec![r]).unwrap_or_default()
    } else {
        sqlx::query_as("SELECT author_id, name FROM authors ORDER BY name")
            .fetch_all(state.inner.library.pool())
            .await
            .map_err(|e| ab_core::Error::Database(format!("upcoming authors list: {e}")))?
    };
    let authors_checked = author_ids.len() as u64;

    let tunables = Tunables::default();
    let client = AudibleClient::new(&tunables.http_client);
    let mut all_candidates: Vec<BookCandidate> = Vec::new();
    for (_aid, name) in &author_ids {
        let products = match client.list_books_by_author(name, max_pages).await {
            Ok(p) => p,
            Err(e) => {
                // Per-author failure shouldn't abort the whole sweep —
                // operator gets a partial list rather than an error
                // for the whole report.
                tracing::warn!(author = %name, error = %e, "api.upcoming.author_failed");
                continue;
            }
        };
        let annotated = annotate_against_library(&state, products).await?;
        for b in annotated {
            if matches!(b.status, BookStatus::Owned) {
                continue;
            }
            if !release_in_window(b.release_date.as_deref(), &today, &cutoff) {
                continue;
            }
            all_candidates.push(b);
        }
    }
    Ok(Json(UpcomingResponse {
        days_window: days,
        authors_checked,
        books: all_candidates,
    }))
}

/// Convert `AudibleProduct` rows to `BookCandidate` rows + flag
/// which are already in `books` (match by ASIN, case-sensitive).
async fn annotate_against_library(
    state: &ApiState,
    products: Vec<ab_catalog::audible::AudibleProduct>,
) -> Result<Vec<BookCandidate>, ApiError> {
    if products.is_empty() {
        return Ok(Vec::new());
    }
    // One IN-list query against `books.asin` so we don't N+1.
    let asins: Vec<String> = products.iter().map(|p| p.asin.clone()).collect();
    let placeholders = std::iter::repeat_n("?", asins.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("SELECT asin FROM books WHERE asin IN ({placeholders})");
    let mut q = sqlx::query_scalar::<_, String>(&sql);
    for a in &asins {
        q = q.bind(a);
    }
    let owned: Vec<String> = q
        .fetch_all(state.inner.library.pool())
        .await
        .map_err(|e| ab_core::Error::Database(format!("report owned-asin lookup: {e}")))?;
    let owned: std::collections::HashSet<String> = owned.into_iter().collect();

    let today = compute_today_date();
    let out: Vec<BookCandidate> = products
        .into_iter()
        .map(|p| {
            let status = if owned.contains(&p.asin) {
                BookStatus::Owned
            } else if p
                .release_date
                .as_deref()
                .is_some_and(|d| d.as_bytes() > today.as_bytes())
            {
                BookStatus::PreOrder
            } else {
                BookStatus::New
            };
            BookCandidate {
                asin: p.asin,
                title: p.title,
                subtitle: p.subtitle,
                status,
                release_date: p.release_date,
                runtime_min: p.runtime_length_min,
                authors: p.authors.iter().map(|c| c.name.clone()).collect(),
                narrators: p.narrators.iter().map(|c| c.name.clone()).collect(),
            }
        })
        .collect();
    Ok(out)
}

/// "YYYY-MM-DD" today in UTC. Audible's `release_date` format is
/// ISO-style yyyy-mm-dd which sorts lexicographically; we compare
/// as bytes.
fn compute_today_date() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let date = chrono::DateTime::<chrono::Utc>::from_timestamp(i64::try_from(now).unwrap_or(0), 0)
        .unwrap_or_else(chrono::Utc::now);
    date.format("%Y-%m-%d").to_string()
}

fn compute_cutoff_date(days_window: u32) -> String {
    let cutoff = chrono::Utc::now() + chrono::Duration::days(i64::from(days_window));
    cutoff.format("%Y-%m-%d").to_string()
}

fn release_in_window(release_date: Option<&str>, today: &str, cutoff: &str) -> bool {
    let Some(rd) = release_date else {
        return false;
    };
    rd.as_bytes() > today.as_bytes() && rd.as_bytes() <= cutoff.as_bytes()
}
