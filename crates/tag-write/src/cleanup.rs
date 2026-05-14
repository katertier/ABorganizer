//! Mass-edit history retention (`CleanupTarget`, ADR-0025).
//!
//! Prunes rows from `library.db::mass_edit_history`. The table
//! is an audit trail for every embedded-tag write (early DNA
//! pass + late AI pass + manual user edits via the web UI); it
//! grows unboundedly without retention.
//!
//! ## Retention model — two ages, per key
//!
//! "Key" = `(target_kind, target_id, field)`. Every row belongs
//! to exactly one key; rows for the same key are an edit history
//! ordered by `recorded_at`.
//!
//! - **Latest row per key** — kept for
//!   [`CleanupTunables::mass_edit_history_latest_days`] days.
//!   This is the row the operator's "undo last edit" surface
//!   targets, so it survives longer.
//! - **Intermediate rows per key** — every row that isn't the
//!   most recent for its key. Kept for
//!   [`CleanupTunables::mass_edit_history_intermediate_days`]
//!   days. Once a newer row shadows an intermediate, it's no
//!   longer reachable through any undo flow — only forensic
//!   audit — so the window can be tighter.
//!
//! Defaults: 90 / 30 days (PROJECT.md spec).
//!
//! ## Why this is `Category::Db` and not `Disk`
//!
//! It frees DB rows, not disk artifacts. The bytes-freed field
//! in the report stays `0` — measuring per-row body size in
//! SQLite is more work than the number is worth; the row count
//! is the operative signal for an operator deciding whether to
//! grow retention. (Same call `ExpiredPairingCodesTarget`
//! makes for its small text rows.)
//!
//! ## `policy.force` semantics
//!
//! `force = true` ignores both age windows and treats every row
//! as eligible — the operator's "wipe the audit trail" escape
//! hatch. `policy.age_seconds` (from the disk-pressure ratchet)
//! is **not** used: this target's retention is spec-driven, not
//! pressure-driven. The age fields come straight from
//! [`CleanupTunables`].
//!
//! [`CleanupTunables`]: ab_core::tunables::CleanupTunables

use async_trait::async_trait;

use ab_core::cleanup::{Category, CleanupReport, Policy};
use ab_core::tunables::CleanupTunables;
use ab_core::{Error, Result};
use ab_pipeline::cleanup::{CleanupCtx, CleanupTarget};

/// Stable target name. Surfaced in CLI summaries + API responses
/// + tracing fields.
pub const TARGET_NAME: &str = "mass-edit-history-retention";

/// Retention target for `mass_edit_history`. Holds the two age
/// thresholds; `report` + `apply` walk the table with a window-
/// function query that flags per-key latest vs intermediate.
#[derive(Debug, Clone)]
pub struct MassEditHistoryRetentionTarget {
    latest_days: u64,
    intermediate_days: u64,
}

impl MassEditHistoryRetentionTarget {
    /// Build from the workspace `CleanupTunables`. Reads the two
    /// retention fields; the rest of `CleanupTunables` (pressure
    /// tiers, baseline age) doesn't apply here.
    #[must_use]
    pub const fn from_tunables(tunables: &CleanupTunables) -> Self {
        Self {
            latest_days: tunables.mass_edit_history_latest_days,
            intermediate_days: tunables.mass_edit_history_intermediate_days,
        }
    }

    /// Cutoff timestamp (UNIX seconds): rows older than this
    /// among per-key-latest rows are eligible.
    fn latest_cutoff(&self, now: i64) -> i64 {
        cutoff(now, self.latest_days)
    }

    /// Cutoff timestamp (UNIX seconds): rows older than this
    /// among per-key-intermediate rows are eligible.
    fn intermediate_cutoff(&self, now: i64) -> i64 {
        cutoff(now, self.intermediate_days)
    }
}

/// Compute `now - days * 86_400` as a UNIX-seconds cutoff,
/// clamped to `i64`. Used for both retention windows.
fn cutoff(now: i64, days: u64) -> i64 {
    let secs = days.saturating_mul(86_400);
    i64::try_from(secs).map_or(0, |s| now.saturating_sub(s))
}

#[async_trait]
impl CleanupTarget for MassEditHistoryRetentionTarget {
    fn category(&self) -> Category {
        Category::Db
    }

