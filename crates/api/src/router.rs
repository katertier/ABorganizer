//! Top-level axum Router builder.

use std::path::{Path as FsPath, PathBuf};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::audiologo_apply::{ApplyCutParams, apply_audiologo_cut};
use crate::error::ApiError;
use crate::state::ApiState;

/// Build the native API router. Mount at `/api/v1`.
#[allow(
    clippy::too_many_lines,
    reason = "single linear route table; splitting only obscures the URI list"
)]
pub fn build_router(state: ApiState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/library/scan", post(library_scan))
        .route("/library/duplicates", get(library_duplicates))
        .route(
            "/library_roots",
            get(crate::library_roots::library_roots_list)
                .post(crate::library_roots::library_roots_create),
        )
        .route(
            "/library_roots/{root_id}",
            delete(crate::library_roots::library_roots_delete),
        )
        .route(
            "/tokens",
            get(crate::tokens::tokens_list).post(crate::tokens::tokens_create),
        )
        .route("/tokens/{token_id}", delete(crate::tokens::tokens_revoke))
        .route(
            "/pairing/codes",
            get(crate::pairing::pairing_codes_list).post(crate::pairing::pairing_codes_create),
        )
        .route(
            "/pairing/codes/{code_id}",
            delete(crate::pairing::pairing_codes_revoke),
        )
        // Public — by design. See crate::auth::PUBLIC_PATHS + the
        // crate::pairing module docs for the brute-force analysis.
        .route("/pairing/consume", post(crate::pairing::pairing_consume))
        .route(
            "/library/pending_speech_installs",
            get(library_pending_speech_installs),
        )
        .route(
            "/library/pending_speech_installs/retry",
            post(library_retry_failed_speech_installs),
        )
        .route("/doctor/speech", get(doctor_speech))
        .route("/doctor/speech/install", post(doctor_speech_install))
        .route("/doctor", get(crate::doctor::doctor_index))
        .route("/doctor/all", get(crate::doctor::doctor_all))
        .route("/doctor/{name}", get(crate::doctor::doctor_one))
        .route("/books", get(books_list))
        .route(
            "/books/{book_id}",
            get(books_get).patch(books_patch).delete(books_delete),
        )
        .route("/books/{book_id}/restore", post(books_restore))
        .route("/books/{book_id}/retry", post(books_retry_stage))
        .route("/books/{book_id}/audiologo", post(books_audiologo_cut))
        .route(
            "/books/{book_id}/status",
            axum::routing::patch(crate::progress::books_status_patch),
        )
        .route(
            "/books/{book_id}/rating",
            axum::routing::patch(crate::progress::books_rating_patch),
        )
        .route(
            "/books/{book_id}/notes",
            axum::routing::patch(crate::progress::books_notes_patch),
        )
        .route(
            "/books/{book_id}/progress",
            get(crate::progress::books_progress_get),
        )
        .route(
            "/session/{book_id}/sync",
            post(crate::progress::session_sync),
        )
        .route(
            "/audiologos/review",
            get(crate::audiologo_review::audiologos_review_list),
        )
        .route(
            "/audiologos/{row_id}/approve",
            post(crate::audiologo_review::audiologos_approve),
        )
        .route(
            "/audiologos/{row_id}/reject",
            post(crate::audiologo_review::audiologos_reject),
        )
        .route("/clean/usage", get(clean_usage))
        .route("/clean/run", post(clean_run))
        .route(
            "/background/tasks",
            get(crate::background::background_tasks_list),
        )
        .route(
            "/background/tasks/{name}/run",
            post(crate::background::background_task_run),
        )
        .route("/names/{kind}/{id}/alias", post(crate::names::names_alias))
        .route("/names/{kind}/{id}/exalt", post(crate::names::names_exalt))
        .route("/names/pending", get(crate::names::names_pending_list))
        .route(
            "/names/pending/{pending_id}/resolve",
            post(crate::names::names_pending_resolve),
        )
        .route("/report/gaps", get(crate::reports::report_gaps))
        .route("/upcoming", get(crate::reports::report_upcoming))
        // Auth middleware applied to ALL routes. The middleware
        // itself checks `crate::auth::PUBLIC_PATHS` to bypass
        // `/health` and `/version`; everything else needs a
        // bearer token that matches an active row in the
        // `tokens` table (or the deprecated `admin_token` tunable
        // when tokens table is still empty, as a one-cycle
        // bootstrap compat).
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::require_token,
        ))
        .with_state(state)
}

// ── Cleanup (slice H.2.3, ADR-0025) ────────────────────────────────

/// Optional `?category=disk|db|queue` filter on `GET /clean/usage`.
#[derive(Deserialize)]
struct CleanUsageQuery {
    /// Restrict the dry-run sweep to one category. Absent → every
    /// registered target reports.
    category: Option<String>,
}

/// Per-target row in the cleanup response. Mirrors
/// [`ab_core::cleanup::CleanupReport`] with the category serialized
/// as its lowercase wire form.
#[derive(Serialize)]
struct CleanReportRow {
    category: String,
    name: String,
    items: u64,
    bytes: u64,
}

impl From<ab_core::cleanup::CleanupReport> for CleanReportRow {
    fn from(r: ab_core::cleanup::CleanupReport) -> Self {
        Self {
            category: r.category.to_string(),
            name: r.name,
            items: r.items,
            bytes: r.bytes,
        }
    }
}

#[derive(Serialize)]
struct CleanUsageResponse {
    /// Effective age cut-off (seconds) — same value targets see in
    /// their `Policy.age_seconds`. Surfaces what the disk-pressure
    /// ratchet picked so operators can see whether any tier
    /// triggered.
    age_seconds: i64,
    /// Per-target reports in registration order.
    targets: Vec<CleanReportRow>,
}

/// `GET /api/v1/clean/usage` — dry-run sweep across every registered
/// cleanup target (or just one category when `?category=` is set).
///
/// No state mutation. The response mirrors the `aborg clean`
/// summary the CLI prints. Disk pressure is computed from
/// `(u64::MAX, u64::MAX)` — the API surface intentionally doesn't
/// re-stat the disk per request; the periodic loop is the source of
/// truth for ratchet activation. Operators who want to force a
/// tighter age can pass `force=true` on `POST /clean/run`.
async fn clean_usage(
    State(state): State<ApiState>,
    axum::extract::Query(q): axum::extract::Query<CleanUsageQuery>,
) -> Result<Json<CleanUsageResponse>, ApiError> {
    let category = match q.category.as_deref() {
        None => None,
        Some(s) => Some(ab_core::cleanup::Category::parse(s).ok_or_else(|| {
            ApiError::BadRequest(format!("unknown category `{s}` (valid: disk, db, queue)"))
        })?),
    };
    let tunables = ab_core::tunables::CleanupTunables::default();
    let age_seconds = ab_core::cleanup::compute_age_seconds(&tunables, u64::MAX, u64::MAX);
    let policy = ab_core::cleanup::Policy::dry_run(age_seconds);
    let cleanup_ctx = ab_pipeline::cleanup::CleanupCtx {
        library: state.inner.library.clone(),
        ephemeral: state.inner.ephemeral.clone(),
    };
    let mut rows: Vec<CleanReportRow> = Vec::new();
    for target in state.inner.cleanup.iter() {
        if let Some(want) = category {
            if target.category() != want {
                continue;
            }
        }
        match target.report(&cleanup_ctx, &policy).await {
            Ok(r) => rows.push(r.into()),
            Err(e) => {
                tracing::warn!(
                    target = target.name(),
                    error = %e,
                    "api.clean.report_failed"
                );
                rows.push(CleanReportRow {
                    category: target.category().to_string(),
                    name: target.name().to_owned(),
                    items: 0,
                    bytes: 0,
                });
            }
        }
    }
    Ok(Json(CleanUsageResponse {
        age_seconds,
        targets: rows,
    }))
}

/// Body of `POST /api/v1/clean/run`.
///
/// Per ADR-0029 § "Exactly one opt-out flag": `commit` is the
/// verb-shaped opt-in to actually delete. Default `false` keeps
/// mutating commands dry-run by default; the CLI surface
/// (`aborg clean … --commit`) is the operator-facing form.
#[derive(Deserialize)]
struct CleanRunRequest {
    /// One of `disk`, `db`, `queue`. Required.
    category: String,
    /// `true` → actually delete; `false` (default) → dry-run.
    /// Renamed from `apply` in slice #87 to align with ADR-0029's
    /// single-opt-out-flag rule.
    #[serde(default)]
    commit: bool,
    /// `true` → ignore the age gate for every target (per-target
    /// semantics in their docstrings). Per ADR-0029 § "second
    /// tier", `force` only relaxes safety checks; it does not
    /// enable mutation on its own — `commit` is the actual
    /// "yes, delete" verb.
    #[serde(default)]
    force: bool,
}

#[derive(Serialize)]
struct CleanRunResponse {
    category: String,
    commit: bool,
    force: bool,
    age_seconds: i64,
    targets: Vec<CleanReportRow>,
}

