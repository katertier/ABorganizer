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

/// Prunes `companion_nearby_books` junction rows older than 90
/// days (ADR-0043 § "CASCADE + retention", tracker #124).
///
/// Why 90 days: the UI dims the `❓` indicator after 7 days
/// (display backstop) but keeps the data so an operator-curated
/// pair-up still works. After 90 days the data is almost
/// certainly stale — either the operator decided the companion
/// was a true orphan or moved the audiobook elsewhere. The DB
/// cleanup avoids accumulating multi-million junction rows on a
/// 100k-book library with a long history of moves.
///
/// `force = true` ignores the 90-day gate. `age_seconds` from
/// the standard `Policy` is also ignored here — the threshold
/// is feature-specific, not the global disk-pressure ratchet.
/// `companion_nearby_books.discovered_at` is unix seconds.
pub struct StaleCompanionHintsTarget;

/// Junction-hint retention threshold. Pinned at 90 days per
/// ADR-0043; configurable via tunables in a follow-up slice when
/// operator demand surfaces.
const STALE_COMPANION_HINTS_AGE_SECS: i64 = 90 * 86_400;

#[async_trait]
impl CleanupTarget for StaleCompanionHintsTarget {
    fn category(&self) -> Category {
        Category::Db
    }

    fn name(&self) -> &'static str {
        "stale-companion-hints"
    }

    async fn report(&self, ctx: &CleanupCtx, policy: &Policy) -> Result<CleanupReport> {
        let cutoff = stale_hints_cutoff(policy);
        let count = if policy.force {
            sqlx::query_scalar!("SELECT COUNT(*) FROM companion_nearby_books")
                .fetch_one(ctx.library.pool())
                .await
                .map_err(|e| Error::Database(format!("count companion_nearby_books: {e}")))?
        } else {
            sqlx::query_scalar!(
                "SELECT COUNT(*) FROM companion_nearby_books WHERE discovered_at < ?",
                cutoff,
            )
            .fetch_one(ctx.library.pool())
            .await
            .map_err(|e| Error::Database(format!("count companion_nearby_books: {e}")))?
        };
        Ok(CleanupReport {
            category: Category::Db,
            name: self.name().to_owned(),
            items: u64::try_from(count).unwrap_or(0),
            bytes: 0,
        })
    }

    async fn apply(&self, ctx: &CleanupCtx, policy: &Policy) -> Result<CleanupReport> {
        let cutoff = stale_hints_cutoff(policy);
        let affected = if policy.force {
            sqlx::query!("DELETE FROM companion_nearby_books")
                .execute(ctx.library.pool())
                .await
                .map_err(|e| Error::Database(format!("delete companion_nearby_books: {e}")))?
                .rows_affected()
        } else {
            sqlx::query!(
                "DELETE FROM companion_nearby_books WHERE discovered_at < ?",
                cutoff,
            )
            .execute(ctx.library.pool())
            .await
            .map_err(|e| Error::Database(format!("delete companion_nearby_books: {e}")))?
            .rows_affected()
        };
        tracing::info!(
            target = self.name(),
            deleted = affected,
            force = policy.force,
            "api.cleanup.applied"
        );
        Ok(CleanupReport {
            category: Category::Db,
            name: self.name().to_owned(),
            items: affected,
            bytes: 0,
        })
    }
}

/// Cut-off timestamp for "is this junction-hint stale?"
fn stale_hints_cutoff(_policy: &Policy) -> i64 {
    unix_now_secs().saturating_sub(STALE_COMPANION_HINTS_AGE_SECS)
}

// ─── Stale operation-journal rows ─────────────────────────────

/// Prunes `operation_journal` rows that have reached a terminal
/// state (`done` / `failed` / `reversed`) and are older than the
/// retention window.
///
/// The journal serves two purposes: crash-recovery (PR #170 reads
/// `pending` rows at startup and flips them to `failed`) and
/// reversible-operation history (the operator can undo recent
/// mutations). Once a row is past the undo window, it's noise —
/// the audit trail proper lives in `mass_edit_history`.
///
/// Retention: 90 days (ADR-0039 schema header). `force = true`
/// drops every terminal-state row regardless of age. `pending`
/// rows are never touched — they're the crash-recovery surface
/// and only the `recover_pending` startup pass should flip them.
pub struct StaleOperationJournalTarget;

/// Retention threshold from the migration-028 schema comment.
const STALE_OPERATION_JOURNAL_AGE_SECS: i64 = 90 * 86_400;

#[async_trait]
impl CleanupTarget for StaleOperationJournalTarget {
    fn category(&self) -> Category {
        Category::Db
    }

