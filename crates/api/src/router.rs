//! Top-level axum Router builder.

use std::path::PathBuf;

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::state::ApiState;

/// Build the native API router. Mount at `/api/v1`.
pub fn build_router(state: ApiState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/library/scan", post(library_scan))
        .route("/books", get(books_list))
        .with_state(state)
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
    uptime_secs: u64,
    app: &'static str,
    version: &'static str,
}

async fn health(State(state): State<ApiState>) -> Json<Health> {
    let uptime = state.inner.started_at.elapsed().as_secs();
    Json(Health {
        status: "ok",
        uptime_secs: uptime,
        app: ab_core::build_info::APP_NAME,
        version: ab_core::build_info::VERSION,
    })
}

#[derive(Serialize)]
struct VersionInfo {
    name: &'static str,
    #[serde(rename = "version")]
    semver: &'static str,
    description: &'static str,
}

async fn version() -> Json<VersionInfo> {
    Json(VersionInfo {
        name: ab_core::build_info::APP_NAME,
        semver: ab_core::build_info::VERSION,
        description: ab_core::build_info::DESCRIPTION,
    })
}

/// Request body for `POST /library/scan`.
#[derive(Deserialize)]
struct ScanRequest {
    /// Filesystem path to scan recursively. Must exist + be readable.
    path: PathBuf,
}

/// Response body for `POST /library/scan`. Mirrors
/// `ab_scan::ScanReport` with paths stringified for JSON.
#[derive(Serialize)]
struct ScanResponse {
    new_book_ids: Vec<i64>,
    skipped_paths: Vec<String>,
    non_audio_count: u64,
    total_walked: u64,
}

async fn library_scan(
    State(state): State<ApiState>,
    Json(req): Json<ScanRequest>,
) -> Result<Json<ScanResponse>, ApiError> {
    let report = ab_scan::scan(&req.path, &state.inner.library).await?;

    // Submit each newly-discovered book to the scheduler for
    // downstream pipeline work (tag-read in slice 1B; more stages
    // wire in here later). Priority::Interactive — scan is a
    // user-initiated request, should preempt background drainage.
    for book_id in &report.new_book_ids {
        if let Err(e) = state
            .inner
            .scheduler
            .submit(*book_id, "tag-read", ab_pipeline::Priority::Interactive)
            .await
        {
            tracing::warn!(
                book = %book_id,
                error = %e,
                "scan.scheduler_submit_failed"
            );
        }
    }

    Ok(Json(ScanResponse {
        new_book_ids: report.new_book_ids.into_iter().map(|b| b.0).collect(),
        skipped_paths: report
            .skipped_paths
            .into_iter()
            .map(|p| p.display().to_string())
            .collect(),
        non_audio_count: report.non_audio_count,
        total_walked: report.total_walked,
    }))
}

/// One row of `GET /books`. Minimal columns for slice 1A; expands
/// in 1B once `tag-read` fills in author/duration/etc.
#[derive(Serialize)]
struct BookRow {
    book_id: i64,
    title: String,
    file_path: Option<String>,
}

#[derive(Serialize)]
struct BooksResponse {
    books: Vec<BookRow>,
}

async fn books_list(State(state): State<ApiState>) -> Result<Json<BooksResponse>, ApiError> {
    let rows: Vec<(i64, String, Option<String>)> = sqlx::query_as(
        "SELECT b.book_id, b.title, \
                (SELECT file_path FROM book_files WHERE book_id = b.book_id LIMIT 1) AS file_path \
         FROM books b \
         ORDER BY b.book_id",
    )
    .fetch_all(state.inner.library.pool())
    .await
    .map_err(|e| ab_core::Error::Database(format!("books list: {e}")))?;

    let books = rows
        .into_iter()
        .map(|(book_id, title, file_path)| BookRow {
            book_id,
            title,
            file_path,
        })
        .collect();
    Ok(Json(BooksResponse { books }))
}
