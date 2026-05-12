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
        .route("/library/duplicates", get(library_duplicates))
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
    // Submit each new BookId to every per-book stage. Stages run
    // concurrently (their DAG dependency lists are empty in slices
    // 1B/1C); each completes independently.
    // (stage, priority) pairs. Most stages run at Interactive
    // because scan is user-initiated; full-book transcribe is
    // expensive enough to deserve its own Idle tier so it never
    // competes with the import-time pipeline. See PROJECT.md
    // "Pipeline priorities."
    let stage_priorities: &[(&'static str, ab_pipeline::Priority)] = &[
        ("tag-read", ab_pipeline::Priority::Interactive),
        ("fingerprint", ab_pipeline::Priority::Interactive),
        ("audible-search", ab_pipeline::Priority::Interactive),
        ("audnexus-enrich", ab_pipeline::Priority::Interactive),
        ("consensus", ab_pipeline::Priority::Interactive),
        ("identity-resolve", ab_pipeline::Priority::Interactive),
        ("audnexus-chapters", ab_pipeline::Priority::Interactive),
        ("embedded-chapters", ab_pipeline::Priority::Interactive),
        ("chapter-pick-winner", ab_pipeline::Priority::Interactive),
        // 6-min head + 30-s tail. Heavier than the other
        // stages (multi-second per book at decode +
        // SpeechAnalyzer time) but seeded at scan time so the
        // language gate + downstream extractors have a
        // transcript by the time the user opens the book.
        ("transcribe-head-tail", ab_pipeline::Priority::Interactive),
        // Transcript extractors — cheap pure-text heuristics
        // over the head transcript; runs at Interactive so the
        // user sees its candidates by the time the book opens.
        (
            "run-transcript-extractors",
            ab_pipeline::Priority::Interactive,
        ),
        // Whole-book transcribe — drains during quiet periods,
        // not in the import-time pipeline.
        ("transcribe-full", ab_pipeline::Priority::Idle),
    ];
    for book_id in &report.new_book_ids {
        for (stage, priority) in stage_priorities {
            if let Err(e) = state
                .inner
                .scheduler
                .submit(*book_id, stage, *priority)
                .await
            {
                tracing::warn!(
                    book = %book_id,
                    stage,
                    error = %e,
                    "scan.scheduler_submit_failed"
                );
            }
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

/// One group of books with matching fingerprints (same recording).
#[derive(Serialize)]
struct DuplicateGroup {
    /// Number of offsets at which all members agree exactly.
    /// 4 offsets agreeing → very-high-confidence match.
    matching_offsets: u32,
    book_ids: Vec<i64>,
    titles: Vec<String>,
}

#[derive(Serialize)]
struct DuplicatesResponse {
    groups: Vec<DuplicateGroup>,
}

/// Group books with identical chromaprint windows at the same offset.
/// Slice 1C only: exact match. Fuzzy matching (Hamming distance
/// < `MATCH_HD`) is a follow-up.
async fn library_duplicates(
    State(state): State<ApiState>,
) -> Result<Json<DuplicatesResponse>, ApiError> {
    let rows = sqlx::query!(
        "SELECT book_id, offset_sec, fingerprint FROM book_fingerprints \
         ORDER BY offset_sec, fingerprint",
    )
    .fetch_all(state.inner.library.pool())
    .await
    .map_err(|e| ab_core::Error::Database(format!("dups query: {e}")))?;

    // (offset_sec, fingerprint) -> list of book_ids that share it.
    let mut by_fp: std::collections::HashMap<(i64, Vec<u8>), Vec<i64>> =
        std::collections::HashMap::new();
    for r in rows {
        by_fp
            .entry((r.offset_sec, r.fingerprint))
            .or_default()
            .push(r.book_id);
    }

    // Sorted-deduped book_id set → count of offsets agreeing.
    let mut group_counts: std::collections::HashMap<Vec<i64>, u32> =
        std::collections::HashMap::new();
    for (_, book_ids) in by_fp {
        if book_ids.len() < 2 {
            continue;
        }
        let mut sorted = book_ids;
        sorted.sort_unstable();
        sorted.dedup();
        *group_counts.entry(sorted).or_default() += 1;
    }

    let mut groups = Vec::with_capacity(group_counts.len());
    for (book_ids, matching_offsets) in group_counts {
        let placeholders = vec!["?"; book_ids.len()].join(",");
        let sql = format!(
            "SELECT book_id, title FROM books WHERE book_id IN ({placeholders}) ORDER BY book_id"
        );
        let mut q = sqlx::query_as::<_, (i64, String)>(&sql);
        for id in &book_ids {
            q = q.bind(*id);
        }
        let title_rows = q
            .fetch_all(state.inner.library.pool())
            .await
            .map_err(|e| ab_core::Error::Database(format!("dups titles: {e}")))?;
        let titles = title_rows.into_iter().map(|(_, t)| t).collect();

        groups.push(DuplicateGroup {
            matching_offsets,
            book_ids,
            titles,
        });
    }
    groups.sort_by_key(|g| std::cmp::Reverse(g.matching_offsets));

    Ok(Json(DuplicatesResponse { groups }))
}

async fn books_list(State(state): State<ApiState>) -> Result<Json<BooksResponse>, ApiError> {
    // `book_id!` forces non-null inference past sqlite's
    // `INTEGER PRIMARY KEY AUTOINCREMENT` nullability quirk
    // (see slice 1D.2 note).
    let rows = sqlx::query!(
        r#"SELECT b.book_id AS "book_id!", b.title,
                  (SELECT file_path FROM book_files
                   WHERE book_id = b.book_id LIMIT 1) AS file_path
           FROM books b
           ORDER BY b.book_id"#,
    )
    .fetch_all(state.inner.library.pool())
    .await
    .map_err(|e| ab_core::Error::Database(format!("books list: {e}")))?;

    let books = rows
        .into_iter()
        .map(|r| BookRow {
            book_id: r.book_id,
            title: r.title,
            file_path: r.file_path,
        })
        .collect();
    Ok(Json(BooksResponse { books }))
}