    fn name(&self) -> &'static str {
        "stale-operation-journal"
    }

    async fn report(&self, ctx: &CleanupCtx, policy: &Policy) -> Result<CleanupReport> {
        let cutoff = stale_journal_cutoff();
        let count = if policy.force {
            sqlx::query_scalar!(
                "SELECT COUNT(*) FROM operation_journal \
                 WHERE progress IN ('done', 'failed', 'reversed')"
            )
            .fetch_one(ctx.library.pool())
            .await
            .map_err(|e| Error::Database(format!("count operation_journal: {e}")))?
        } else {
            sqlx::query_scalar!(
                "SELECT COUNT(*) FROM operation_journal \
                 WHERE progress IN ('done', 'failed', 'reversed') \
                   AND created_at < ?",
                cutoff,
            )
            .fetch_one(ctx.library.pool())
            .await
            .map_err(|e| Error::Database(format!("count operation_journal: {e}")))?
        };
        Ok(CleanupReport {
            category: Category::Db,
            name: self.name().to_owned(),
            items: u64::try_from(count).unwrap_or(0),
            bytes: 0,
        })
    }

    async fn apply(&self, ctx: &CleanupCtx, policy: &Policy) -> Result<CleanupReport> {
        let cutoff = stale_journal_cutoff();
        let affected = if policy.force {
            sqlx::query!(
                "DELETE FROM operation_journal \
                 WHERE progress IN ('done', 'failed', 'reversed')"
            )
            .execute(ctx.library.pool())
            .await
            .map_err(|e| Error::Database(format!("delete operation_journal: {e}")))?
            .rows_affected()
        } else {
            sqlx::query!(
                "DELETE FROM operation_journal \
                 WHERE progress IN ('done', 'failed', 'reversed') \
                   AND created_at < ?",
                cutoff,
            )
            .execute(ctx.library.pool())
            .await
            .map_err(|e| Error::Database(format!("delete operation_journal: {e}")))?
            .rows_affected()
        };
        tracing::info!(
            target = self.name(),
            deleted = affected,
            force = policy.force,
            "api.cleanup.applied"
        );
        Ok(CleanupReport {
            category: Category::Db,
            name: self.name().to_owned(),
            items: affected,
            bytes: 0,
        })
    }
}