    fn name(&self) -> &'static str {
        TARGET_NAME
    }

    async fn report(&self, ctx: &CleanupCtx, policy: &Policy) -> Result<CleanupReport> {
        let count = count_eligible(self, ctx, policy).await?;
        Ok(CleanupReport {
            category: Category::Db,
            name: TARGET_NAME.to_owned(),
            items: count,
            // DB rows; size estimate not worth the complexity.
            bytes: 0,
        })
    }

    async fn apply(&self, ctx: &CleanupCtx, policy: &Policy) -> Result<CleanupReport> {
        let affected = delete_eligible(self, ctx, policy).await?;
        tracing::info!(
            target = TARGET_NAME,
            deleted = affected,
            force = policy.force,
            latest_days = self.latest_days,
            intermediate_days = self.intermediate_days,
            "tag_write.cleanup.applied"
        );
        Ok(CleanupReport {
            category: Category::Db,
            name: TARGET_NAME.to_owned(),
            items: affected,
            bytes: 0,
        })
    }
}

/// Count rows matching the eligibility rule. Shared between
/// `report` (dry-run count) and the test surface.
async fn count_eligible(
    target: &MassEditHistoryRetentionTarget,
    ctx: &CleanupCtx,
    policy: &Policy,
) -> Result<u64> {
    if policy.force {
        let n: i64 = sqlx::query_scalar!("SELECT COUNT(*) FROM mass_edit_history")
            .fetch_one(ctx.library.pool())
            .await
            .map_err(|e| Error::Database(format!("mass_edit_history count force: {e}")))?;
        return Ok(u64::try_from(n).unwrap_or(0));
    }
    let now = unix_now_secs();
    let latest_cutoff = target.latest_cutoff(now);
    let intermediate_cutoff = target.intermediate_cutoff(now);
    // Window function flags row #1 per key as latest, rest as
    // intermediate. Eligibility splits on `rn` and the matching
    // cutoff. SQLite 3.25+ has `row_number` OVER (required min
    // version is well past that in any platform we target).
    let n: i64 = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*) AS "n!: i64"
          FROM (
            SELECT recorded_at,
                   row_number() OVER (
                     PARTITION BY target_kind, target_id, field
                     ORDER BY recorded_at DESC, edit_id DESC
                   ) AS rn
              FROM mass_edit_history
          )
         WHERE (rn = 1 AND recorded_at < ?)
            OR (rn > 1 AND recorded_at < ?)
        "#,
        latest_cutoff,
        intermediate_cutoff,
    )
    .fetch_one(ctx.library.pool())
    .await
    .map_err(|e| Error::Database(format!("mass_edit_history count: {e}")))?;
    Ok(u64::try_from(n).unwrap_or(0))
}

/// Delete eligible rows; returns affected count. Wraps the
/// delete in a transaction so a partial failure leaves the
/// table consistent (relevant on FORCE deletes of large
/// histories — SQLite can fault mid-DELETE under disk pressure).
async fn delete_eligible(
    target: &MassEditHistoryRetentionTarget,
    ctx: &CleanupCtx,
    policy: &Policy,
) -> Result<u64> {
    let mut tx = ctx
        .library
        .pool()
        .begin()
        .await
        .map_err(|e| Error::Database(format!("mass_edit_history tx begin: {e}")))?;
    let affected = if policy.force {
        sqlx::query!("DELETE FROM mass_edit_history")
            .execute(&mut *tx)
            .await
            .map_err(|e| Error::Database(format!("mass_edit_history force delete: {e}")))?
            .rows_affected()
    } else {
        let now = unix_now_secs();
        let latest_cutoff = target.latest_cutoff(now);
        let intermediate_cutoff = target.intermediate_cutoff(now);
        // The DELETE-with-window-function dance isn't portable
        // — SQLite doesn't allow window funcs in DELETE's WHERE
        // clause. We compute eligible `edit_id`s in a subquery
        // and delete by primary key.
        sqlx::query!(
            r#"
            DELETE FROM mass_edit_history
             WHERE edit_id IN (
               SELECT edit_id FROM (
                 SELECT edit_id, recorded_at,
                        row_number() OVER (
                          PARTITION BY target_kind, target_id, field
                          ORDER BY recorded_at DESC, edit_id DESC
                        ) AS rn
                   FROM mass_edit_history
               )
              WHERE (rn = 1 AND recorded_at < ?)
                 OR (rn > 1 AND recorded_at < ?)
             )
            "#,
            latest_cutoff,
            intermediate_cutoff,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| Error::Database(format!("mass_edit_history delete: {e}")))?
        .rows_affected()
    };
    tx.commit()
        .await
        .map_err(|e| Error::Database(format!("mass_edit_history tx commit: {e}")))?;
    Ok(affected)
}