/// `POST /api/v1/clean/run` — operator-triggered cleanup for one
/// category. `commit=true` switches each target into delete mode;
/// `commit=false` is identical to `GET /clean/usage?category=…`.
async fn clean_run(
    State(state): State<ApiState>,
    Json(req): Json<CleanRunRequest>,
) -> Result<Json<CleanRunResponse>, ApiError> {
    let category = ab_core::cleanup::Category::parse(&req.category).ok_or_else(|| {
        ApiError::BadRequest(format!(
            "unknown category `{}` (valid: disk, db, queue)",
            req.category
        ))
    })?;
    let tunables = ab_core::tunables::CleanupTunables::default();
    let age_seconds = ab_core::cleanup::compute_age_seconds(&tunables, u64::MAX, u64::MAX);
    // `Policy::apply` is the internal name on the cleanup struct
    // and stays as-is — that's the low-level field the targets
    // read. The API + CLI surface uses `commit` per ADR-0029.
    let policy = ab_core::cleanup::Policy {
        age_seconds,
        force: req.force,
        apply: req.commit,
    };
    let cleanup_ctx = ab_pipeline::cleanup::CleanupCtx {
        library: state.inner.library.clone(),
        ephemeral: state.inner.ephemeral.clone(),
    };
    let reports =
        ab_pipeline::cleanup::run_category(&cleanup_ctx, &state.inner.cleanup, category, policy)
            .await?;
    Ok(Json(CleanRunResponse {
        category: category.to_string(),
        commit: req.commit,
        force: req.force,
        age_seconds,
        targets: reports.into_iter().map(Into::into).collect(),
    }))
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

/// Canonicalise + verify the requested scan path is at-or-under
/// one of the active rows in `library_roots`. Returns the
/// canonical path on success, [`ApiError::BadRequest`] on any
/// rejection (empty root list, nonexistent path, path outside
/// the allow-list).
///
/// **Source of truth (post-B.7, tracker #119)**: roots live in
/// the `library_roots` table (DB-backed, managed via
/// `GET/POST/DELETE /api/v1/library_roots`). The previous
/// `tunables.security.library_roots` Vec + one-cycle seed bridge
/// have been removed; the REST surface is the only registration
/// path.
///
/// Surfaced as a free function so the `library_scan` handler
/// stays under the `clippy::too_many_lines` cap.
async fn validate_scan_path(state: &ApiState, requested: &FsPath) -> Result<PathBuf, ApiError> {
    let canonical = std::fs::canonicalize(requested).map_err(|e| {
        tracing::info!(
            requested = %requested.display(),
            error = %e,
            "api.library_scan.reject_canonicalize_failed"
        );
        ApiError::BadRequest(format!("path does not exist or is not readable: {e}"))
    })?;
    let under = crate::library_roots::path_is_under_any_root(state, &canonical).await?;
    if !under {
        // Distinguish "no roots at all" from "outside the list"
        // so the operator's diagnostic is precise. Cheap extra
        // query — happens only on the reject path.
        let roots_exist: i64 = sqlx::query_scalar!(
            "SELECT COUNT(*) AS \"n!: i64\" FROM library_roots WHERE is_active = 1",
        )
        .fetch_one(state.inner.library.pool())
        .await
        .map_err(|e| ab_core::Error::Database(format!("library_scan roots-count check: {e}")))?;
        if roots_exist == 0 {
            tracing::warn!(
                requested = %canonical.display(),
                "api.library_scan.reject_no_roots_configured"
            );
            return Err(ApiError::BadRequest(
                "scan disabled: no library_roots registered (POST /api/v1/library_roots)"
                    .to_owned(),
            ));
        }
        tracing::warn!(
            requested = %canonical.display(),
            "api.library_scan.reject_outside_roots"
        );
        return Err(ApiError::BadRequest(format!(
            "path {} is not under any registered library root",
            canonical.display(),
        )));
    }
    Ok(canonical)
}

async fn library_scan(
    State(state): State<ApiState>,
    Json(req): Json<ScanRequest>,
) -> Result<Json<ScanResponse>, ApiError> {
    let requested = validate_scan_path(&state, &req.path).await?;
    let report =
        ab_scan::scan_with_excludes(&requested, &state.inner.library, &state.inner.scan_excludes)
            .await?;

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
    // (stage, priority) pairs — `StageId` is the typed
    // single-source-of-truth for each stage's name; importing
    // the stage's crate is a `Cargo.toml` edit, not a structural
    // dep cost (all of these crates were already transitively in
    // the daemon's compile graph). A renamed stage now surfaces
    // as an unresolved-symbol error here, not a runtime
    // "pipeline.stage.unknown" warning.
    let stage_priorities: &[(ab_pipeline::StageId, ab_pipeline::Priority)] = &[
        (ab_tag_read::STAGE_ID, ab_pipeline::Priority::Interactive),
        (ab_fingerprint::STAGE_ID, ab_pipeline::Priority::Interactive),
        (
            ab_catalog::audible_search::STAGE_ID,
            ab_pipeline::Priority::Interactive,
        ),
        (
            ab_catalog::enrich::STAGE_ID,
            ab_pipeline::Priority::Interactive,
        ),
        (
            ab_catalog::consensus::STAGE_ID,
            ab_pipeline::Priority::Interactive,
        ),
        (
            ab_catalog::identity::STAGE_ID,
            ab_pipeline::Priority::Interactive,
        ),
        (
            ab_catalog::chapters::STAGE_ID,
            ab_pipeline::Priority::Interactive,
        ),
        (
            ab_catalog::embedded_chapters::STAGE_ID,
            ab_pipeline::Priority::Interactive,
        ),
        (
            ab_catalog::chapter_winner::STAGE_ID,
            ab_pipeline::Priority::Interactive,
        ),
        // 6-min head + 30-s tail. Heavier than the other
        // stages (multi-second per book at decode +
        // SpeechAnalyzer time) but seeded at scan time so the
        // language gate + downstream extractors have a
        // transcript by the time the user opens the book.
        (
            ab_transcript::stage::STAGE_ID,
            ab_pipeline::Priority::Interactive,
        ),
        // Description language detector runs right after
        // consensus (the description winner is picked there).
        (
            ab_transcript::description_lang_stage::STAGE_ID,
            ab_pipeline::Priority::Interactive,
        ),
        // Transcript extractors — cheap pure-text heuristics
        // over the head transcript; runs at Interactive so the
        // user sees its candidates by the time the book opens.
        (
            ab_transcript::extract_stage::STAGE_ID,
            ab_pipeline::Priority::Interactive,
        ),
        // Sampled transcribe — Background priority. Three 60-s
        // windows at 25/50/75%. Provides authoritative
        // post-transcribe language signal + a representative
        // DNA-tag corpus before the full-book transcribe lands.
        (
            ab_transcript::samples_stage::STAGE_ID,
            ab_pipeline::Priority::Background,
        ),
        // Whole-book transcribe — drains during quiet periods,
        // not in the import-time pipeline.
        (
            ab_transcript::full_stage::STAGE_ID,
            ab_pipeline::Priority::Idle,
        ),
        // Transcode-to-m4b (ADR-0027). Background priority so it
        // doesn't preempt the import-time AI cluster but still
        // drains during the daemon's working hours; the parallel
        // `book_file_refs` lifecycle keeps sources alive for
        // concurrent AI reads.
        //
        // `tag-write-early` and `tag-write-final` (ADR-0028) are
        // **deliberately absent** from this list. Both have
        // non-empty `requires()` chains terminating at stages
        // already submitted here (Early on `tag-read`; Final on
        // `consensus` + every AI extractor + `transcode-m4b`).
        // The scheduler's `dispatch_ready_dependents` fires them
        // automatically once their last dependency completes
        // per book; a redundant scan-time submit would just
        // execute them with empty winner sets and Skip. For
        // already-imported books where the auto-dispatch window
        // has passed, `aborg book retry --stage tag-write-final`
        // (ADR-0023) is the manual trigger.
    ];
    for book_id in &report.new_book_ids {
        for (stage, priority) in stage_priorities {
            if let Err(e) = state
                .inner
                .scheduler
                .submit(*book_id, *stage, *priority)
                .await
            {
                tracing::warn!(
                    book = %book_id,
                    stage = %stage,
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
    /// Display author — `is_prime=1` alias from `author_aliases` if
    /// any, else `authors.name`. `None` when the book has no
    /// resolved author (consensus hasn't run yet or no candidates).
    /// Per ADR-0026.
    #[serde(skip_serializing_if = "Option::is_none")]
    author: Option<String>,
    /// Display narrator(s) — same prime-alias rule, comma-separated
    /// when more than one. `None` when no narrators linked.
    #[serde(skip_serializing_if = "Option::is_none")]
    narrators: Option<String>,
    /// Display series — same prime-alias rule. Picks the
    /// `book_series.is_primary = 1` row (C5.6); secondary series
    /// don't surface in the list view.
    #[serde(skip_serializing_if = "Option::is_none")]
    series: Option<String>,
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

/// Response shape for `GET /library/pending_speech_installs`.
/// One row per locale that books are waiting on; the daemon's
/// idle install loop drains these in the background.
#[derive(Serialize)]
struct PendingSpeechInstall {
    locale: String,
    status: String,
    blocked_book_count: i64,
    last_error: Option<String>,
}

#[derive(Serialize)]
struct PendingSpeechInstallsResponse {
    installs: Vec<PendingSpeechInstall>,
}

/// Surface the speech-model install backlog to clients so
/// interactive scans can show "this book is waiting on
/// <locale> model install" instead of silently leaving the
/// stage at `Skipped`. The idle install loop is what drives
/// the actual download; this endpoint is read-only.
async fn library_pending_speech_installs(
    State(state): State<ApiState>,
) -> Result<Json<PendingSpeechInstallsResponse>, ApiError> {
    let rows = sqlx::query!(
        "SELECT p.locale, p.status, p.last_error, \
                (SELECT COUNT(*) FROM book_locale_blocks b \
                 WHERE b.locale = p.locale) AS \"blocked_book_count!\" \
         FROM pending_speech_installs p \
         WHERE p.status IN ('pending', 'installing', 'failed') \
         ORDER BY p.status, p.queued_at",
    )
    .fetch_all(state.inner.ephemeral.pool())
    .await
    .map_err(|e| ab_core::Error::Database(format!("pending speech installs: {e}")))?;

    let installs = rows
        .into_iter()
        .map(|r| PendingSpeechInstall {
            locale: r.locale,
            status: r.status,
            blocked_book_count: r.blocked_book_count,
            last_error: r.last_error,
        })
        .collect();
    Ok(Json(PendingSpeechInstallsResponse { installs }))
}

/// Response shape for `POST /library/pending_speech_installs/retry`.
#[derive(Serialize)]
struct RetryFailedSpeechInstallsResponse {
    /// Count of rows flipped from `failed` to `pending`.
    requeued: i64,
}

/// Flip every `pending_speech_installs.status='failed'` row
/// back to `'pending'` so the idle install loop picks them up
/// on the next wake. Idempotent: rows already `'pending'` /
/// `'installing'` / `'installed'` aren't touched.
///
/// Used by the future config UI's "retry blocked installs"
/// button after the user fixes the underlying issue
/// (re-enable Apple Intelligence in System Settings, install
/// a missing language pack manually, etc.). For v0 the UI
/// just POSTs here; future versions could allow per-locale
/// selection via a body parameter.
async fn library_retry_failed_speech_installs(
    State(state): State<ApiState>,
) -> Result<Json<RetryFailedSpeechInstallsResponse>, ApiError> {
    let result = sqlx::query!(
        "UPDATE pending_speech_installs \
         SET status = 'pending', last_error = NULL \
         WHERE status = 'failed'",
    )
    .execute(state.inner.ephemeral.pool())
    .await
    .map_err(|e| ab_core::Error::Database(format!("retry failed speech installs: {e}")))?;
    let requeued = result.rows_affected();
    tracing::info!(requeued, "library.speech_installs.retry");
    // u64 → i64 cast: rows_affected is a row count, capped at
    // a few thousand pending locales in any sane deployment;
    // well inside i64.
    #[allow(clippy::cast_possible_wrap)]
    let requeued_i64 = requeued as i64;
    Ok(Json(RetryFailedSpeechInstallsResponse {
        requeued: requeued_i64,
    }))
}

/// Query parameters for `GET /books` — matches the
/// `API.md` contract (REVIEW.md § 1.5 flagged the previous
/// handler ignoring every one of these).
///
/// All fields optional; combined with AND. The `q` parameter
/// is a case-insensitive substring against `books.title`;
/// `author` / `series` resolve against the prime-alias view
/// (consistent with the row's display values, not the raw
/// `authors.name` / `series.name`).
///
/// `limit` defaults to 100 and is hard-capped at
/// [`BOOKS_LIST_MAX_LIMIT`] so a missing / pathological
/// caller can't ask for 10 million rows at once. `offset`
/// defaults to 0.
#[derive(Deserialize, Debug, Default)]
struct BooksQuery {
    /// Substring filter against `books.title` (case-insensitive).
    q: Option<String>,
    /// Substring filter against the displayed author name.
    author: Option<String>,
    /// Substring filter against the displayed primary-series
    /// name.
    series: Option<String>,
    /// Page size (capped at [`BOOKS_LIST_MAX_LIMIT`]).
    limit: Option<u32>,
    /// Page offset.
    offset: Option<u32>,
    /// When `true`, soft-deleted books (those with
    /// `books.deleted_at IS NOT NULL`) appear in the response.
    /// Default `false` — most callers want the active library.
    /// See migration 024 + slice #102 for the soft-delete
    /// semantics.
    #[serde(default)]
    include_deleted: bool,
}

async fn books_list(
    State(state): State<ApiState>,
    axum::extract::Query(q): axum::extract::Query<BooksQuery>,
) -> Result<Json<BooksResponse>, ApiError> {
    // Delegate to the canonical executor (ADR-0031). The native
    // `GET /books?q=&author=&series=&...` query-string surface is
    // translated to `QueryFilter` here; future callers
    // (saved_queries, dispatcher, search) hit `ab_query::execute`
    // directly with the JSON-encoded struct.
    let filter = ab_query::QueryFilter {
        q: q.q,
        author: q.author,
        series: q.series,
        limit: q.limit,
        offset: q.offset,
        include_deleted: q.include_deleted,
        ..Default::default()
    };
    let rows = ab_query::execute(state.inner.library.pool(), &filter)
        .await
        .map_err(|e| ab_core::Error::Database(format!("books list: {e}")))?;

    let books = rows
        .into_iter()
        .map(|r| BookRow {
            book_id: r.book_id,
            title: r.title,
            file_path: r.file_path,
            author: r.author,
            narrators: r.narrators,
            series: r.series,
        })
        .collect();
    Ok(Json(BooksResponse { books }))
}

// ── GET /books/{book_id} ─────────────────────────────────────────

/// Active file row inside [`BookDetailResponse`].
#[derive(Serialize)]
struct BookFileDetail {
    file_id: i64,
    file_path: String,
    duration_ms: Option<i64>,
    file_size: Option<i64>,
    is_active: bool,
}

/// Per-stage row of `pipeline_progress` (lives in the ephemeral DB).
#[derive(Serialize)]
struct StageProgressRow {
    stage: String,
    /// One of `pending`, `running`, `succeeded`, `failed`, `skipped`.
    status: String,
    /// Unix seconds; `None` if the stage has never run.
    started_at: Option<i64>,
    /// Unix seconds; `None` until the stage terminates.
    completed_at: Option<i64>,
    /// Populated when `status = 'failed'`; surfaces the failure
    /// reason for the diagnostic CLI.
    failure_reason: Option<String>,
}

/// Per-chapter-source row count.
#[derive(Serialize)]
struct ChapterSourceCount {
    source: String,
    count: i64,
}

/// Successful response body for `GET /books/{book_id}`.
///
/// Diagnostic-shaped: every field an operator might want when
/// asking "why didn't this book extract / why does it look
/// wrong." Heavier than `/books` list rows by design — this is
/// the per-book detail surface.
#[derive(Serialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "JSON response struct — each bool is an independent presence flag for a specific AI-derived field. A state machine doesn't fit the open-set semantics; clients (web UI + CLI) consume each flag separately."
)]
struct BookDetailResponse {
    // Core book row.
    book_id: i64,
    title: String,
    subtitle: Option<String>,
    description: Option<String>,
    language: Option<String>,
    duration_ms: Option<i64>,
    asin: Option<String>,
    isbn: Option<String>,
    release_date: Option<String>,
    abridged: Option<bool>,
    explicit: Option<bool>,
    audiologo_status: String,
    /// Soft-delete timestamp (`books.deleted_at`). `null` for
    /// active books; unix-seconds when soft-deleted (slice #102,
    /// migration 024). The list endpoint hides soft-deleted
    /// rows by default; the detail endpoint always returns the
    /// row regardless of state so callers can render a
    /// "restore this book" UI without a second roundtrip.
    deleted_at: Option<i64>,

    // Joined.
    author: Option<String>,
    narrators: Option<String>,
    publisher: Option<String>,
    series: Option<String>,

    // AI-derived field presence (avoid bloating response with
    // multi-KB string payloads on a "show" call). Operator can
    // ask for full content via per-field endpoints when needed.
    has_summary: bool,
    has_story_arc: bool,
    has_setting: bool,
    has_characters: bool,

    files: Vec<BookFileDetail>,
    stages: Vec<StageProgressRow>,
    chapters: Vec<ChapterSourceCount>,
}

/// `GET /api/v1/books/{book_id}` — per-book detail surface.
/// Returns 404 if the book doesn't exist.
#[allow(
    clippy::too_many_lines,
    reason = "Diagnostic endpoint: per-book row + files + stages + chapters all assembled in one handler. Four separate sqlx::query! calls each with their own error mapping, then one composition step. Splitting hurts top-to-bottom readability — the steps are sequential and the macro requires inline query literals."
)]
async fn books_get(
    State(state): State<ApiState>,
    Path(book_id): Path<i64>,
) -> Result<Json<BookDetailResponse>, ApiError> {
    let pool = state.inner.library.pool();

    // Core book row + joined identity fields. The COALESCE
    // expressions mirror `books_list`'s prime-alias preference
    // so display strings are consistent across endpoints.
    let core = sqlx::query!(
        r#"SELECT
              b.book_id      AS "book_id!: i64",
              b.title,
              b.subtitle,
              b.description,
              b.language,
              b.duration_ms,
              b.asin,
              b.isbn,
              b.release_date,
              b.abridged,
              b.explicit,
              b.audiologo_status,
              b.deleted_at,
              b.summary_spoiler_free AS summary,
              b.story_arc_json,
              b.setting,
              COALESCE(
                  (SELECT alias FROM author_aliases
                     WHERE author_id = b.author_id AND is_prime = 1 LIMIT 1),
                  (SELECT name FROM authors WHERE author_id = b.author_id LIMIT 1)
              ) AS author,
              (SELECT GROUP_CONCAT(
                          COALESCE(
                              (SELECT alias FROM narrator_aliases na
                                 WHERE na.narrator_id = n.narrator_id AND is_prime = 1 LIMIT 1),
                              n.name
                          ), ', ')
                 FROM book_narrator bn
                 JOIN narrators n ON n.narrator_id = bn.narrator_id
                 WHERE bn.book_id = b.book_id) AS narrators,
              (SELECT name FROM publishers WHERE publisher_id = b.publisher_id LIMIT 1) AS publisher,
              (SELECT COALESCE(
                          (SELECT alias FROM series_aliases sa
                             WHERE sa.series_id = s.series_id AND is_prime = 1 LIMIT 1),
                          s.name)
                 FROM book_series bs
                 JOIN series s ON s.series_id = bs.series_id
                 WHERE bs.book_id = b.book_id
                 LIMIT 1) AS series
          FROM books b
          WHERE b.book_id = ?"#,
        book_id,
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("books_get core: {e}"))))?
    .ok_or_else(|| ApiError::NotFound(format!("book {book_id}")))?;

    // `characters` is its own table — a presence check is "does
    // at least one row exist for this book".
    let chars_row = sqlx::query!(
        r#"SELECT EXISTS(SELECT 1 FROM characters WHERE book_id = ?) AS "exists!: i64""#,
        book_id,
    )
    .fetch_one(pool)
    .await
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("books_get chars: {e}"))))?;
    let has_characters = chars_row.exists != 0;

    let file_rows = sqlx::query!(
        r#"SELECT
              file_id      AS "file_id!: i64",
              file_path,
              duration_ms,
              file_size,
              is_active    AS "is_active!: i64"
            FROM book_files
            WHERE book_id = ?
            ORDER BY file_id"#,
        book_id,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("books_get files: {e}"))))?;

    // `pipeline_progress` lives in the ephemeral DB (restartable
    // state — see migrations/ephemeral/001_initial.sql).
    let stage_rows = sqlx::query!(
        r#"SELECT stage, status, started_at, completed_at, failure_reason
            FROM pipeline_progress
            WHERE book_id = ?
            ORDER BY stage"#,
        book_id,
    )
    .fetch_all(state.inner.ephemeral.pool())
    .await
    .map_err(|e| ApiError::Internal(ab_core::Error::Database(format!("books_get stages: {e}"))))?;

    let chapter_rows = sqlx::query!(
        r#"SELECT source, COUNT(*) AS "count!: i64"
            FROM chapters
            WHERE book_id = ? AND is_winner = 1
            GROUP BY source
            ORDER BY source"#,
        book_id,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!("books_get chapters: {e}")))
    })?;

    Ok(Json(BookDetailResponse {
        book_id: core.book_id,
        title: core.title,
        subtitle: core.subtitle,
        description: core.description,
        language: core.language,
        duration_ms: core.duration_ms,
        asin: core.asin,
        isbn: core.isbn,
        release_date: core.release_date,
        abridged: core.abridged.map(|v| v != 0),
        explicit: core.explicit.map(|v| v != 0),
        audiologo_status: core.audiologo_status,
        deleted_at: core.deleted_at,
        author: core.author,
        narrators: core.narrators,
        publisher: core.publisher,
        series: core.series,
        has_summary: core.summary.is_some(),
        has_story_arc: core.story_arc_json.is_some(),
        has_setting: core.setting.is_some(),
        has_characters,
        files: file_rows
            .into_iter()
            .map(|r| BookFileDetail {
                file_id: r.file_id,
                file_path: r.file_path,
                duration_ms: r.duration_ms,
                file_size: r.file_size,
                is_active: r.is_active != 0,
            })
            .collect(),
        stages: stage_rows
            .into_iter()
            .map(|r| StageProgressRow {
                stage: r.stage,
                status: r.status,
                started_at: r.started_at,
                completed_at: r.completed_at,
                failure_reason: r.failure_reason,
            })
            .collect(),
        chapters: chapter_rows
            .into_iter()
            .map(|r| ChapterSourceCount {
                source: r.source,
                count: r.count,
            })
            .collect(),
    }))
}

