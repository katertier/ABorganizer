//! Doctor check registry (ADR-0037, slice B.9).
//!
//! Health checks live behind the [`DoctorCheck`] trait so adding a
//! new check is "impl + register" without touching the router. Two
//! checks ship in this slice: `speech` and `llm`. Future checks
//! (chapters, config, library, schema, audio, companions, pipeline)
//! join the registry in their owning slices.
//!
//! Read-only by contract: [`CheckCtx`] exposes only pool handles,
//! so an implementation literally cannot mutate state.

use std::sync::Arc;

use ab_db::{EphemeralDb, LibraryDb};
use async_trait::async_trait;
use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;

use crate::error::ApiError;
use crate::state::ApiState;

/// Overall verdict for one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Ok,
    Warning,
    Failure,
}

/// One structured finding inside a [`CheckReport`].
#[derive(Debug, Clone, Serialize)]
pub struct CheckFinding {
    pub severity: CheckStatus,
    pub message: String,
    /// Optional remediation hint ("run X to fix").
    pub remediation: Option<String>,
    /// Optional deep link into operator docs.
    pub doc_url: Option<String>,
}

/// Result of running one check.
#[derive(Debug, Clone, Serialize)]
pub struct CheckReport {
    pub status: CheckStatus,
    pub summary: String,
    pub details: Vec<CheckFinding>,
}

impl CheckReport {
    pub fn ok(summary: impl Into<String>) -> Self {
        Self {
            status: CheckStatus::Ok,
            summary: summary.into(),
            details: Vec::new(),
        }
    }

    pub fn warn(summary: impl Into<String>) -> Self {
        Self {
            status: CheckStatus::Warning,
            summary: summary.into(),
            details: Vec::new(),
        }
    }

    pub fn fail(summary: impl Into<String>) -> Self {
        Self {
            status: CheckStatus::Failure,
            summary: summary.into(),
            details: Vec::new(),
        }
    }
}

/// Context exposed to every check. Pool handles only — checks
/// cannot mutate state because no mutable surface is reachable
/// here.
#[derive(Clone)]
pub struct CheckCtx {
    pub library: LibraryDb,
    pub ephemeral: EphemeralDb,
}

/// One read-only health check.
#[async_trait]
pub trait DoctorCheck: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    async fn run(&self, ctx: &CheckCtx) -> CheckReport;
}

/// Cheap-to-clone registry; loops + handlers share one instance.
#[derive(Clone)]
pub struct DoctorRegistry {
    checks: Arc<Vec<Arc<dyn DoctorCheck>>>,
}

impl DoctorRegistry {
    #[must_use]
    pub fn new(checks: Vec<Arc<dyn DoctorCheck>>) -> Self {
        Self {
            checks: Arc::new(checks),
        }
    }

    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.checks.iter().map(|c| c.name()).collect()
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn DoctorCheck>> {
        self.checks.iter().find(|c| c.name() == name).cloned()
    }

    pub async fn run_all(&self, ctx: &CheckCtx) -> Vec<(&'static str, CheckReport)> {
        let mut out = Vec::with_capacity(self.checks.len());
        for c in self.checks.iter() {
            out.push((c.name(), c.run(ctx).await));
        }
        out
    }
}

impl std::fmt::Debug for DoctorRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DoctorRegistry")
            .field(
                "checks",
                &self.checks.iter().map(|c| c.name()).collect::<Vec<_>>(),
            )
            .finish()
    }
}

// ── Two starter checks ─────────────────────────────────────────────

/// `llm` — Foundation Models availability + reason.
pub struct LlmCheck;

#[async_trait]
impl DoctorCheck for LlmCheck {
    fn name(&self) -> &'static str {
        "llm"
    }
    fn description(&self) -> &'static str {
        "Apple Intelligence Foundation Models availability"
    }
    async fn run(&self, _ctx: &CheckCtx) -> CheckReport {
        match ab_foundation_models::status().await {
            Ok(report) if report.available => CheckReport::ok("Foundation Models available"),
            Ok(report) => {
                let reason = report
                    .reason
                    .map_or_else(|| "unavailable".to_owned(), |r| format!("{r:?}"));
                let mut r = CheckReport::warn(format!("Foundation Models unavailable: {reason}"));
                r.details.push(CheckFinding {
                    severity: CheckStatus::Warning,
                    message: reason,
                    remediation: Some(
                        "Confirm Apple Intelligence is enabled in System Settings → Apple Intelligence & Siri.".into(),
                    ),
                    doc_url: None,
                });
                r
            }
            Err(e) => {
                let mut r = CheckReport::fail("Foundation Models bridge unreachable");
                r.details.push(CheckFinding {
                    severity: CheckStatus::Failure,
                    message: e.to_string(),
                    remediation: Some(
                        "Rebuild ab-foundation-models; verify macOS 26 + Apple Silicon.".into(),
                    ),
                    doc_url: None,
                });
                r
            }
        }
    }
}

