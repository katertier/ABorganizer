//! Cleanup-subsystem runtime (slice H.2, ADR-0025).
//!
//! Holds the [`CleanupTarget`] trait, the [`CleanupCtx`] every
//! target gets at run time, and the periodic loop spawned by
//! the daemon at startup. Trait + ctx live here (not in
//! `ab_core`) because they reference `LibraryDb` /
//! `EphemeralDb`, which depend on `ab_core` — so the trait
//! has to sit downstream of those.
//!
//! Pure types — [`ab_core::Category`], [`ab_core::Policy`],
//! [`ab_core::CleanupReport`], [`ab_core::compute_age_seconds`]
//! — live in `ab_core::cleanup`, since they're dependency-
//! free.
//!
//! # Periodic loop
//!
//! The loop walks every registered target each tick, asks
//! `report()` to surface what's eligible, logs the count + bytes
//! per target, then asks `apply()` if `auto_apply` is on. v1
//! ships with `auto_apply = false` — the loop is observability
//! only, the operator applies via `aborg clean ... --apply`.
//! A future tick of policy work (per ADR-0025 § auto-apply
//! semantics) flips the default to a per-target opt-in.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use ab_core::cleanup::{Category, CleanupReport, Policy, compute_age_seconds};
use ab_core::tunables::CleanupTunables;
use ab_core::{Error, Result};
use ab_db::{EphemeralDb, LibraryDb};

/// Shared context every target gets at run time. Same shape
/// as `StageContext` but cleanup-scoped: no per-stage name,
/// no cancellation token (the periodic loop already owns
/// cancellation at the loop level).
#[derive(Clone)]
pub struct CleanupCtx {
    /// Persistent library DB. Targets in [`Category::Db`]
    /// usually query against this pool.
    pub library: LibraryDb,
    /// Restartable state DB. Targets in [`Category::Queue`]
    /// (pairing codes, jobs, rate-limits) live here.
    pub ephemeral: EphemeralDb,
}

/// One cleanup target. Each registered target = one
/// implementation of this trait. The periodic loop and the
/// manual `aborg clean ...` endpoints both go through the
/// same trait surface.
#[async_trait]
pub trait CleanupTarget: Send + Sync {
    /// Self-identification: which category this target
    /// belongs to.
    fn category(&self) -> Category;

    /// Stable name surfaced in logs + API responses.
    fn name(&self) -> &'static str;

    /// Dry-run: how many items would be pruned under this
    /// policy, and how many bytes that would free. Never
    /// mutates state.
    ///
    /// # Errors
    ///
    /// Surfaces underlying DB / FS errors. The caller logs +
    /// continues; a broken target doesn't take the whole
    /// cleanup cycle down.
    async fn report(&self, ctx: &CleanupCtx, policy: &Policy) -> Result<CleanupReport>;

    /// Apply the policy. Called only when `policy.apply` is
    /// true. Returns the same shape as `report` — `items` and
    /// `bytes` are the count + bytes ACTUALLY freed, not
    /// estimates.
    ///
    /// # Errors
    ///
    /// See `report`.
    async fn apply(&self, ctx: &CleanupCtx, policy: &Policy) -> Result<CleanupReport>;
}

/// Registered set of targets. The daemon builds this once at
/// startup and hands it to both the periodic loop and the
/// `aborg clean` HTTP handlers.
#[derive(Clone)]
pub struct CleanupRegistry {
    targets: Arc<Vec<Arc<dyn CleanupTarget>>>,
}

impl CleanupRegistry {
    /// Build a registry from a list of trait objects. Order
    /// is preserved for the `aborg clean` summary output.
    #[must_use]
    pub fn new(targets: Vec<Arc<dyn CleanupTarget>>) -> Self {
        Self {
            targets: Arc::new(targets),
        }
    }

    /// Read-only slice over every registered target.
    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn CleanupTarget>> {
        self.targets.iter()
    }

    /// Filter to a single category. Used by the
    /// `aborg clean disk|db|queue` per-category endpoints.
    #[must_use]
    pub fn for_category(&self, cat: Category) -> Vec<Arc<dyn CleanupTarget>> {
        self.targets
            .iter()
            .filter(|t| t.category() == cat)
            .map(Arc::clone)
            .collect()
    }
}

impl std::fmt::Debug for CleanupRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CleanupRegistry")
            .field("targets", &self.targets.len())
            .finish()
    }
}