// ── PATCH /books/{book_id} ───────────────────────────────────────

/// Body of `PATCH /api/v1/books/{book_id}`.
///
/// Every field is `Option<T>` — `None` (or absent) means "leave
/// untouched"; `Some(value)` means "update this field to value".
///
/// **v1 scope (slice #89):** the simple-mapping fields whose
/// values land in a single `books.<col>`. The join-driven fields
/// (`author`, `narrator`, `publisher`, `series`, `genre`) and
/// the set-typed `cover_url` need their own identity-resolve
/// plumbing and ship as follow-up slices.
#[derive(Deserialize)]
struct BooksPatchRequest {
    title: Option<String>,
    subtitle: Option<String>,
    description: Option<String>,
    language: Option<String>,
    release_date: Option<String>,
    asin: Option<String>,
    isbn: Option<String>,
    abridged: Option<bool>,
    explicit: Option<bool>,
}

/// Response from `PATCH /api/v1/books/{book_id}`.
#[derive(Serialize)]
struct BooksPatchResponse {
    book_id: i64,
    /// The `book_field_provenance.field` strings that were
    /// updated by this request, in stable order. Empty when the
    /// caller sent no field updates.
    updated: Vec<String>,
}

/// `PATCH /api/v1/books/{book_id}` — write user-edit provenance.
///
/// For every field present in the body the handler:
///
/// 1. Demotes any existing `is_winner = 1` row for `(book_id,
///    field)`.
/// 2. Inserts a new `source = 'user_edit'`, `stage =
///    'api-user-edit'`, `confidence = 1.0`, `is_winner = 1`
///    row into `book_field_provenance`.
/// 3. Updates the matching `books.<col>` so subsequent reads
///    (including the next `tag-write-final` run) see the user's
///    value immediately, not the next-cycle AI alternative.
///
/// All three steps run inside a single transaction; partial
/// failure leaves the book in its prior state.
///
/// Per ADR-0028's user-edit rule, the
/// `confidence = 1.0` floor means consensus's winner-pick
/// naturally prefers the user-edit row over every AI candidate,
/// and `TagWriteFinalStage`'s per-field skip
/// (`ab_tag_write::skip_for_final_pass`) keeps the value sticky
/// across the late tag-write pass.
///
/// Returns **404** when `book_id` doesn't exist; **400** when
/// the body has no field updates (the partial-write semantics
/// mean an empty body is almost certainly a caller mistake;
/// surface it eagerly).
#[allow(
    clippy::too_many_lines,
    reason = "per-field update flow expands linearly: 9 fields × (validate → record_user_edit → update column). Splitting via macro or per-field helper hides the SQL each field touches and forces a generic `value: &str` boundary that loses the bool/string type distinction. The current shape reads top-to-bottom field-by-field."
)]
async fn books_patch(
    State(state): State<ApiState>,
    Path(book_id): Path<i64>,
    Json(req): Json<BooksPatchRequest>,
) -> Result<Json<BooksPatchResponse>, ApiError> {
    use crate::user_edits::record_user_edit;
    use ab_core::Field;

    // 0. Verify book exists. Cheap separate query so the 404
    // doesn't get masked by a transaction-rollback chain.
    let exists = sqlx::query!(
        r#"SELECT EXISTS(SELECT 1 FROM books WHERE book_id = ?) AS "exists!: i64""#,
        book_id,
    )
    .fetch_one(state.inner.library.pool())
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!("books_patch exists: {e}")))
    })?;
    if exists.exists == 0 {
        return Err(ApiError::NotFound(format!("book {book_id}")));
    }

    // 1. Refuse an empty body — almost certainly a caller bug.
    let any_update = req.title.is_some()
        || req.subtitle.is_some()
        || req.description.is_some()
        || req.language.is_some()
        || req.release_date.is_some()
        || req.asin.is_some()
        || req.isbn.is_some()
        || req.abridged.is_some()
        || req.explicit.is_some();
    if !any_update {
        return Err(ApiError::BadRequest(
            "no fields to update; supply at least one field".to_owned(),
        ));
    }

    let mut tx = state.inner.library.pool().begin().await.map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!("books_patch begin: {e}")))
    })?;

    let mut updated: Vec<String> = Vec::new();

    if let Some(v) = req.title.as_deref() {
        record_user_edit(&mut tx, book_id, Field::Title, Some(v)).await?;
        sqlx::query!("UPDATE books SET title = ? WHERE book_id = ?", v, book_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                ApiError::Internal(ab_core::Error::Database(format!("books_patch title: {e}")))
            })?;
        updated.push(Field::Title.as_str().to_owned());
    }
    if let Some(v) = req.subtitle.as_deref() {
        record_user_edit(&mut tx, book_id, Field::Subtitle, Some(v)).await?;
        sqlx::query!(
            "UPDATE books SET subtitle = ? WHERE book_id = ?",
            v,
            book_id
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            ApiError::Internal(ab_core::Error::Database(format!(
                "books_patch subtitle: {e}"
            )))
        })?;
        updated.push(Field::Subtitle.as_str().to_owned());
    }
    if let Some(v) = req.description.as_deref() {
        record_user_edit(&mut tx, book_id, Field::Description, Some(v)).await?;
        sqlx::query!(
            "UPDATE books SET description = ? WHERE book_id = ?",
            v,
            book_id
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            ApiError::Internal(ab_core::Error::Database(format!(
                "books_patch description: {e}"
            )))
        })?;
        updated.push(Field::Description.as_str().to_owned());
    }
    if let Some(v) = req.language.as_deref() {
        record_user_edit(&mut tx, book_id, Field::Language, Some(v)).await?;
        sqlx::query!(
            "UPDATE books SET language = ? WHERE book_id = ?",
            v,
            book_id
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            ApiError::Internal(ab_core::Error::Database(format!(
                "books_patch language: {e}"
            )))
        })?;
        updated.push(Field::Language.as_str().to_owned());
    }
    if let Some(v) = req.release_date.as_deref() {
        record_user_edit(&mut tx, book_id, Field::ReleaseDate, Some(v)).await?;
        sqlx::query!(
            "UPDATE books SET release_date = ? WHERE book_id = ?",
            v,
            book_id
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            ApiError::Internal(ab_core::Error::Database(format!(
                "books_patch release_date: {e}"
            )))
        })?;
        updated.push(Field::ReleaseDate.as_str().to_owned());
    }
    if let Some(v) = req.asin.as_deref() {
        record_user_edit(&mut tx, book_id, Field::Asin, Some(v)).await?;
        sqlx::query!("UPDATE books SET asin = ? WHERE book_id = ?", v, book_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                ApiError::Internal(ab_core::Error::Database(format!("books_patch asin: {e}")))
            })?;
        updated.push(Field::Asin.as_str().to_owned());
    }
    if let Some(v) = req.isbn.as_deref() {
        record_user_edit(&mut tx, book_id, Field::Isbn, Some(v)).await?;
        sqlx::query!("UPDATE books SET isbn = ? WHERE book_id = ?", v, book_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                ApiError::Internal(ab_core::Error::Database(format!("books_patch isbn: {e}")))
            })?;
        updated.push(Field::Isbn.as_str().to_owned());
    }
    if let Some(v) = req.abridged {
        // Bool → "0" / "1" for the provenance string column; the
        // books column is INTEGER and stores the i64 form
        // directly.
        let v_str = if v { "1" } else { "0" };
        let v_int: i64 = i64::from(v);
        record_user_edit(&mut tx, book_id, Field::Abridged, Some(v_str)).await?;
        sqlx::query!(
            "UPDATE books SET abridged = ? WHERE book_id = ?",
            v_int,
            book_id
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            ApiError::Internal(ab_core::Error::Database(format!(
                "books_patch abridged: {e}"
            )))
        })?;
        updated.push(Field::Abridged.as_str().to_owned());
    }
    if let Some(v) = req.explicit {
        let v_str = if v { "1" } else { "0" };
        let v_int: i64 = i64::from(v);
        record_user_edit(&mut tx, book_id, Field::Explicit, Some(v_str)).await?;
        sqlx::query!(
            "UPDATE books SET explicit = ? WHERE book_id = ?",
            v_int,
            book_id
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            ApiError::Internal(ab_core::Error::Database(format!(
                "books_patch explicit: {e}"
            )))
        })?;
        updated.push(Field::Explicit.as_str().to_owned());
    }

    tx.commit().await.map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!("books_patch commit: {e}")))
    })?;

    Ok(Json(BooksPatchResponse { book_id, updated }))
}