/// `speech` — `SpeechAnalyzer` probe via the `en-US` locale.
pub struct SpeechCheck;

#[async_trait]
impl DoctorCheck for SpeechCheck {
    fn name(&self) -> &'static str {
        "speech"
    }
    fn description(&self) -> &'static str {
        "SpeechAnalyzer availability + per-locale install state"
    }
    async fn run(&self, _ctx: &CheckCtx) -> CheckReport {
        match ab_speech::speech_locale_status("en-US").await {
            Ok(report) if report.status == "installed" => {
                CheckReport::ok("SpeechAnalyzer ready (en-US installed)")
            }
            Ok(report) => {
                CheckReport::warn(format!("SpeechAnalyzer en-US status: {}", report.status))
            }
            Err(e) => {
                let mut r = CheckReport::fail("SpeechAnalyzer bridge unreachable");
                r.details.push(CheckFinding {
                    severity: CheckStatus::Failure,
                    message: e.to_string(),
                    remediation: Some(
                        "Verify the SpeechAnalyzer Swift FFI builds and macOS supports it.".into(),
                    ),
                    doc_url: None,
                });
                r
            }
        }
    }
}

/// `journal` — `operation_journal` health.
///
/// Counts `pending` rows. After daemon startup PR #170's
/// `recover_pending` pass should have flushed every pending row to
/// `failed`, so a non-zero count here means either (a) the daemon
/// is mid-batch and these are live in-flight ops, or (b) something
/// is wedged and the operator should investigate. Either way it's
/// a `Warning` rather than `Failure` — `pending` is a normal
/// transient state for an active daemon.
pub struct JournalCheck;

#[async_trait]
impl DoctorCheck for JournalCheck {
    fn name(&self) -> &'static str {
        "journal"
    }
    fn description(&self) -> &'static str {
        "operation_journal pending-row count (crash-recovery surface, ADR-0039)"
    }
    async fn run(&self, ctx: &CheckCtx) -> CheckReport {
        let pending: i64 = match sqlx::query_scalar!(
            "SELECT COUNT(*) FROM operation_journal WHERE progress = 'pending'",
        )
        .fetch_one(ctx.library.pool())
        .await
        {
            Ok(n) => n,
            Err(e) => {
                let mut r = CheckReport::fail("operation_journal pending count failed");
                r.details.push(CheckFinding {
                    severity: CheckStatus::Failure,
                    message: e.to_string(),
                    remediation: Some(
                        "Inspect ab-db logs; verify the library DB is reachable.".into(),
                    ),
                    doc_url: None,
                });
                return r;
            }
        };
        if pending == 0 {
            return CheckReport::ok("no pending journal rows");
        }
        let mut r = CheckReport::warn(format!("{pending} pending journal row(s)"));
        r.details.push(CheckFinding {
            severity: CheckStatus::Warning,
            message: format!(
                "{pending} operation_journal row(s) in 'pending' state. Normal for an active \
                 daemon mid-batch; suspicious after a clean restart."
            ),
            remediation: Some(
                "If the daemon was restarted, the startup recover_pending pass should have \
                 flipped these to 'failed'. Re-check after a clean restart; if the count \
                 persists, file an issue with the affected op_kind values."
                    .into(),
            ),
            doc_url: None,
        });
        r
    }
}

/// `failed-ops` — count of `progress='failed'` rows whose
/// `failed_reason` is NOT the crash-recovery sentinel.
///
/// Splits "real pipeline failure" from "crash-recovery flushed
/// the row at startup": the latter is informational (the daemon
/// restarted mid-batch and #170's startup pass did its job), the
/// former is a genuine triage signal. Filter via the public
/// constant `ab_journal::RECOVERY_FAILED_REASON` so the prefix
/// stays in sync if the message ever changes.
pub struct FailedOpsCheck;