fn stale_journal_cutoff() -> i64 {
    unix_now_secs().saturating_sub(STALE_OPERATION_JOURNAL_AGE_SECS)
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

    async fn seed_companion(ctx: &CleanupCtx, path: &str) -> i64 {
        // Insert the book the companion will reference + the
        // companion row itself. Returns the companion_id so the
        // test can attach junction-hint rows.
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'placeholder')")
            .execute(ctx.library.pool())
            .await
            .expect("seed book");
        sqlx::query(
            "INSERT INTO book_companions \
             (path, format, parse_tier, content_hash, bytes, discovered_at) \
             VALUES (?, 'pdf', 'document', 'deadbeef', 100, 0)",
        )
        .bind(path)
        .execute(ctx.library.pool())
        .await
        .expect("seed companion");
        sqlx::query_scalar::<_, i64>("SELECT last_insert_rowid()")
            .fetch_one(ctx.library.pool())
            .await
            .expect("last rowid")
    }

    async fn seed_hint(ctx: &CleanupCtx, companion_id: i64, book_id: i64, discovered_at: i64) {
        sqlx::query(
            "INSERT INTO companion_nearby_books (companion_id, book_id, discovered_at) \
             VALUES (?, ?, ?)",
        )
        .bind(companion_id)
        .bind(book_id)
        .bind(discovered_at)
        .execute(ctx.library.pool())
        .await
        .expect("seed hint");
    }

    #[tokio::test]
    async fn stale_hints_report_counts_only_old_rows() {
        let (ctx, _tmp) = fresh_ctx().await;
        let companion_id = seed_companion(&ctx, "/lib/x.pdf").await;
        let now = unix_now_secs();
        // 100 days old → stale; 30 days old → fresh.
        sqlx::query("INSERT INTO books (book_id, title) VALUES (2, 'b2')")
            .execute(ctx.library.pool())
            .await
            .expect("seed book2");
        seed_hint(&ctx, companion_id, 1, now - (100 * 86_400)).await;
        seed_hint(&ctx, companion_id, 2, now - (30 * 86_400)).await;
        let target = StaleCompanionHintsTarget;
        let report = target
            .report(&ctx, &Policy::dry_run(86_400))
            .await
            .expect("report");
        assert_eq!(report.items, 1, "only the 100-day-old hint is stale");
        assert_eq!(report.category, Category::Db);
    }

    #[tokio::test]
    async fn stale_hints_apply_deletes_old_only() {
        let (ctx, _tmp) = fresh_ctx().await;
        let companion_id = seed_companion(&ctx, "/lib/x.pdf").await;
        sqlx::query("INSERT INTO books (book_id, title) VALUES (2, 'b2')")
            .execute(ctx.library.pool())
            .await
            .expect("seed book2");
        let now = unix_now_secs();
        seed_hint(&ctx, companion_id, 1, now - (100 * 86_400)).await;
        seed_hint(&ctx, companion_id, 2, now - (30 * 86_400)).await;
        let target = StaleCompanionHintsTarget;
        let policy = Policy {
            age_seconds: 86_400,
            force: false,
            apply: true,
        };
        let report = target.apply(&ctx, &policy).await.expect("apply");
        assert_eq!(report.items, 1);
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM companion_nearby_books")
            .fetch_one(ctx.library.pool())
            .await
            .expect("count after apply");
        assert_eq!(remaining, 1, "fresh hint survives");
    }

    #[tokio::test]
    async fn stale_hints_force_deletes_everything() {
        let (ctx, _tmp) = fresh_ctx().await;
        let companion_id = seed_companion(&ctx, "/lib/x.pdf").await;
        sqlx::query("INSERT INTO books (book_id, title) VALUES (2, 'b2')")
            .execute(ctx.library.pool())
            .await
            .expect("seed book2");
        let now = unix_now_secs();
        // both well under 90 days
        seed_hint(&ctx, companion_id, 1, now - 100).await;
        seed_hint(&ctx, companion_id, 2, now - 200).await;
        let target = StaleCompanionHintsTarget;
        let policy = Policy {
            age_seconds: 86_400,
            force: true,
            apply: true,
        };
        let report = target.apply(&ctx, &policy).await.expect("force apply");
        assert_eq!(report.items, 2, "force drops both hints");
    }

    #[tokio::test]
    async fn stale_hints_apply_is_idempotent() {
        let (ctx, _tmp) = fresh_ctx().await;
        let companion_id = seed_companion(&ctx, "/lib/x.pdf").await;
        seed_hint(&ctx, companion_id, 1, unix_now_secs() - (100 * 86_400)).await;
        let target = StaleCompanionHintsTarget;
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

    // ─── StaleOperationJournalTarget ────────────────────────────

    async fn seed_journal(ctx: &CleanupCtx, op_kind: &str, progress: &str, created_at: i64) {
        sqlx::query(
            "INSERT INTO operation_journal \
                (op_kind, target_kind, target_id, pre_state_json, created_at, progress) \
              VALUES (?, 'book', 1, '{}', ?, ?)",
        )
        .bind(op_kind)
        .bind(created_at)
        .bind(progress)
        .execute(ctx.library.pool())
        .await
        .expect("seed journal");
    }

    #[tokio::test]
    async fn journal_report_drops_old_terminal_rows_only() {
        let (ctx, _tmp) = fresh_ctx().await;
        let now = unix_now_secs();
        // Eligible: old + terminal.
        seed_journal(&ctx, "old-done", "done", now - (100 * 86_400)).await;
        seed_journal(&ctx, "old-failed", "failed", now - (100 * 86_400)).await;
        seed_journal(&ctx, "old-reversed", "reversed", now - (100 * 86_400)).await;
        // Not eligible: pending (the recovery-pass concern, never cleaned here).
        seed_journal(&ctx, "old-pending", "pending", now - (100 * 86_400)).await;
        // Not eligible: terminal but inside the retention window.
        seed_journal(&ctx, "fresh-done", "done", now - (30 * 86_400)).await;

        let target = StaleOperationJournalTarget;
        let report = target
            .report(
                &ctx,
                &Policy {
                    age_seconds: 86_400,
                    force: false,
                    apply: false,
                },
            )
            .await
            .expect("report");
        assert_eq!(report.items, 3);
    }

    #[tokio::test]
    async fn journal_apply_with_force_drops_every_terminal_row() {
        let (ctx, _tmp) = fresh_ctx().await;
        seed_journal(&ctx, "a", "done", unix_now_secs() - 60).await;
        seed_journal(&ctx, "b", "failed", unix_now_secs() - 60).await;
        seed_journal(&ctx, "c", "pending", unix_now_secs() - 60).await;
        let target = StaleOperationJournalTarget;
        let report = target
            .apply(
                &ctx,
                &Policy {
                    age_seconds: 86_400,
                    force: true,
                    apply: true,
                },
            )
            .await
            .expect("apply");
        assert_eq!(report.items, 2, "pending must survive even under force");
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM operation_journal")
            .fetch_one(ctx.library.pool())
            .await
            .expect("count");
        assert_eq!(remaining, 1);
    }

    #[tokio::test]
    async fn journal_apply_is_idempotent() {
        let (ctx, _tmp) = fresh_ctx().await;
        seed_journal(&ctx, "a", "done", unix_now_secs() - (100 * 86_400)).await;
        let target = StaleOperationJournalTarget;
        let policy = Policy {
            age_seconds: 86_400,
            force: false,
            apply: true,
        };
        let first = target.apply(&ctx, &policy).await.expect("first");
        assert_eq!(first.items, 1);
        let second = target.apply(&ctx, &policy).await.expect("second");
        assert_eq!(second.items, 0);
    }
}