// ── DELETE /books/{book_id} ──────────────────────────────────────

/// Query parameters for `DELETE /api/v1/books/{book_id}`.
#[derive(Deserialize)]
struct BooksDeleteQuery {
    /// Per ADR-0029 § "Truly-irreversible operations", `force`
    /// is the second-tier opt-in for the unrecoverable variant.
    /// Without `force`, the handler soft-deletes (reversible by
    /// flipping `books.deleted_at` back to NULL — a future
    /// `restore` endpoint will surface this). With `force=true`,
    /// the row is hard-deleted with CASCADE.
    #[serde(default)]
    force: bool,
}

/// `DELETE /api/v1/books/{book_id}` — soft-delete by default,
/// hard-delete with `?force=true`.
///
/// **Soft delete** (no `force`, the default): sets
/// `books.deleted_at = unix-now`. Row stays in the database,
/// FKs intact; the list endpoint hides it (unless
/// `?include_deleted=true`); the dispatcher stops scheduling
/// new pipeline work for it. A future restore endpoint can
/// flip `deleted_at` back to NULL.
///
/// **Hard delete** (`?force=true`): drops the row outright.
/// Every `ON DELETE CASCADE` FK on `books.book_id` clears in
/// the same transaction (`book_field_provenance`, `book_files`,
/// `chapters`, `book_narrator`, `book_series`, `book_tags`,
/// `characters`). `mass_edit_history` rows survive with
/// orphaned `target_id` — that's the audit-trail-preservation
/// design.
///
/// Both paths return **204 `NoContent`** on success and **404
/// `NotFound`** when the book doesn't exist. The soft path is
/// idempotent: a second soft-delete on an already-soft-deleted
/// row returns 204 with no further changes (`deleted_at` not
/// overwritten — preserves the original deletion timestamp).
///
/// On-disk audio files in `book_files.file_path` are NOT
/// removed by either path — that's a separate cleanup concern
/// (future `OrphanBookFilesTarget` per the
/// cleanup-future-targets memo).
async fn books_delete(
    State(state): State<ApiState>,
    Path(book_id): Path<i64>,
    axum::extract::Query(q): axum::extract::Query<BooksDeleteQuery>,
) -> Result<StatusCode, ApiError> {
    let pool = state.inner.library.pool();

    // Cheap existence check up front so 404 doesn't surface as
    // "0 rows affected" — distinguishes a missing book from a
    // concurrent delete race.
    let exists = sqlx::query!(
        r#"SELECT EXISTS(SELECT 1 FROM books WHERE book_id = ?) AS "exists!: i64""#,
        book_id,
    )
    .fetch_one(pool)
    .await
    .map_err(|e| {
        ApiError::Internal(ab_core::Error::Database(format!(
            "books_delete exists: {e}"
        )))
    })?;
    if exists.exists == 0 {
        return Err(ApiError::NotFound(format!("book {book_id}")));
    }

    if q.force {
        sqlx::query!("DELETE FROM books WHERE book_id = ?", book_id)
            .execute(pool)
            .await
            .map_err(|e| {
                ApiError::Internal(ab_core::Error::Database(format!("books_delete hard: {e}")))
            })?;
        tracing::info!(book_id, "api.books.deleted_force");
    } else {
        // Soft-delete. `WHERE deleted_at IS NULL` preserves the
        // ORIGINAL deletion timestamp on re-call — idempotent
        // per the doc.
        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        )
        .unwrap_or(i64::MAX);
        sqlx::query!(
            "UPDATE books SET deleted_at = ? WHERE book_id = ? AND deleted_at IS NULL",
            now,
            book_id,
        )
        .execute(pool)
        .await
        .map_err(|e| {
            ApiError::Internal(ab_core::Error::Database(format!("books_delete soft: {e}")))
        })?;
        tracing::info!(book_id, deleted_at = now, "api.books.soft_deleted");
    }

    Ok(StatusCode::NO_CONTENT)
}