#[async_trait]
impl DoctorCheck for FailedOpsCheck {
    fn name(&self) -> &'static str {
        "failed-ops"
    }
    fn description(&self) -> &'static str {
        "operation_journal rows with progress='failed' from a real pipeline failure (excludes crash-recovery flushes)"
    }
    async fn run(&self, ctx: &CheckCtx) -> CheckReport {
        let recovery_reason = ab_journal::RECOVERY_FAILED_REASON;
        let failed: i64 = match sqlx::query_scalar!(
            "SELECT COUNT(*) FROM operation_journal \
             WHERE progress = 'failed' \
               AND (failed_reason IS NULL OR failed_reason != ?)",
            recovery_reason,
        )
        .fetch_one(ctx.library.pool())
        .await
        {
            Ok(n) => n,
            Err(e) => {
                let mut r = CheckReport::fail("operation_journal failed-ops count failed");
                r.details.push(CheckFinding {
                    severity: CheckStatus::Failure,
                    message: e.to_string(),
                    remediation: Some(
                        "Inspect ab-db logs; verify the library DB is reachable.".into(),
                    ),
                    doc_url: None,
                });
                return r;
            }
        };
        if failed == 0 {
            return CheckReport::ok("no failed-op rows (excluding crash-recovery flushes)");
        }
        let mut r = CheckReport::warn(format!("{failed} failed-op row(s)"));
        r.details.push(CheckFinding {
            severity: CheckStatus::Warning,
            message: format!(
                "{failed} operation_journal row(s) with progress='failed' from real pipeline \
                 failures. Use GET /api/v1/operation_journal?progress=failed to triage; group \
                 by op_kind to find recurring failure points."
            ),
            remediation: Some(
                "Inspect failed_reason per row. Re-queue via per-book retry if the underlying \
                 cause is transient (network, file lock); otherwise file an issue with the \
                 op_kind + failed_reason."
                    .into(),
            ),
            doc_url: None,
        });
        r
    }
}

/// `orphan-companions` — count of `book_companions` rows with no
/// paired audiobook (`book_id IS NULL`).
///
/// Orphans are persistent by design (ADR-0043 § "CASCADE +
/// retention" — true orphans never auto-delete). A high count
/// here means the operator has companion files in their library
/// that the auto-pair geometry couldn't claim — usually because
/// they're in an ambiguous directory or alongside multiple
/// audiobooks the system can't disambiguate without operator
/// input.
pub struct OrphanCompanionsCheck;

#[async_trait]
impl DoctorCheck for OrphanCompanionsCheck {
    fn name(&self) -> &'static str {
        "orphan-companions"
    }
    fn description(&self) -> &'static str {
        "book_companions rows with no paired audiobook (book_id IS NULL)"
    }
    async fn run(&self, ctx: &CheckCtx) -> CheckReport {
        let orphans: i64 = match sqlx::query_scalar!(
            "SELECT COUNT(*) FROM book_companions WHERE book_id IS NULL",
        )
        .fetch_one(ctx.library.pool())
        .await
        {
            Ok(n) => n,
            Err(e) => {
                let mut r = CheckReport::fail("book_companions orphan count failed");
                r.details.push(CheckFinding {
                    severity: CheckStatus::Failure,
                    message: e.to_string(),
                    remediation: Some(
                        "Inspect ab-db logs; verify the library DB is reachable.".into(),
                    ),
                    doc_url: None,
                });
                return r;
            }
        };
        if orphans == 0 {
            return CheckReport::ok("no orphan companions");
        }
        let mut r = CheckReport::warn(format!("{orphans} orphan companion(s)"));
        r.details.push(CheckFinding {
            severity: CheckStatus::Warning,
            message: format!(
                "{orphans} book_companions row(s) with no paired audiobook. Either the \
                 auto-pair geometry couldn't claim them (companion in an ambiguous dir or \
                 alongside multiple audiobooks) or the companion is a true orphan."
            ),
            remediation: Some(
                "Inspect the companion_nearby_books junction-hint table per orphan to find \
                 candidate audiobooks; pair manually via the (future) operator UI, or leave \
                 as orphan if it's a standalone download."
                    .into(),
            ),
            doc_url: None,
        });
        r
    }
}

/// `library-roots-reachable` — every active `library_roots.path` must exist
/// on disk and be a directory.
///
/// Catches the operator's "external drive unmounted, the daemon is still
/// pointing at the old SMB share" failure mode. Read-only: just `stat()`
/// per path.
pub struct LibraryRootsReachableCheck;

