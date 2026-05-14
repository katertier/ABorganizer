//! Concrete [`CleanupTarget`] implementations exposed by the API
//! crate (slice H.2.3, ADR-0025).
//!
//! Each target is a small `struct`-with-no-state that owns the
//! query that decides "what is eligible" for one
//! category × table combination. The daemon registers an
//! `Arc<dyn CleanupTarget>` for each one at startup; the periodic
//! loop and the `/api/v1/clean/run` HTTP handler both reach them
//! through the same trait surface.
//!
//! # Adding a new target
//!
//! 1. Write the `struct` + `impl CleanupTarget` here (or in the
//!    owning feature crate — orphan rules permit either).
//! 2. Register it in `bins/aborg-daemon/src/main.rs` via the
//!    `build_cleanup_registry` helper.
//! 3. Add a usage note to PROJECT.md / SCHEMA.md so the next
//!    operator knows what's in scope.
//!
//! Targets MUST be idempotent — `apply` followed by `report` must
//! return `items: 0` for the same `Policy`. The periodic loop
//! relies on this to be quiet on ticks where nothing is eligible.

use async_trait::async_trait;

use ab_core::cleanup::{Category, CleanupReport, Policy};
use ab_core::{Error, Result};
use ab_pipeline::cleanup::{CleanupCtx, CleanupTarget};

/// Prunes expired, unconsumed pairing codes from
/// `ephemeral.db::pairing_codes`.
///
/// "Unconsumed" = `consumed_token_id IS NULL`. Consumed rows are
/// kept as an audit trail (the pairing flow's user-facing surface
/// references them by code); only the dead-on-arrival ones go.
/// `expires_at` is a Unix timestamp in seconds — same convention as
/// every other ephemeral table.
///
/// `force = true` (via `aborg clean queue --force`) ignores the
/// `expires_at` gate entirely; useful when an operator pushes a
/// breaking change to the pairing-code scope vocabulary and wants
/// every pending code invalidated. The `age_seconds` policy field
/// is unused for this target — pairing-code expiry is set at
/// issue time, not derived from `now - age`.
pub struct ExpiredPairingCodesTarget;

#[async_trait]
impl CleanupTarget for ExpiredPairingCodesTarget {
    fn category(&self) -> Category {
        Category::Queue
    }

    fn name(&self) -> &'static str {
        "expired-pairing-codes"
    }

    async fn report(&self, ctx: &CleanupCtx, policy: &Policy) -> Result<CleanupReport> {
        let now = unix_now_secs();
        let count = if policy.force {
            sqlx::query_scalar!(
                "SELECT COUNT(*) FROM pairing_codes WHERE consumed_token_id IS NULL"
            )
            .fetch_one(ctx.ephemeral.pool())
            .await
            .map_err(|e| Error::Database(format!("count pairing_codes: {e}")))?
        } else {
            sqlx::query_scalar!(
                "SELECT COUNT(*) FROM pairing_codes \
                 WHERE consumed_token_id IS NULL AND expires_at < ?",
                now,
            )
            .fetch_one(ctx.ephemeral.pool())
            .await
            .map_err(|e| Error::Database(format!("count pairing_codes: {e}")))?
        };
        Ok(CleanupReport {
            category: Category::Queue,
            name: self.name().to_owned(),
            items: u64::try_from(count).unwrap_or(0),
            bytes: 0, // small text rows; size estimate not worth the complexity.
        })
    }

    async fn apply(&self, ctx: &CleanupCtx, policy: &Policy) -> Result<CleanupReport> {
        let now = unix_now_secs();
        let affected = if policy.force {
            sqlx::query!("DELETE FROM pairing_codes WHERE consumed_token_id IS NULL")
                .execute(ctx.ephemeral.pool())
                .await
                .map_err(|e| Error::Database(format!("delete pairing_codes: {e}")))?
                .rows_affected()
        } else {
            sqlx::query!(
                "DELETE FROM pairing_codes \
                 WHERE consumed_token_id IS NULL AND expires_at < ?",
                now,
            )
            .execute(ctx.ephemeral.pool())
            .await
            .map_err(|e| Error::Database(format!("delete pairing_codes: {e}")))?
            .rows_affected()
        };
        tracing::info!(
            target = self.name(),
            deleted = affected,
            force = policy.force,
            "api.cleanup.applied"
        );
        Ok(CleanupReport {
            category: Category::Queue,
            name: self.name().to_owned(),
            items: affected,
            bytes: 0,
        })
    }
}