// ── POST /books/{book_id}/restore ────────────────────────────────

/// Response shape for `POST /api/v1/books/{book_id}/restore`.
/// Returns the `book_id` plus a `restored` flag so the caller
/// can distinguish "we actually flipped `deleted_at` back to NULL"
/// from "this book was already active" without parsing the
/// `deleted_at` field separately.
#[derive(Serialize)]
struct BooksRestoreResponse {
    book_id: i64,
    /// `true` if `deleted_at` was non-NULL before the call (and
    /// is NULL after). `false` if the book was already active
    /// — the call is a no-op in that case (idempotent).
    restored: bool,
}

/// `POST /api/v1/books/{book_id}/restore` — un-soft-delete a
/// book by flipping `books.deleted_at` back to NULL.
///
/// Slice #102 added soft-delete (`DELETE /books/{id}` default).
/// This is its symmetric undo. The row stays the same; the
/// `deleted_at` column flips from a unix-timestamp to NULL,
/// which un-hides the book from `GET /books` and re-allows
/// the pipeline dispatcher to schedule new work for it.
///
/// **Idempotent**: restoring an already-active book returns
/// 200 with `restored: false` and no further state change.
/// Operators don't need to check the book's state before
/// calling.
///
/// # Errors
///
/// - [`ApiError::NotFound`] when `book_id` doesn't exist
///   (neither active nor soft-deleted).
/// - [`ApiError::Internal`] on DB failure.
async fn books_restore(
    State(state): State<ApiState>,
    Path(book_id): Path<i64>,
) -> Result<Json<BooksRestoreResponse>, ApiError> {
    let pool = state.inner.library.pool();

    // Read the current state before mutating. Distinguishes the
    // three cases:
    //   - row missing → 404
    //   - row exists, deleted_at IS NULL → no-op (restored=false)
    //   - row exists, deleted_at IS NOT NULL → flip + restored=true
    let row = sqlx::query!(r#"SELECT deleted_at FROM books WHERE book_id = ?"#, book_id,)
        .fetch_optional(pool)
        .await
        .map_err(|e| {
            ApiError::Internal(ab_core::Error::Database(format!(
                "books_restore lookup: {e}"
            )))
        })?
        .ok_or_else(|| ApiError::NotFound(format!("book {book_id}")))?;

    let was_deleted = row.deleted_at.is_some();
    if was_deleted {
        sqlx::query!(
            "UPDATE books SET deleted_at = NULL WHERE book_id = ?",
            book_id,
        )
        .execute(pool)
        .await
        .map_err(|e| {
            ApiError::Internal(ab_core::Error::Database(format!(
                "books_restore update: {e}"
            )))
        })?;
        tracing::info!(book_id, "api.books.restored");
    } else {
        tracing::info!(book_id, "api.books.restore_noop");
    }

    Ok(Json(BooksRestoreResponse {
        book_id,
        restored: was_deleted,
    }))
}

// ── /doctor/speech ───────────────────────────────────────────────

/// One locale's status as the doctor surfaces it. Combines:
///
/// - The library-side view (how many books need this locale).
/// - The Speech-framework view (installed / supported / etc.).
/// - The idle-installer view (any failed install with last error).
#[derive(Serialize)]
struct DoctorSpeechLocale {
    /// BCP-47 primary subtag — e.g. `"en"`, `"de"`, `"zh-Hans"`.
    locale: String,
    /// Number of books in the library carrying this locale as a
    /// language-provenance candidate.
    library_books: i64,
    /// Number of books whose `transcribe-head-tail` stage hit
    /// `ModelNotInstalled` for this locale and is waiting.
    blocked_books: i64,
    /// `installed` / `supported` / `downloading` / `unsupported`
    /// / `unknown` as reported by the Speech SDK. `null` when
    /// the FFI bridge isn't linked (non-macOS / no-swiftc build).
    sdk_status: Option<String>,
    /// `true` only when `sdk_status == "installed"` — convenience
    /// for clients that don't want to string-match.
    sdk_installed: bool,
    /// Idle-installer state if we've previously attempted:
    /// `pending` / `installing` / `failed` / `null`.
    idle_state: Option<String>,
    /// Last install error from the idle installer, if any.
    last_error: Option<String>,
}

#[derive(Serialize)]
struct DoctorSpeechResponse {
    /// `false` when Apple Intelligence is disabled / not
    /// provisioned on the host. When false, every per-locale
    /// install will fail — the user fixes via System Settings
    /// before retrying.
    framework_available: bool,
    /// Per-locale rows.
    locales: Vec<DoctorSpeechLocale>,
}

/// `GET /api/v1/doctor/speech` — surface everything the doctor
/// command needs to diagnose Speech-model state. Read-only.
async fn doctor_speech(
    State(state): State<ApiState>,
) -> Result<Json<DoctorSpeechResponse>, ApiError> {
    // Library-side: distinct languages we care about, with
    // per-locale book counts.
    let library_rows = sqlx::query!(
        r#"SELECT value AS "locale!",
                  COUNT(DISTINCT book_id) AS "n!"
           FROM book_field_provenance
           WHERE field = 'language' AND value IS NOT NULL
           GROUP BY value
           ORDER BY value"#,
    )
    .fetch_all(state.inner.library.pool())
    .await
    .map_err(|e| ab_core::Error::Database(format!("doctor lib langs: {e}")))?;

    // Idle-installer side: pending / installing / failed rows
    // (we don't surface "installed" — those are cleaned up).
    let ephem_rows = sqlx::query!(
        r#"SELECT p.locale,
                  p.status,
                  p.last_error,
                  (SELECT COUNT(*) FROM book_locale_blocks b
                   WHERE b.locale = p.locale) AS "blocked!: i64"
           FROM pending_speech_installs p
           WHERE p.status IN ('pending', 'installing', 'failed')"#,
    )
    .fetch_all(state.inner.ephemeral.pool())
    .await
    .map_err(|e| ab_core::Error::Database(format!("doctor pending: {e}")))?;

    // Merge: union of all locales seen in either source.
    let mut locales: std::collections::BTreeMap<String, DoctorSpeechLocale> =
        std::collections::BTreeMap::new();
    for r in library_rows {
        locales.insert(
            r.locale.clone(),
            DoctorSpeechLocale {
                locale: r.locale,
                library_books: r.n,
                blocked_books: 0,
                sdk_status: None,
                sdk_installed: false,
                idle_state: None,
                last_error: None,
            },
        );
    }
    for r in ephem_rows {
        let row = locales
            .entry(r.locale.clone())
            .or_insert_with(|| DoctorSpeechLocale {
                locale: r.locale.clone(),
                library_books: 0,
                blocked_books: 0,
                sdk_status: None,
                sdk_installed: false,
                idle_state: None,
                last_error: None,
            });
        row.blocked_books = r.blocked;
        row.idle_state = Some(r.status);
        row.last_error = r.last_error;
    }

    // SDK side: query each locale's status via the typed FFI.
    // First query establishes `framework_available`; that flag
    // is the same across all locales (it's a host-wide gate).
    let mut framework_available = true;
    for row in locales.values_mut() {
        match ab_speech::speech_locale_status(&row.locale).await {
            Ok(report) => {
                framework_available = framework_available && report.framework_available;
                row.sdk_installed = report.status == "installed";
                row.sdk_status = Some(report.status);
            }
            Err(e) => {
                tracing::warn!(locale = %row.locale, error = %e, "doctor.speech_status_failed");
                // Best-effort: leave sdk_* fields as-is (None
                // / false). UI shows "?" for unknown.
            }
        }
    }

    Ok(Json(DoctorSpeechResponse {
        framework_available,
        locales: locales.into_values().collect(),
    }))
}

/// Body for `POST /api/v1/doctor/speech/install`.
///
/// Exactly one of `locale` / `all` must be set. The
/// `untagged` enum form gives clients a clean shape:
/// `{"locale": "de"}` or `{"all": true}`.
#[derive(Deserialize)]
#[serde(untagged)]
enum DoctorSpeechInstallRequest {
    Locale {
        /// Canonical BCP-47 form, primary subtag suffices
        /// (`"de"`, `"en"`, `"zh-Hans"`).
        locale: String,
    },
    All {
        /// When `true`, install every locale that currently has
        /// books in the library AND whose SDK status isn't
        /// already `installed`. Idempotent: locales already
        /// installed are skipped, not re-downloaded.
        #[serde(rename = "all")]
        _all: bool,
    },
}

#[derive(Serialize)]
struct DoctorSpeechInstallResponse {
    /// Locales newly installed by this call.
    installed: Vec<String>,
    /// Locales already installed when we checked — no work done.
    already_installed: Vec<String>,
    /// Locales whose install failed; pair `(locale, reason)`.
    failed: Vec<(String, String)>,
}

/// `POST /api/v1/doctor/speech/install`. Body is a
/// [`DoctorSpeechInstallRequest`].
async fn doctor_speech_install(
    State(state): State<ApiState>,
    Json(req): Json<DoctorSpeechInstallRequest>,
) -> Result<Json<DoctorSpeechInstallResponse>, ApiError> {
    let locales: Vec<String> = match req {
        DoctorSpeechInstallRequest::Locale { locale } => vec![locale],
        DoctorSpeechInstallRequest::All { .. } => {
            // Every locale that has a library candidate row.
            sqlx::query_scalar!(
                r#"SELECT DISTINCT value AS "v!"
                   FROM book_field_provenance
                   WHERE field = 'language' AND value IS NOT NULL"#,
            )
            .fetch_all(state.inner.library.pool())
            .await
            .map_err(|e| ab_core::Error::Database(format!("doctor all locales: {e}")))?
        }
    };

    let mut installed = Vec::new();
    let mut already_installed = Vec::new();
    let mut failed: Vec<(String, String)> = Vec::new();
    for locale in &locales {
        // Skip when already installed — saves the round trip.
        match ab_speech::speech_locale_status(locale).await {
            Ok(report) if report.status == "installed" => {
                already_installed.push(locale.clone());
                continue;
            }
            Ok(_) => { /* fall through to install */ }
            Err(e) => {
                failed.push((locale.clone(), format!("status check: {e}")));
                continue;
            }
        }
        match ab_speech::install_speech_model_typed(locale).await {
            Ok(()) => installed.push(locale.clone()),
            Err(e) => failed.push((locale.clone(), e.to_string())),
        }
    }

    tracing::info!(
        installed = installed.len(),
        skipped = already_installed.len(),
        failed = failed.len(),
        "doctor.speech_install.done"
    );
    Ok(Json(DoctorSpeechInstallResponse {
        installed,
        already_installed,
        failed,
    }))
}

// ── /books/{book_id}/retry ────────────────────────────────────

/// Body for `POST /api/v1/books/{book_id}/retry`.
///
/// See ADR-0023 for the full design. Generic over any
/// registered pipeline stage (`tag-read`, `fingerprint`,
/// `extract-summary-spoiler-free`, …); not limited to LLM
/// extractors.
///
/// `stages` accepts either:
///
/// - a list of stage names: `{"stages": ["tag-read", "fingerprint"]}`
/// - the literal string `"all"`: `{"stages": "all"}` → every
///   registered stage.
///
/// Per ADR-0023 the operator names the exact set to reset; no
/// implicit graph traversal. Auto-dispatch via slice 1F.A.2
/// will re-run downstream stages whose outputs were derived
/// from this set; if the operator wants those reset too, they
/// list them in `stages` explicitly.
#[derive(Deserialize)]
struct RetryRequest {
    /// One-or-many stage selector. See struct docs.
    stages: StagesSelector,
}

/// `stages` accepts either a list or the literal `"all"`.
/// Serde's untagged enum picks the matching variant from the
/// JSON shape.
#[derive(Deserialize)]
#[serde(untagged)]
enum StagesSelector {
    /// Explicit list of stage names.
    Many(Vec<String>),
    /// `"all"` → expanded to every registered stage at handler
    /// time.
    Wildcard(String),
}

/// Per-stage result line in [`RetryResponse`].
#[derive(Serialize)]
struct RetryStageResult {
    stage: String,
    /// `true` when reset deleted at least one row from any of
    /// the storage tiers it sweeps. `false` for stages that
    /// had no rows.
    reset_cleared_state: bool,
    /// `true` when the resubmit was successfully queued. A
    /// stage whose `reset()` failed has this `false`; the
    /// `error` field carries the reason.
    submitted: bool,
    /// Set when reset or submit failed for this specific
    /// stage. Other stages in the same request still attempt
    /// reset+submit (best-effort).
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Successful response body for `POST /books/{book_id}/retry`.
#[derive(Serialize)]
struct RetryResponse {
    book_id: i64,
    submitted_at: String,
    /// One result line per stage in the order the handler
    /// processed them (matching topological order, not
    /// request order).
    results: Vec<RetryStageResult>,
}

/// `POST /api/v1/books/{book_id}/retry` — call `Stage::reset`
/// for each requested stage, then submit each at Background
/// priority. ADR-0023 (multi-stage semantics added in slice
/// H.1.6).
///
/// The `stages` field accepts either a list of stage names or
/// the literal `"all"`. Each stage is reset+submitted
/// independently; per-stage errors land in the response
/// without aborting the rest (best-effort). HTTP-level
/// failures (unknown book, unknown stage in the list,
/// malformed body) return non-200.
///
/// Returns:
///
/// - **200** with one [`RetryStageResult`] per stage on success.
/// - **404** if `book_id` isn't in `books`.
/// - **400** if `stages` contains an unknown name (with
///   `known_stages: [..]` in the body so the operator can
///   recover from a typo).
async fn books_retry_stage(
    State(state): State<ApiState>,
    Path(book_id): Path<i64>,
    Json(req): Json<RetryRequest>,
) -> Response {
    if let Err(resp) = ensure_book_known(&state, book_id).await {
        return resp;
    }
    let resolved = match resolve_stages(&state, &req.stages) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    let results = run_retry_for_each(&state, book_id, resolved).await;
    let submitted_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    tracing::info!(book_id, stages = results.len(), "api.retry.completed");
    Json(RetryResponse {
        book_id,
        submitted_at,
        results,
    })
    .into_response()
}

/// Reject the request with a 404/500 if the book isn't in the
/// library. Separate helper so the main handler stays under
/// the `too_many_lines` cap.
async fn ensure_book_known(state: &ApiState, book_id: i64) -> Result<(), Response> {
    let book_known =
        sqlx::query_scalar!(r#"SELECT 1 AS "ok!" FROM books WHERE book_id = ?"#, book_id,)
            .fetch_optional(state.inner.library.pool())
            .await
            .map_err(|e| {
                let api_err: ApiError =
                    ab_core::Error::Database(format!("retry book lookup: {e}")).into();
                api_err.into_response()
            })?
            .is_some();
    if !book_known {
        return Err(ApiError::NotFound(format!("book_id {book_id} unknown")).into_response());
    }
    Ok(())
}

/// `(stage_name, stage_handle)` pair returned by
/// [`resolve_stages`]. The `String` is the operator-supplied
/// name (preserved for the response row); the `Arc<dyn Stage>`
/// is the registered stage object the handler will call
/// `reset()` on. Aliased to dodge `clippy::type_complexity` on
/// the helper's signature.
type ResolvedStage = (String, std::sync::Arc<dyn ab_pipeline::Stage>);

/// Expand the `stages` selector into a vec of `(name, stage)`.
/// `"all"` walks the DAG in topo order; an explicit list
/// validates every name up-front and fails the whole request
/// (with `known_stages` in the body) on the first miss.
fn resolve_stages(
    state: &ApiState,
    sel: &StagesSelector,
) -> Result<Vec<ResolvedStage>, Box<Response>> {
    match sel {
        StagesSelector::Wildcard(s) if s == "all" => Ok(state
            .inner
            .dag
            .iter_topo()
            .map(|(name, stage_arc)| (name.to_owned(), std::sync::Arc::clone(stage_arc)))
            .collect()),
        StagesSelector::Wildcard(other) => {
            let body = serde_json::json!({
                "type": "about:blank#bad-stages-selector",
                "title": "Bad Request",
                "status": 400,
                "detail": format!(
                    "stages must be a list or the literal \"all\"; got {other:?}"
                ),
            });
            Err(Box::new(
                (StatusCode::BAD_REQUEST, Json(body)).into_response(),
            ))
        }
        StagesSelector::Many(names) => {
            let mut acc = Vec::with_capacity(names.len());
            for n in names {
                let Some(stage_arc) = state.inner.dag.stage_by_name(n) else {
                    let mut known = state.inner.dag.known_stage_names();
                    known.sort_unstable();
                    let body = serde_json::json!({
                        "type": "about:blank#unknown-stage",
                        "title": "Bad Request",
                        "status": 400,
                        "detail": format!("unknown stage: {n:?}"),
                        "known_stages": known,
                    });
                    return Err(Box::new(
                        (StatusCode::BAD_REQUEST, Json(body)).into_response(),
                    ));
                };
                acc.push((n.clone(), stage_arc));
            }
            Ok(acc)
        }
    }
}

/// Per-stage best-effort reset + submit. Per-stage failures
/// land in `RetryStageResult.error` without aborting the rest.
async fn run_retry_for_each(
    state: &ApiState,
    book_id: i64,
    resolved: Vec<ResolvedStage>,
) -> Vec<RetryStageResult> {
    // Clone the daemon's root cancellation token (not a fresh
    // one) so SIGTERM-triggered shutdown also halts retry-spawned
    // stage work. Constructing a new token here was the bug —
    // `transcribe-full` retries would keep going past graceful
    // shutdown.
    let stage_ctx = ab_pipeline::StageContext {
        library: state.inner.library.clone(),
        ephemeral: state.inner.ephemeral.clone(),
        cancel: state.inner.cancel.clone(),
        stage_name: "",
    };
    let mut results = Vec::with_capacity(resolved.len());
    for (name, stage_arc) in resolved {
        let mut result = RetryStageResult {
            stage: name.clone(),
            reset_cleared_state: false,
            submitted: false,
            error: None,
        };
        let pre = pre_reset_signal(state, book_id, &name).await;
        if let Err(e) = stage_arc.reset(&stage_ctx, ab_core::BookId(book_id)).await {
            result.error = Some(format!("reset failed: {e}"));
            tracing::warn!(book_id, stage = %name, error = %e, "api.retry.reset_failed");
            results.push(result);
            continue;
        }
        let post = pre_reset_signal(state, book_id, &name).await;
        result.reset_cleared_state = pre > post;

        let stage_id = ab_pipeline::StageId::new(
            state
                .inner
                .dag
                .known_stage_names()
                .into_iter()
                .find(|s| *s == name.as_str())
                .unwrap_or(""),
        );
        match state
            .inner
            .scheduler
            .submit(
                ab_core::BookId(book_id),
                stage_id,
                ab_pipeline::Priority::Background,
            )
            .await
        {
            Ok(()) => result.submitted = true,
            Err(e) => {
                result.error = Some(format!("submit failed: {e}"));
                tracing::warn!(book_id, stage = %name, error = %e, "api.retry.submit_failed");
            }
        }
        results.push(result);
    }
    results
}

/// Best-effort row counter across the three storage tiers
/// `Stage::reset` sweeps. Used to derive the
/// `reset_cleared_state` flag without threading it through
/// the trait method. Errors are swallowed (counted as 0); the
/// flag is observability, not correctness.
async fn pre_reset_signal(api_state: &ApiState, book_id: i64, stage_name: &str) -> i64 {
    let mut total: i64 = 0;
    if let Ok(Some(n)) = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM pipeline_progress WHERE book_id = ? AND stage = ?",
    )
    .bind(book_id)
    .bind(stage_name)
    .fetch_optional(api_state.inner.ephemeral.pool())
    .await
    {
        total += n;
    }
    if let Ok(Some(n)) = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM book_field_provenance WHERE book_id = ? AND stage = ?",
    )
    .bind(book_id)
    .bind(stage_name)
    .fetch_optional(api_state.inner.library.pool())
    .await
    {
        total += n;
    }
    if let Some(keys) = ab_core::cache_keys_for_stage(stage_name) {
        for key in keys {
            if let Ok(Some(n)) = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM ai_cache WHERE book_id = ? AND cache_type = ?",
            )
            .bind(book_id)
            .bind(key.as_str())
            .fetch_optional(api_state.inner.library.pool())
            .await
            {
                total += n;
            }
        }
    }
    total
}