#[async_trait]
impl DoctorCheck for LibraryRootsReachableCheck {
    fn name(&self) -> &'static str {
        "library-roots-reachable"
    }
    fn description(&self) -> &'static str {
        "every active library_roots.path exists on disk and is a directory"
    }
    async fn run(&self, ctx: &CheckCtx) -> CheckReport {
        let rows = match sqlx::query!(
            r#"SELECT root_id AS "root_id!: i64", path, label
             FROM library_roots
             WHERE is_active = 1
             ORDER BY root_id"#
        )
        .fetch_all(ctx.library.pool())
        .await
        {
            Ok(r) => r,
            Err(e) => {
                let mut r = CheckReport::fail("library_roots query failed");
                r.details.push(CheckFinding {
                    severity: CheckStatus::Failure,
                    message: e.to_string(),
                    remediation: Some(
                        "Inspect ab-db logs; verify the library DB is reachable.".into(),
                    ),
                    doc_url: None,
                });
                return r;
            }
        };
        if rows.is_empty() {
            return CheckReport::ok("no active library roots configured");
        }
        let mut missing = Vec::new();
        let mut not_dir = Vec::new();
        for row in &rows {
            let p = std::path::Path::new(&row.path);
            match std::fs::symlink_metadata(p) {
                Ok(md) if md.is_dir() => {}
                Ok(_) => not_dir.push((row.root_id, row.path.clone(), row.label.clone())),
                Err(_) => missing.push((row.root_id, row.path.clone(), row.label.clone())),
            }
        }
        if missing.is_empty() && not_dir.is_empty() {
            return CheckReport::ok(format!("{} library root(s) reachable", rows.len()));
        }
        let mut r = CheckReport::warn(format!(
            "{} of {} library root(s) unreachable",
            missing.len() + not_dir.len(),
            rows.len()
        ));
        for (root_id, path, label) in &missing {
            r.details.push(CheckFinding {
                severity: CheckStatus::Warning,
                message: format!(
                    "library_roots root_id={root_id} label={label:?} path={path:?} not found"
                ),
                remediation: Some(
                    "Mount the source volume, or DELETE the root via the library_roots API if \
                     it's gone for good."
                        .into(),
                ),
                doc_url: None,
            });
        }
        for (root_id, path, label) in &not_dir {
            r.details.push(CheckFinding {
                severity: CheckStatus::Warning,
                message: format!(
                    "library_roots root_id={root_id} label={label:?} path={path:?} exists but \
                     is not a directory"
                ),
                remediation: Some(
                    "Verify the path; if a file was created where a directory belongs, remove \
                     it or correct the library_roots row."
                        .into(),
                ),
                doc_url: None,
            });
        }
        r
    }
}

/// `db-integrity` — runs `PRAGMA integrity_check` against both the library
/// and ephemeral SQLite databases.
///
/// SQLite returns the single string `ok` for a clean DB; any other output
/// is a list of corruption findings (one per line). Cheap to run on small-
/// to-medium DBs (microseconds to single-digit seconds at 100k books per
/// SQLite's own benchmarks); catches B-tree corruption / orphan pages /
/// invalid free-list entries that no application-level check would catch.
pub struct DbIntegrityCheck;