/// Seconds since the Unix epoch. Saturates on the clock-skew edge
/// case (`UNIX_EPOCH` somehow in the future), matching the rest of
/// the codebase's defensive convention.
fn unix_now_secs() -> i64 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    i64::try_from(secs).unwrap_or(i64::MAX)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
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

    async fn seed_code(ctx: &CleanupCtx, code_label: &str, expires_at: i64, consumed: bool) {
        // Migration 004 reshaped this table: `code TEXT PK` →
        // `code_id INTEGER PK AUTOINCREMENT, code_hash TEXT`.
        // The `ExpiredPairingCodesTarget` cleanup target only
        // filters on `consumed_token_id` + `expires_at` (no
        // reference to the code column at all), so the test
        // seed just needs a plausible row shape — the `code_hash`
        // value here is a placeholder, not a real argon2id hash.
        let token: Option<i64> = if consumed { Some(1) } else { None };
        sqlx::query(
            "INSERT INTO pairing_codes \
             (code_hash, device_label, scopes_json, issued_at, expires_at, consumed_token_id) \
             VALUES (?, ?, '[]', 0, ?, ?)",
        )
        .bind(format!("hash-of-{code_label}"))
        .bind(code_label)
        .bind(expires_at)
        .bind(token)
        .execute(ctx.ephemeral.pool())
        .await
        .expect("seed pairing_code");
    }

    #[tokio::test]
    async fn report_counts_only_unconsumed_expired() {
        let (ctx, _tmp) = fresh_ctx().await;
        let now = unix_now_secs();
        // Three rows: expired+unconsumed (eligible), expired+consumed (not), future+unconsumed (not).
        seed_code(&ctx, "OLD-PEND", now - 3600, false).await;
        seed_code(&ctx, "OLD-USED", now - 3600, true).await;
        seed_code(&ctx, "NEW-PEND", now + 3600, false).await;
        let target = ExpiredPairingCodesTarget;
        let report = target
            .report(&ctx, &Policy::dry_run(86_400))
            .await
            .expect("report");
        assert_eq!(report.items, 1, "only OLD-PEND should match");
        assert_eq!(report.category, Category::Queue);
    }

    #[tokio::test]
    async fn apply_deletes_only_unconsumed_expired() {
        let (ctx, _tmp) = fresh_ctx().await;
        let now = unix_now_secs();
        seed_code(&ctx, "OLD-PEND", now - 3600, false).await;
        seed_code(&ctx, "OLD-USED", now - 3600, true).await;
        seed_code(&ctx, "NEW-PEND", now + 3600, false).await;
        let target = ExpiredPairingCodesTarget;
        let policy = Policy {
            age_seconds: 86_400,
            force: false,
            apply: true,
        };
        let report = target.apply(&ctx, &policy).await.expect("apply");
        assert_eq!(report.items, 1);
        // The two non-eligible rows survive.
        let surviving: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pairing_codes")
            .fetch_one(ctx.ephemeral.pool())
            .await
            .expect("count after apply");
        assert_eq!(surviving, 2);
    }

    #[tokio::test]
    async fn force_deletes_unconsumed_even_when_not_expired() {
        let (ctx, _tmp) = fresh_ctx().await;
        let now = unix_now_secs();
        seed_code(&ctx, "NEW-PEND", now + 3600, false).await;
        seed_code(&ctx, "NEW-USED", now + 3600, true).await;
        let target = ExpiredPairingCodesTarget;
        let policy = Policy {
            age_seconds: 86_400,
            force: true,
            apply: true,
        };
        let report = target.apply(&ctx, &policy).await.expect("apply force");
        assert_eq!(report.items, 1, "force still skips consumed rows");
        let surviving: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pairing_codes")
            .fetch_one(ctx.ephemeral.pool())
            .await
            .expect("count after force");
        assert_eq!(surviving, 1, "consumed pairing code preserved as audit");
    }

    #[tokio::test]
    async fn apply_is_idempotent() {
        let (ctx, _tmp) = fresh_ctx().await;
        let now = unix_now_secs();
        seed_code(&ctx, "OLD-PEND", now - 3600, false).await;
        let target = ExpiredPairingCodesTarget;
        let policy = Policy {
            age_seconds: 86_400,
            force: false,
            apply: true,
        };
        let first = target.apply(&ctx, &policy).await.expect("first apply");
        assert_eq!(first.items, 1);
        let second = target.apply(&ctx, &policy).await.expect("second apply");
        assert_eq!(second.items, 0, "second apply must be a no-op");
    }
}