// ── /books/{book_id}/audiologo ────────────────────────────────

/// Body for `POST /api/v1/books/{book_id}/audiologo` (ADR-0024
/// slice 4A).
///
/// Operator-driven manual cut. The endpoint inserts a row at
/// `book_file_audiologos.status='applied'`, `method='manual'`,
/// then recomputes `books.duration_ms` and shifts the affected
/// `chapters` rows (the chapter-shift maths is encapsulated in
/// [`apply_audiologo_cut`]).
///
/// `add_fingerprint=true` additionally samples the audio at
/// `[jingle_start_ms, jingle_end_ms]` and inserts an
/// `audiologos` row with `verified_via='manual'` so the cut
/// becomes reusable across the library. The fingerprint
/// sampling code itself lives in slice 4B; for 4A the field is
/// accepted but the fingerprint insert is deferred (logged +
/// returns `audiologo_id: null`).
/// Intro vs. outro discriminator on the audiologo-cut request
/// body. Serde-validated at deserialise time — invalid strings
/// surface as a structured 400 from axum before the handler
/// runs, dropping the manual `if req.kind != "intro" && ...`
/// check. (Cross-model code review REVIEW.md § 3.6.)
#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AudiologoKind {
    /// Cut at the start of the file.
    Intro,
    /// Cut at the end of the file.
    Outro,
}

