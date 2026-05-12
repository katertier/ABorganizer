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
