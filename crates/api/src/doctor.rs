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
}