impl AudiologoKind {
    /// String form used in DB writes + downstream APIs that
    /// still take `&str` (slated for typed migration of their
    /// own in a follow-up slice).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Intro => "intro",
            Self::Outro => "outro",
        }
    }
}

#[derive(Deserialize, Debug)]
struct AudiologoCutRequest {
    /// Whether the cut applies at the start (`intro`) or end
    /// (`outro`) of the file. Lowercase only; serde rejects
    /// `Intro` / `OUTRO` with a structured 400.
    kind: AudiologoKind,
    /// Where the jingle begins, ms from file start.
    jingle_start_ms: i64,
    /// Where the jingle ends, ms from file start.
    jingle_end_ms: i64,
    /// Optional padding override; NULL = use
    /// `AudiologoTunables.{intro|outro}_padding_ms`.
    padding_ms: Option<i64>,
    /// When true, sample + fingerprint the range and insert
    /// into `audiologos`. Deferred to 4B; 4A accepts the
    /// field but does not insert.
    #[serde(default)]
    add_fingerprint: bool,
    /// Optional `file_id` — when omitted, applies to the
    /// first file for intros, last file for outros.
    file_id: Option<i64>,
}

/// Successful response body.
#[derive(Serialize)]
struct AudiologoCutResponse {
    book_id: i64,
    file_id: i64,
    kind: AudiologoKind,
    row_id: i64,
    /// `null` when `add_fingerprint=false` or when the
    /// fingerprint insert is deferred (4A).
    audiologo_id: Option<i64>,
    /// `true` when the operator asked for the cut to be
    /// fingerprinted (`add_fingerprint=true`) but the actual
    /// sample-and-hash pass is deferred to slice 4B. Surfaces
    /// in the CLI so the operator knows the request was
    /// recorded but not yet fulfilled.
    fingerprint_deferred: bool,
    /// How many `chapters` rows were shifted by this cut.
    chapters_shifted: i64,
    /// `books.duration_ms` after the cut applied.
    new_duration_ms: Option<i64>,
}