#[async_trait]
impl DoctorCheck for DbIntegrityCheck {
    fn name(&self) -> &'static str {
        "db-integrity"
    }
    fn description(&self) -> &'static str {
        "PRAGMA integrity_check on the library + ephemeral SQLite databases"
    }
    async fn run(&self, ctx: &CheckCtx) -> CheckReport {
        let library = pragma_integrity(ctx.library.pool(), "library").await;
        let ephemeral = pragma_integrity(ctx.ephemeral.pool(), "ephemeral").await;
        let mut findings = Vec::new();
        let mut summary_parts = Vec::new();
        let mut overall = CheckStatus::Ok;
        for (label, result) in [("library", library), ("ephemeral", ephemeral)] {
            match result {
                Ok(messages) if messages.iter().all(|m| m == "ok") => {
                    summary_parts.push(format!("{label} ok"));
                }
                Ok(messages) => {
                    overall = CheckStatus::Failure;
                    summary_parts.push(format!("{label} corrupt"));
                    for m in messages {
                        findings.push(CheckFinding {
                            severity: CheckStatus::Failure,
                            message: format!("{label}: {m}"),
                            remediation: Some(
                                "Restore from the most recent backup; investigate root cause \
                                 (disk error, abrupt shutdown). If recent writes are \
                                 unrecoverable, dump the DB with `.dump` and re-create."
                                    .into(),
                            ),
                            doc_url: None,
                        });
                    }
                }
                Err(e) => {
                    overall = CheckStatus::Failure;
                    summary_parts.push(format!("{label} query failed"));
                    findings.push(CheckFinding {
                        severity: CheckStatus::Failure,
                        message: format!("{label}: {e}"),
                        remediation: Some(
                            "Check the DB is reachable; inspect ab-db logs for open errors.".into(),
                        ),
                        doc_url: None,
                    });
                }
            }
        }
        let summary = summary_parts.join(", ");
        let mut report = match overall {
            CheckStatus::Ok => CheckReport::ok(summary),
            CheckStatus::Warning => CheckReport::warn(summary),
            CheckStatus::Failure => CheckReport::fail(summary),
        };
        report.details = findings;
        report
    }
}

async fn pragma_integrity(
    pool: &sqlx::SqlitePool,
    _label: &str,
) -> Result<Vec<String>, sqlx::Error> {
    // PRAGMA integrity_check returns one row per finding. A clean DB returns
    // a single row with the value "ok". Built at runtime (sqlx::query!
    // macro can't bind PRAGMA statements meaningfully) — fine here, the
    // SQL is a fixed literal with no user input.
    let rows: Vec<(String,)> = sqlx::query_as("PRAGMA integrity_check")
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(s,)| s).collect())
}

/// `ai-cache-size` — `ai_cache` row count + total content bytes.
///
/// Catches runaway cache growth (transcript blobs, DNA-tag JSON,
/// per-chapter caches) before it eats the operator's disk.
///
/// Status is `Ok` until total bytes exceed [`AI_CACHE_BUDGET_BYTES`]
/// (5 `GiB`) — then `Warning`, with the actual figure in the summary.
/// `Failure` only when the COUNT/SUM query itself errors.
pub struct AiCacheSizeCheck;

/// Soft budget for the `ai_cache` table.
///
/// Crosses → doctor warns. Set well above the 100k-book ceiling's
/// expected cache footprint (transcript caches dominate, ~10 KB
/// compressed per book → ~1 `GiB` at 100k books); 5 `GiB` leaves
/// headroom for `DNA-tags` + samples + future cache types without
/// nagging on healthy libraries.
pub const AI_CACHE_BUDGET_BYTES: i64 = 5 * 1024 * 1024 * 1024;

#[async_trait]
impl DoctorCheck for AiCacheSizeCheck {
    fn name(&self) -> &'static str {
        "ai-cache-size"
    }
    fn description(&self) -> &'static str {
        "ai_cache row count + total content bytes vs soft budget"
    }
    async fn run(&self, ctx: &CheckCtx) -> CheckReport {
        let row = match sqlx::query!(
            r#"SELECT
                COUNT(*) AS "rows!: i64",
                COALESCE(SUM(LENGTH(content)), 0) AS "bytes!: i64"
              FROM ai_cache"#,
        )
        .fetch_one(ctx.library.pool())
        .await
        {
            Ok(r) => r,
            Err(e) => {
                let mut r = CheckReport::fail("ai_cache size query failed");
                r.details.push(CheckFinding {
                    severity: CheckStatus::Failure,
                    message: e.to_string(),
                    remediation: Some(
                        "Inspect ab-db logs; verify the library DB is reachable.".into(),
                    ),
                    doc_url: None,
                });
                return r;
            }
        };
        let mib = row.bytes / (1024 * 1024);
        let budget_mib = AI_CACHE_BUDGET_BYTES / (1024 * 1024);
        if row.bytes <= AI_CACHE_BUDGET_BYTES {
            return CheckReport::ok(format!(
                "ai_cache: {rows} row(s), {mib} MiB (budget {budget_mib} MiB)",
                rows = row.rows,
            ));
        }
        let mut r = CheckReport::warn(format!(
            "ai_cache: {rows} row(s), {mib} MiB exceeds budget {budget_mib} MiB",
            rows = row.rows,
        ));
        r.details.push(CheckFinding {
            severity: CheckStatus::Warning,
            message: format!(
                "ai_cache content totals {mib} MiB across {rows} row(s); soft budget is \
                 {budget_mib} MiB. Most space is usually transcript caches (compressed); \
                 DNA-tag + sample caches are small.",
                rows = row.rows,
            ),
            remediation: Some(
                "If growth is unexpected, audit cache_type distribution \
                 (SELECT cache_type, COUNT(*), SUM(LENGTH(content)) FROM ai_cache GROUP BY \
                 cache_type ORDER BY 3 DESC) — a single cache_type dominating may indicate \
                 a stuck pipeline rerun loop. Cleanup options: drop transient cache_types \
                 (`DELETE FROM ai_cache WHERE cache_type = 'samples'` etc.); the stages \
                 that fill them will re-cache on demand."
                    .into(),
            ),
            doc_url: None,
        });
        r
    }
}