/// Seconds since the Unix epoch. Saturates on clock skew —
/// matches the rest of the codebase's defensive convention.
fn unix_now_secs() -> i64 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    i64::try_from(secs).unwrap_or(i64::MAX)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use ab_core::tunables::DbTunables;
    use ab_db::{EphemeralDb, LibraryDb};

    async fn fresh_ctx() -> (CleanupCtx, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let library = LibraryDb::open(&tmp.path().join("library.db"), &DbTunables::default())
            .await
            .expect("open library");
        let ephemeral = EphemeralDb::open(&tmp.path().join("ephemeral.db"), &DbTunables::default())
            .await
            .expect("open ephemeral");
        (CleanupCtx { library, ephemeral }, tmp)
    }

    /// Seed one `mass_edit_history` row. `recorded_at` overrides
    /// the schema's `strftime('%s','now')` default so tests can
    /// place rows on either side of the retention cutoffs.
    async fn seed_edit(
        ctx: &CleanupCtx,
        target_kind: &str,
        target_id: i64,
        field: &str,
        recorded_at: i64,
    ) {
        sqlx::query(
            "INSERT INTO mass_edit_history \
             (target_kind, target_id, field, before_value, after_value, batch_id, actor, recorded_at) \
             VALUES (?, ?, ?, NULL, '\"x\"', 'b1', 'test', ?)",
        )
        .bind(target_kind)
        .bind(target_id)
        .bind(field)
        .bind(recorded_at)
        .execute(ctx.library.pool())
        .await
        .expect("seed edit");
    }

    fn target_with(latest_days: u64, intermediate_days: u64) -> MassEditHistoryRetentionTarget {
        MassEditHistoryRetentionTarget {
            latest_days,
            intermediate_days,
        }
    }

    #[test]
    fn from_tunables_picks_up_both_windows() {
        let t = CleanupTunables {
            mass_edit_history_latest_days: 120,
            mass_edit_history_intermediate_days: 14,
            ..CleanupTunables::default()
        };
        let target = MassEditHistoryRetentionTarget::from_tunables(&t);
        assert_eq!(target.latest_days, 120);
        assert_eq!(target.intermediate_days, 14);
    }

    #[test]
    fn cutoff_subtracts_days_from_now() {
        // 90 days = 7_776_000 seconds.
        let now = 100_000_000_i64;
        let c = cutoff(now, 90);
        assert_eq!(c, 100_000_000 - 90 * 86_400);
    }

    #[test]
    fn cutoff_saturates_at_zero_for_huge_days() {
        // Insanely large `days` shouldn't underflow.
        let c = cutoff(100, u64::MAX);
        assert_eq!(c, 0);
    }

    #[tokio::test]
    async fn report_zero_on_empty_table() {
        let (ctx, _tmp) = fresh_ctx().await;
        let target = target_with(90, 30);
        let report = target
            .report(&ctx, &Policy::dry_run(86_400))
            .await
            .expect("report");
        assert_eq!(report.items, 0);
        assert_eq!(report.category, Category::Db);
        assert_eq!(report.name, TARGET_NAME);
    }

    #[tokio::test]
    async fn fresh_rows_are_never_eligible() {
        let (ctx, _tmp) = fresh_ctx().await;
        let now = unix_now_secs();
        // Latest for ("book", 1, "title"); intermediate just one
        // day older. Both within their windows.
        seed_edit(&ctx, "book", 1, "title", now - 86_400).await;
        seed_edit(&ctx, "book", 1, "title", now - 2 * 86_400).await;
        let target = target_with(90, 30);
        let report = target
            .report(&ctx, &Policy::dry_run(86_400))
            .await
            .expect("report");
        assert_eq!(report.items, 0, "rows within both windows");
    }

    #[tokio::test]
    async fn old_intermediate_is_eligible_old_latest_is_not() {
        let (ctx, _tmp) = fresh_ctx().await;
        let now = unix_now_secs();
        // Latest is 45 days old → past the 30-day intermediate
        // window but inside the 90-day latest window.
        // Intermediate is 60 days old → past the 30-day window.
        seed_edit(&ctx, "book", 1, "title", now - 45 * 86_400).await;
        seed_edit(&ctx, "book", 1, "title", now - 60 * 86_400).await;
        let target = target_with(90, 30);
        let report = target
            .report(&ctx, &Policy::dry_run(86_400))
            .await
            .expect("report");
        assert_eq!(
            report.items, 1,
            "only the older (intermediate) row should be eligible"
        );
    }

    #[tokio::test]
    async fn old_latest_is_eligible_when_past_latest_window() {
        let (ctx, _tmp) = fresh_ctx().await;
        let now = unix_now_secs();
        // Singleton row 120 days old → past the 90-day latest
        // window.
        seed_edit(&ctx, "book", 1, "title", now - 120 * 86_400).await;
        let target = target_with(90, 30);
        let report = target
            .report(&ctx, &Policy::dry_run(86_400))
            .await
            .expect("report");
        assert_eq!(report.items, 1);
    }

    #[tokio::test]
    async fn per_key_partitioning_doesnt_cross_target_id() {
        let (ctx, _tmp) = fresh_ctx().await;
        let now = unix_now_secs();
        // Two different books, each with a singleton row 60 days
        // old. Neither should be classified as intermediate just
        // because the other book exists — each key is independent.
        seed_edit(&ctx, "book", 1, "title", now - 60 * 86_400).await;
        seed_edit(&ctx, "book", 2, "title", now - 60 * 86_400).await;
        let target = target_with(90, 30);
        let report = target
            .report(&ctx, &Policy::dry_run(86_400))
            .await
            .expect("report");
        assert_eq!(report.items, 0, "each key has its own latest");
    }

    #[tokio::test]
    async fn apply_deletes_only_eligible_rows() {
        let (ctx, _tmp) = fresh_ctx().await;
        let now = unix_now_secs();
        // Three rows for the same key:
        // - newest (10d) → latest, kept
        // - middle (45d) → intermediate past 30d, eligible
        // - oldest (200d) → intermediate past 30d, eligible
        seed_edit(&ctx, "book", 1, "title", now - 10 * 86_400).await;
        seed_edit(&ctx, "book", 1, "title", now - 45 * 86_400).await;
        seed_edit(&ctx, "book", 1, "title", now - 200 * 86_400).await;
        let target = target_with(90, 30);
        let policy = Policy {
            age_seconds: 86_400,
            force: false,
            apply: true,
        };
        let report = target.apply(&ctx, &policy).await.expect("apply");
        assert_eq!(report.items, 2);
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mass_edit_history")
            .fetch_one(ctx.library.pool())
            .await
            .expect("count after apply");
        assert_eq!(remaining, 1, "only the latest survives");
    }

    #[tokio::test]
    async fn force_deletes_everything() {
        let (ctx, _tmp) = fresh_ctx().await;
        let now = unix_now_secs();
        seed_edit(&ctx, "book", 1, "title", now).await;
        seed_edit(&ctx, "book", 2, "author", now - 86_400).await;
        let target = target_with(90, 30);
        let policy = Policy {
            age_seconds: 86_400,
            force: true,
            apply: true,
        };
        let report = target.apply(&ctx, &policy).await.expect("apply force");
        assert_eq!(report.items, 2);
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mass_edit_history")
            .fetch_one(ctx.library.pool())
            .await
            .expect("count after force");
        assert_eq!(remaining, 0);
    }

    #[tokio::test]
    async fn apply_is_idempotent() {
        let (ctx, _tmp) = fresh_ctx().await;
        let now = unix_now_secs();
        seed_edit(&ctx, "book", 1, "title", now - 200 * 86_400).await;
        seed_edit(&ctx, "book", 1, "title", now - 300 * 86_400).await;
        let target = target_with(90, 30);
        let policy = Policy {
            age_seconds: 86_400,
            force: false,
            apply: true,
        };
        let first = target.apply(&ctx, &policy).await.expect("first apply");
        // 300-day-old intermediate eligible; 200-day-old latest
        // also eligible. Both go.
        assert_eq!(first.items, 2);
        let second = target.apply(&ctx, &policy).await.expect("second apply");
        assert_eq!(second.items, 0, "second pass is a no-op");
    }
}