/// Look up the target `file_id` given an optional explicit
/// override + the kind. For intros (no override): file with
/// minimum `file_id` ordered ascending. For outros
/// (no override): file with maximum `file_id`. Validated
/// against `book_files` joined to `books`.
async fn resolve_target_file(
    state: &ApiState,
    book_id: i64,
    kind: &str,
    explicit_file_id: Option<i64>,
) -> Result<Option<i64>, ApiError> {
    if let Some(file_id) = explicit_file_id {
        let row = sqlx::query_scalar!(
            r#"SELECT file_id AS "file_id!" FROM book_files
               WHERE file_id = ? AND book_id = ?"#,
            file_id,
            book_id,
        )
        .fetch_optional(state.inner.library.pool())
        .await
        .map_err(|e| ab_core::Error::Database(format!("audiologo file lookup: {e}")))?;
        return Ok(row);
    }

    // Default: file[0] for intro, file[N-1] for outro. Order
    // by file_id ascending; the scan stage inserts them in
    // path-sorted order so this matches the playback sequence.
    let row = if kind == "outro" {
        sqlx::query_scalar!(
            r#"SELECT file_id AS "file_id!" FROM book_files
               WHERE book_id = ?
               ORDER BY file_id DESC LIMIT 1"#,
            book_id,
        )
        .fetch_optional(state.inner.library.pool())
        .await
    } else {
        sqlx::query_scalar!(
            r#"SELECT file_id AS "file_id!" FROM book_files
               WHERE book_id = ?
               ORDER BY file_id ASC LIMIT 1"#,
            book_id,
        )
        .fetch_optional(state.inner.library.pool())
        .await
    };
    row.map_err(|e| ab_core::Error::Database(format!("audiologo default file: {e}")).into())
}

/// `POST /api/v1/books/{book_id}/audiologo`. Manual cut path
/// from ADR-0024. The fingerprint insert side (when
/// `add_fingerprint=true`) is deferred to slice 4B; this 4A
/// endpoint records the cut and shifts chapters but logs a
/// deferred note when fingerprinting is requested.
async fn books_audiologo_cut(
    State(state): State<ApiState>,
    Path(book_id): Path<i64>,
    Json(req): Json<AudiologoCutRequest>,
) -> Result<Json<AudiologoCutResponse>, ApiError> {
    // `kind` is now a typed `AudiologoKind` enum — serde
    // rejects invalid strings at deserialise time, so the
    // previous manual `if req.kind != "intro" ...` runtime
    // check is gone.
    let kind_str = req.kind.as_str();
    if req.jingle_start_ms < 0 || req.jingle_end_ms <= req.jingle_start_ms {
        return Err(ApiError::BadRequest(format!(
            "jingle_start_ms ({}) must be >= 0 and < jingle_end_ms ({})",
            req.jingle_start_ms, req.jingle_end_ms,
        )));
    }

    // Validate book.
    let book_known =
        sqlx::query_scalar!(r#"SELECT 1 AS "ok!" FROM books WHERE book_id = ?"#, book_id,)
            .fetch_optional(state.inner.library.pool())
            .await
            .map_err(|e| ab_core::Error::Database(format!("audiologo book lookup: {e}")))?;
    if book_known.is_none() {
        return Err(ApiError::NotFound(format!("book_id {book_id} unknown")));
    }

    // Resolve the target file.
    let Some(file_id) = resolve_target_file(&state, book_id, kind_str, req.file_id).await? else {
        return Err(ApiError::BadRequest(format!(
            "book {book_id} has no files matching the request",
        )));
    };

    // 409 pre-check: refuse if there's already an 'applied'
    // row for this (file_id, kind). The schema's partial
    // UNIQUE index would block the insert anyway, but it
    // surfaces as an opaque sqlx error; this pre-check gives
    // the operator a clear "reject or re-detect first" message.
    let existing_applied = sqlx::query_scalar!(
        r#"SELECT audiologo_row_id AS "id!: i64"
           FROM book_file_audiologos
           WHERE file_id = ? AND kind = ? AND status = 'applied'"#,
        file_id,
        kind_str,
    )
    .fetch_optional(state.inner.library.pool())
    .await
    .map_err(|e| ab_core::Error::Database(format!("audiologo applied lookup: {e}")))?;
    if let Some(existing_id) = existing_applied {
        return Err(ApiError::Conflict(format!(
            "file_id {file_id} kind={kind_str} already has an applied cut (row_id={existing_id}); \
             reject or re-detect the existing cut before applying a new one",
        )));
    }

    // Insert the cut + shift chapters + recompute duration.
    // Encapsulated in `crate::audiologo_apply::apply_audiologo_cut`
    // so the catalog-bootstrap path (4B) can reuse the same
    // maths AND so the chapter-shift logic is reachable from
    // integration tests without spinning up the full router.
    let outcome = apply_audiologo_cut(
        state.inner.library.pool(),
        ApplyCutParams {
            book_id,
            file_id,
            kind: kind_str,
            jingle_start_ms: req.jingle_start_ms,
            jingle_end_ms: req.jingle_end_ms,
            padding_ms: req.padding_ms,
            method: ab_audiologo::Method::Manual.as_str(),
            audiologo_id: None, // deferred fingerprint insert
            confidence: 1.0,
            // Manual cuts default to "always pad" — operator
            // hasn't told us the boundary lands in natural
            // silence. Per-row override could be exposed via the
            // POST body in a later slice if operators want to
            // skip synthetic silence on hand-tuned cuts.
            head_silence_ms: 500,
            tail_silence_ms: 1500,
            head_lands_in_silence: false,
            tail_lands_in_silence: false,
        },
    )
    .await?;

    if req.add_fingerprint {
        // Slice 4B wires the actual sample+fingerprint pass.
        // 4A logs that the fingerprint persistence was
        // requested so the eventual 4B run can pick up the
        // deferred work (or the operator can re-issue the
        // cut with --add-fingerprint once 4B ships).
        tracing::info!(
            book_id,
            file_id,
            kind = kind_str,
            jingle_start_ms = req.jingle_start_ms,
            jingle_end_ms = req.jingle_end_ms,
            "audiologo.manual.add_fingerprint_deferred_to_4b",
        );
    }

    Ok(Json(AudiologoCutResponse {
        book_id,
        file_id,
        kind: req.kind,
        row_id: outcome.row_id,
        audiologo_id: None,
        fingerprint_deferred: req.add_fingerprint,
        chapters_shifted: outcome.chapters_shifted,
        new_duration_ms: outcome.new_duration_ms,
    }))
}