// ── HTTP surface ──────────────────────────────────────────────────

#[derive(Serialize)]
pub struct DoctorIndexEntry {
    pub name: &'static str,
    pub description: &'static str,
}

#[derive(Serialize)]
pub struct DoctorIndexResponse {
    pub checks: Vec<DoctorIndexEntry>,
}

/// `GET /api/v1/doctor` — registry index.
pub async fn doctor_index(State(state): State<ApiState>) -> Json<DoctorIndexResponse> {
    let checks = state
        .inner
        .doctor
        .checks
        .iter()
        .map(|c| DoctorIndexEntry {
            name: c.name(),
            description: c.description(),
        })
        .collect();
    Json(DoctorIndexResponse { checks })
}

#[derive(Serialize)]
pub struct DoctorAllResponse {
    pub reports: Vec<NamedReport>,
}

#[derive(Serialize)]
pub struct NamedReport {
    pub name: &'static str,
    pub report: CheckReport,
}

/// `GET /api/v1/doctor/all` — run every registered check.
pub async fn doctor_all(State(state): State<ApiState>) -> Json<DoctorAllResponse> {
    let ctx = CheckCtx {
        library: state.inner.library.clone(),
        ephemeral: state.inner.ephemeral.clone(),
    };
    let reports = state
        .inner
        .doctor
        .run_all(&ctx)
        .await
        .into_iter()
        .map(|(name, report)| NamedReport { name, report })
        .collect();
    Json(DoctorAllResponse { reports })
}