/// Bundle of long-lived references the cleanup loop needs.
/// Mirrors the dispatcher's `DispatcherCtx` shape so the two
/// loops read symmetrically in daemon-main wiring.
pub struct CleanupLoopCtx {
    /// Library + ephemeral handles for the per-target
    /// queries.
    pub cleanup_ctx: CleanupCtx,
    /// Registered set of targets.
    pub registry: CleanupRegistry,
    /// Periodic-loop interval + age tunables.
    pub tunables: CleanupTunables,
    /// Closure that returns `(free_bytes, total_bytes)` for
    /// the disk that holds [`CleanupCtx::library`]. Boxed so
    /// the daemon can inject a real `statvfs` call while
    /// tests inject a fixed value. `(u64::MAX, u64::MAX)` is
    /// the "no pressure detected" sentinel.
    pub disk_free: Arc<dyn Fn() -> (u64, u64) + Send + Sync>,
}

/// Spawn the periodic cleanup loop.
///
/// Returns when the cancellation token fires. Logs failures
/// via `tracing` and continues — a bad tick must not kill
/// the daemon's only periodic-prune path.
///
/// `tunables.check_secs == 0` disables the loop entirely
/// (operator-triggered cleanup via `aborg clean ...` still
/// works — the registry is shared with the API handlers).
///
/// v1 ships with `auto_apply = false`: the loop reports
/// what's eligible but never deletes. Operator applies via
/// `aborg clean ... --apply`. Per ADR-0025 § auto-apply this
/// is the conservative default; per-target opt-in lands in
/// a follow-up.
pub async fn run_cleanup_loop(ctx: CleanupLoopCtx, cancel: CancellationToken) {
    let CleanupLoopCtx {
        cleanup_ctx,
        registry,
        tunables,
        disk_free,
    } = ctx;
    if tunables.check_secs == 0 {
        tracing::info!("pipeline.cleanup.disabled");
        return;
    }
    let interval = Duration::from_secs(tunables.check_secs);
    tracing::info!(
        check_secs = tunables.check_secs,
        targets = registry.targets.len(),
        "pipeline.cleanup.start"
    );
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                tracing::info!("pipeline.cleanup.stop");
                return;
            }
            () = tokio::time::sleep(interval) => {
                tick(&cleanup_ctx, &registry, &tunables, disk_free.as_ref()).await;
            }
        }
    }
}

/// One full sweep: ask every registered target to report
/// under the current age policy. Errors are logged + skipped;
/// no target failure aborts the rest. Dry-run by default
/// (`auto_apply = false`).
async fn tick(
    cleanup_ctx: &CleanupCtx,
    registry: &CleanupRegistry,
    tunables: &CleanupTunables,
    disk_free: &(dyn Fn() -> (u64, u64) + Send + Sync),
) {
    let (free, total) = disk_free();
    let age_seconds = compute_age_seconds(tunables, free, total);
    let policy = Policy::dry_run(age_seconds);
    let mut total_items: u64 = 0;
    let mut total_bytes: u64 = 0;
    for target in registry.iter() {
        match target.report(cleanup_ctx, &policy).await {
            Ok(report) => {
                if report.items > 0 || report.bytes > 0 {
                    tracing::info!(
                        category = report.category.as_str(),
                        target = report.name.as_str(),
                        items = report.items,
                        bytes = report.bytes,
                        age_days = age_seconds / 86_400,
                        "pipeline.cleanup.eligible"
                    );
                }
                total_items += report.items;
                total_bytes += report.bytes;
            }
            Err(e) => {
                tracing::warn!(
                    category = target.category().as_str(),
                    target = target.name(),
                    error = %e,
                    "pipeline.cleanup.report_failed"
                );
            }
        }
    }
    if total_items > 0 || total_bytes > 0 {
        tracing::info!(
            total_items,
            total_bytes,
            age_days = age_seconds / 86_400,
            "pipeline.cleanup.tick"
        );
    }
}

/// Run every target in a category, return their reports.
///
/// Used by the `aborg clean disk|db|queue` HTTP handlers
/// (`apply` controlled by the policy passed in). Failures
/// land as zero-item reports with an error logged.
///
/// # Errors
///
/// Never; per-target failures are absorbed into log lines.
/// Result-type kept for forward-compat (future versions
/// might surface auth/quota errors).
pub async fn run_category(
    cleanup_ctx: &CleanupCtx,
    registry: &CleanupRegistry,
    category: Category,
    policy: Policy,
) -> Result<Vec<CleanupReport>> {
    let targets = registry.for_category(category);
    let mut out = Vec::with_capacity(targets.len());
    for target in targets {
        let result = if policy.apply {
            target.apply(cleanup_ctx, &policy).await
        } else {
            target.report(cleanup_ctx, &policy).await
        };
        match result {
            Ok(r) => out.push(r),
            Err(e) => {
                tracing::warn!(
                    category = category.as_str(),
                    target = target.name(),
                    error = %e,
                    apply = policy.apply,
                    "pipeline.cleanup.target_failed"
                );
                // Surface an empty report so the caller can
                // still see this target was attempted.
                out.push(CleanupReport {
                    category,
                    name: target.name().to_owned(),
                    items: 0,
                    bytes: 0,
                });
                // Keep `Error` reachable so future
                // policy choices (fail-fast, per-target
                // abort) have an escape hatch.
                let _: Error = e;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use ab_core::tunables::DbTunables;

    /// Minimal test target that returns a fixed item count
    /// and tracks how many times `apply` was called.
    struct FixedTarget {
        cat: Category,
        n: u64,
        called: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl CleanupTarget for FixedTarget {
        fn category(&self) -> Category {
            self.cat
        }
        fn name(&self) -> &'static str {
            "fixed"
        }
        async fn report(&self, _ctx: &CleanupCtx, _policy: &Policy) -> Result<CleanupReport> {
            Ok(CleanupReport {
                category: self.cat,
                name: "fixed".to_owned(),
                items: self.n,
                bytes: self.n * 1024,
            })
        }
        async fn apply(&self, _ctx: &CleanupCtx, _policy: &Policy) -> Result<CleanupReport> {
            self.called
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(CleanupReport {
                category: self.cat,
                name: "fixed".to_owned(),
                items: self.n,
                bytes: self.n * 1024,
            })
        }
    }

    async fn fresh_ctx() -> (CleanupCtx, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let lib = LibraryDb::open(&tmp.path().join("library.db"), &DbTunables::default())
            .await
            .expect("open library");
        let eph = EphemeralDb::open(&tmp.path().join("ephemeral.db"), &DbTunables::default())
            .await
            .expect("open ephemeral");
        (
            CleanupCtx {
                library: lib,
                ephemeral: eph,
            },
            tmp,
        )
    }

    #[tokio::test]
    async fn registry_for_category_filters_correctly() {
        let called = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let registry = CleanupRegistry::new(vec![
            Arc::new(FixedTarget {
                cat: Category::Disk,
                n: 1,
                called: Arc::clone(&called),
            }),
            Arc::new(FixedTarget {
                cat: Category::Db,
                n: 2,
                called: Arc::clone(&called),
            }),
            Arc::new(FixedTarget {
                cat: Category::Queue,
                n: 3,
                called,
            }),
        ]);
        assert_eq!(registry.for_category(Category::Disk).len(), 1);
        assert_eq!(registry.for_category(Category::Db).len(), 1);
        assert_eq!(registry.for_category(Category::Queue).len(), 1);
    }

    #[tokio::test]
    async fn run_category_dry_run_does_not_call_apply() {
        let called = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (cleanup_ctx, _tmp) = fresh_ctx().await;
        let registry = CleanupRegistry::new(vec![Arc::new(FixedTarget {
            cat: Category::Db,
            n: 5,
            called: called.clone(),
        })]);
        let reports = run_category(
            &cleanup_ctx,
            &registry,
            Category::Db,
            Policy::dry_run(86_400),
        )
        .await
        .expect("run_category");
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].items, 5);
        assert_eq!(
            called.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "dry-run must not call apply()"
        );
    }

    #[tokio::test]
    async fn run_category_apply_actually_calls_apply() {
        let called = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (cleanup_ctx, _tmp) = fresh_ctx().await;
        let registry = CleanupRegistry::new(vec![Arc::new(FixedTarget {
            cat: Category::Db,
            n: 5,
            called: called.clone(),
        })]);
        let policy = Policy {
            age_seconds: 86_400,
            force: false,
            apply: true,
        };
        let reports = run_category(&cleanup_ctx, &registry, Category::Db, policy)
            .await
            .expect("run_category");
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].items, 5);
        assert_eq!(
            called.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "apply path must call apply()"
        );
    }

    #[tokio::test]
    async fn run_category_filters_to_one_category() {
        let called = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (cleanup_ctx, _tmp) = fresh_ctx().await;
        let registry = CleanupRegistry::new(vec![
            Arc::new(FixedTarget {
                cat: Category::Disk,
                n: 1,
                called: Arc::clone(&called),
            }),
            Arc::new(FixedTarget {
                cat: Category::Db,
                n: 2,
                called,
            }),
        ]);
        let reports = run_category(
            &cleanup_ctx,
            &registry,
            Category::Disk,
            Policy::dry_run(86_400),
        )
        .await
        .expect("run_category");
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].category, Category::Disk);
        assert_eq!(reports[0].items, 1);
    }
}