/// `GET /api/v1/doctor/{name}` — run a single registered check.
pub async fn doctor_one(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> Result<Json<CheckReport>, ApiError> {
    let check = state
        .inner
        .doctor
        .get(name.as_str())
        .ok_or_else(|| ApiError::NotFound(format!("doctor check {name}")))?;
    let ctx = CheckCtx {
        library: state.inner.library.clone(),
        ephemeral: state.inner.ephemeral.clone(),
    };
    let report = check.run(&ctx).await;
    Ok(Json(report))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use ab_core::tunables::DbTunables;
    use tempfile::TempDir;

    async fn fresh_ctx() -> (CheckCtx, TempDir) {
        let tmp = TempDir::new().expect("tmpdir");
        let library = LibraryDb::open(&tmp.path().join("library.db"), &DbTunables::default())
            .await
            .expect("open library");
        let ephemeral = EphemeralDb::open(&tmp.path().join("ephemeral.db"), &DbTunables::default())
            .await
            .expect("open ephemeral");
        (CheckCtx { library, ephemeral }, tmp)
    }

    #[tokio::test]
    async fn journal_check_ok_when_no_pending_rows() {
        let (ctx, _tmp) = fresh_ctx().await;
        let report = JournalCheck.run(&ctx).await;
        assert_eq!(report.status, CheckStatus::Ok);
    }

    #[tokio::test]
    async fn journal_check_warns_when_pending_rows_present() {
        let (ctx, _tmp) = fresh_ctx().await;
        sqlx::query(
            "INSERT INTO operation_journal \
                (op_kind, target_kind, target_id, pre_state_json, progress) \
              VALUES ('tag-write-final', 'book', 1, '{}', 'pending'), \
                     ('batch-edit', 'book', 2, '{}', 'pending')",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed pending");
        let report = JournalCheck.run(&ctx).await;
        assert_eq!(report.status, CheckStatus::Warning);
        assert!(
            report.summary.contains('2'),
            "summary should mention 2 pending rows: {}",
            report.summary
        );
    }

    #[tokio::test]
    async fn journal_check_ignores_done_failed_reversed_rows() {
        let (ctx, _tmp) = fresh_ctx().await;
        sqlx::query(
            "INSERT INTO operation_journal \
                (op_kind, target_kind, target_id, pre_state_json, progress) \
              VALUES ('a', 'book', 1, '{}', 'done'), \
                     ('b', 'book', 2, '{}', 'failed'), \
                     ('c', 'book', 3, '{}', 'reversed')",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed terminal rows");
        let report = JournalCheck.run(&ctx).await;
        assert_eq!(report.status, CheckStatus::Ok);
    }

    #[tokio::test]
    async fn failed_ops_check_ok_when_no_failed_rows() {
        let (ctx, _tmp) = fresh_ctx().await;
        let report = FailedOpsCheck.run(&ctx).await;
        assert_eq!(report.status, CheckStatus::Ok);
    }

    #[tokio::test]
    async fn failed_ops_check_warns_only_on_real_failures() {
        let (ctx, _tmp) = fresh_ctx().await;
        // One real failure + one crash-recovery flush (with the
        // canonical reason). Only the real failure should count.
        sqlx::query(
            "INSERT INTO operation_journal \
                (op_kind, target_kind, target_id, pre_state_json, progress, failed_reason) \
              VALUES (?, 'book', 1, '{}', 'failed', 'network timeout'), \
                     (?, 'book', 2, '{}', 'failed', ?)",
        )
        .bind("tag-write-final")
        .bind("tag-write-final")
        .bind(ab_journal::RECOVERY_FAILED_REASON)
        .execute(ctx.library.pool())
        .await
        .expect("seed failed rows");
        let report = FailedOpsCheck.run(&ctx).await;
        assert_eq!(report.status, CheckStatus::Warning);
        assert!(
            report.summary.contains('1'),
            "summary should mention 1 real failure: {}",
            report.summary
        );
    }

    #[tokio::test]
    async fn orphan_companions_check_ok_when_all_paired() {
        let (ctx, _tmp) = fresh_ctx().await;
        // Seed a book + a paired companion. No orphans.
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 't')")
            .execute(ctx.library.pool())
            .await
            .expect("seed book");
        sqlx::query(
            "INSERT INTO book_companions \
                (book_id, path, format, parse_tier, content_hash, bytes, discovered_at) \
              VALUES (1, '/x.epub', 'epub', 'text_extractable', 'h', 1, 0)",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed paired companion");
        let report = OrphanCompanionsCheck.run(&ctx).await;
        assert_eq!(report.status, CheckStatus::Ok);
    }

    #[tokio::test]
    async fn orphan_companions_check_warns_when_orphans_present() {
        let (ctx, _tmp) = fresh_ctx().await;
        sqlx::query(
            "INSERT INTO book_companions \
                (book_id, path, format, parse_tier, content_hash, bytes, discovered_at) \
              VALUES (NULL, '/orphan1.pdf', 'pdf', 'document', 'h1', 1, 0), \
                     (NULL, '/orphan2.epub', 'epub', 'text_extractable', 'h2', 1, 0)",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed orphans");
        let report = OrphanCompanionsCheck.run(&ctx).await;
        assert_eq!(report.status, CheckStatus::Warning);
        assert!(
            report.summary.contains('2'),
            "summary should mention 2 orphans: {}",
            report.summary
        );
    }

    #[tokio::test]
    async fn library_roots_reachable_check_ok_when_no_roots() {
        let (ctx, _tmp) = fresh_ctx().await;
        let report = LibraryRootsReachableCheck.run(&ctx).await;
        assert_eq!(report.status, CheckStatus::Ok);
        assert!(
            report.summary.contains("no active"),
            "summary: {}",
            report.summary
        );
    }

    #[tokio::test]
    async fn library_roots_reachable_check_ok_when_all_paths_exist() {
        let (ctx, tmp) = fresh_ctx().await;
        let root_a = tmp.path().join("a");
        let root_b = tmp.path().join("b");
        std::fs::create_dir_all(&root_a).expect("mkdir a");
        std::fs::create_dir_all(&root_b).expect("mkdir b");
        let path_a = root_a.to_string_lossy().to_string();
        let path_b = root_b.to_string_lossy().to_string();
        sqlx::query("INSERT INTO library_roots (path, label) VALUES (?, 'A'), (?, 'B')")
            .bind(&path_a)
            .bind(&path_b)
            .execute(ctx.library.pool())
            .await
            .expect("seed roots");
        let report = LibraryRootsReachableCheck.run(&ctx).await;
        assert_eq!(report.status, CheckStatus::Ok);
        assert!(report.summary.contains('2'), "summary: {}", report.summary);
    }

    #[tokio::test]
    async fn library_roots_reachable_check_warns_on_missing_and_not_dir() {
        let (ctx, tmp) = fresh_ctx().await;
        let real_dir = tmp.path().join("real");
        let plain_file = tmp.path().join("not_a_dir.txt");
        std::fs::create_dir_all(&real_dir).expect("mkdir real");
        std::fs::write(&plain_file, b"x").expect("write file");
        let path_real = real_dir.to_string_lossy().to_string();
        let path_file = plain_file.to_string_lossy().to_string();
        let path_missing = tmp.path().join("vanished").to_string_lossy().to_string();
        sqlx::query(
            "INSERT INTO library_roots (path, label) VALUES (?, 'real'), (?, 'file'), (?, 'gone')",
        )
        .bind(&path_real)
        .bind(&path_file)
        .bind(&path_missing)
        .execute(ctx.library.pool())
        .await
        .expect("seed mixed roots");
        let report = LibraryRootsReachableCheck.run(&ctx).await;
        assert_eq!(report.status, CheckStatus::Warning);
        assert!(
            report.summary.contains("2 of 3"),
            "summary: {}",
            report.summary
        );
        assert_eq!(report.details.len(), 2);
    }

    #[tokio::test]
    async fn library_roots_reachable_check_skips_inactive_rows() {
        let (ctx, tmp) = fresh_ctx().await;
        let path_missing = tmp.path().join("gone").to_string_lossy().to_string();
        sqlx::query("INSERT INTO library_roots (path, label, is_active) VALUES (?, 'soft', 0)")
            .bind(&path_missing)
            .execute(ctx.library.pool())
            .await
            .expect("seed inactive root");
        let report = LibraryRootsReachableCheck.run(&ctx).await;
        assert_eq!(report.status, CheckStatus::Ok);
    }

    #[tokio::test]
    async fn db_integrity_check_ok_for_fresh_db() {
        let (ctx, _tmp) = fresh_ctx().await;
        let report = DbIntegrityCheck.run(&ctx).await;
        assert_eq!(report.status, CheckStatus::Ok);
        assert!(
            report.summary.contains("library ok") && report.summary.contains("ephemeral ok"),
            "summary: {}",
            report.summary
        );
        assert!(report.details.is_empty());
    }

    #[tokio::test]
    async fn ai_cache_size_check_ok_when_empty() {
        let (ctx, _tmp) = fresh_ctx().await;
        let report = AiCacheSizeCheck.run(&ctx).await;
        assert_eq!(report.status, CheckStatus::Ok);
        assert!(
            report.summary.contains("0 row"),
            "summary: {}",
            report.summary
        );
    }

    #[tokio::test]
    async fn ai_cache_size_check_ok_within_budget() {
        let (ctx, _tmp) = fresh_ctx().await;
        // Seed a book + a tiny cache row. Well under budget.
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'b')")
            .execute(ctx.library.pool())
            .await
            .expect("seed book");
        sqlx::query(
            "INSERT INTO ai_cache (book_id, cache_type, content) VALUES (1, 'dna_tags', ?)",
        )
        .bind(vec![0u8; 1024]) // 1 KiB
        .execute(ctx.library.pool())
        .await
        .expect("seed cache row");
        let report = AiCacheSizeCheck.run(&ctx).await;
        assert_eq!(report.status, CheckStatus::Ok);
        assert!(
            report.summary.contains("1 row"),
            "summary: {}",
            report.summary
        );
    }

    #[tokio::test]
    async fn db_integrity_check_runs_after_writes() {
        // Sanity: a DB that has been written to should still report ok.
        let (ctx, _tmp) = fresh_ctx().await;
        for i in 1..=10 {
            sqlx::query("INSERT INTO books (book_id, title) VALUES (?, ?)")
                .bind(i)
                .bind(format!("book {i}"))
                .execute(ctx.library.pool())
                .await
                .expect("insert book");
        }
        let report = DbIntegrityCheck.run(&ctx).await;
        assert_eq!(report.status, CheckStatus::Ok);
    }
}
