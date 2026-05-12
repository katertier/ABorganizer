//! Idle-priority Speech model installer.
//!
//! Pairs with the `transcribe-head-tail` stage: when transcribe
//! sees `BridgeError::ModelNotInstalled` it writes a row into
//! `pending_speech_installs` + a (book, locale) row into
//! `book_locale_blocks` and returns `Skipped`. This module's
//! [`run_idle_install_loop`] wakes periodically, drains the
//! pending table by calling [`install_speech_model_typed`], and
//! re-queues the unblocked books at Background priority.
//!
//! ## Why a separate loop (not a Stage)
//!
//! Installs are system-wide: one model serves every book in that
//! locale. Stages take a `BookId` per call; modelling per-locale
//! work as a fake-BookId stage would be awkward. Easier to spawn
//! one tokio task that owns the install state machine and uses
//! the existing `Scheduler::submit` to fan back out to book-level
//! work.
//!
//! ## Failure semantics
//!
//! Two error classes:
//!
//! - **Terminal** — `FrameworkUnavailable`, `LocaleUnsupported`.
//!   Status flips to `'failed'`; books stay blocked until manual
//!   intervention (or a future "unblock all" UI). No retry.
//! - **Transient** — everything else (network glitch during
//!   download, etc.). Row stays `'pending'`; the next wake
//!   retries with a fresh `last_attempted_at`.

use std::sync::Arc;

use ab_core::tunables::TranscribeTunables;
use ab_core::{BookId, Result};
use ab_db::EphemeralDb;
use ab_pipeline::{Priority, Scheduler};
use tokio_util::sync::CancellationToken;

use crate::stage::STAGE_NAME;
use ab_speech::{BridgeError, install_speech_model_typed};

/// Spawnable idle install loop. Wakes every
/// `tunables.idle_install_check_secs`; on each wake:
///
/// 1. Garbage-collect rows whose status is `'installed'` — the
///    previous wake already re-queued the blocked books, so the
///    row is just clutter.
/// 2. Pick the oldest `'pending'` row, atomically flip it to
///    `'installing'`.
/// 3. Call [`install_speech_model_typed`].
/// 4. Classify the result and persist:
///    - `Ok(())`: mark `'installed'`, read matching
///      `book_locale_blocks`, submit each book to the scheduler
///      at `Priority::Background`, delete those block rows.
///    - `Err(FrameworkUnavailable | LocaleUnsupported)`: mark
///      `'failed'` with the error message; books stay blocked.
///    - `Err(_)`: leave `'pending'`, bump `last_attempted_at`,
///      next wake retries.
/// 5. Loop until cancelled.
///
/// The function never returns errors — failures are logged via
/// `tracing` and the loop keeps going. Cancellation via the
/// shared `CancellationToken` is the only exit.
pub async fn run_idle_install_loop(
    ephemeral: EphemeralDb,
    scheduler: Arc<Scheduler>,
    transcribe: TranscribeTunables,
    cancel: CancellationToken,
) {
    use tokio::time::{Duration, sleep};
    let interval = Duration::from_secs(transcribe.idle_install_check_secs);
    tracing::info!(
        idle_install_check_secs = transcribe.idle_install_check_secs,
        "transcribe.idle_install.start"
    );
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                tracing::info!("transcribe.idle_install.stop");
                return;
            }
            () = sleep(interval) => {
                if let Err(e) = process_one_tick(&ephemeral, &scheduler).await {
                    tracing::warn!(error = %e, "transcribe.idle_install.tick_failed");
                }
            }
        }
    }
}

/// One iteration of the idle loop. Drains at most one pending
/// row to keep the wake cheap (a slow install shouldn't block
/// other pending locales for the whole tick). Caller wakes
/// again on the next interval.
async fn process_one_tick(ephemeral: &EphemeralDb, scheduler: &Scheduler) -> Result<()> {
    // GC any leftover 'installed' rows from prior ticks.
    let _ = sqlx::query!("DELETE FROM pending_speech_installs WHERE status = 'installed'")
        .execute(ephemeral.pool())
        .await;

    let Some(locale) = claim_next_pending(ephemeral).await? else {
        return Ok(());
    };
    tracing::info!(locale = %locale, "transcribe.idle_install.attempting");

    match install_speech_model_typed(&locale).await {
        Ok(()) => {
            tracing::info!(locale = %locale, "transcribe.idle_install.installed");
            mark_installed(ephemeral, &locale).await?;
            let books = take_blocked_books(ephemeral, &locale).await?;
            for book_id in books {
                if let Err(e) = scheduler
                    .submit(book_id, STAGE_NAME, Priority::Background)
                    .await
                {
                    tracing::warn!(
                        book = %book_id,
                        locale = %locale,
                        error = %e,
                        "transcribe.idle_install.requeue_failed"
                    );
                }
            }
        }
        Err(e @ (BridgeError::FrameworkUnavailable | BridgeError::LocaleUnsupported)) => {
            let msg = format!("{e}");
            tracing::warn!(locale = %locale, error = %msg, "transcribe.idle_install.terminal");
            mark_failed(ephemeral, &locale, &msg).await?;
        }
        Err(e) => {
            let msg = format!("{e}");
            tracing::warn!(locale = %locale, error = %msg, "transcribe.idle_install.transient");
            mark_attempted(ephemeral, &locale, &msg).await?;
        }
    }
    Ok(())
}

/// Atomically claim the oldest `'pending'` row, flipping it to
/// `'installing'`. Returns the locale on success, `None` when
/// nothing is pending. The atomic flip prevents a second wake
/// from racing on the same row if a previous tick is somehow
/// still in flight (shouldn't happen with `biased; sleep`, but
/// belt-and-braces).
async fn claim_next_pending(ephemeral: &EphemeralDb) -> Result<Option<String>> {
    let row = sqlx::query!(
        "UPDATE pending_speech_installs \
         SET status = 'installing', last_attempted_at = strftime('%s','now') \
         WHERE locale = ( \
             SELECT locale FROM pending_speech_installs \
             WHERE status = 'pending' \
             ORDER BY queued_at LIMIT 1 \
         ) \
         RETURNING locale",
    )
    .fetch_optional(ephemeral.pool())
    .await
    .map_err(|e| ab_core::Error::Database(format!("claim pending install: {e}")))?;
    Ok(row.map(|r| r.locale))
}

async fn mark_installed(ephemeral: &EphemeralDb, locale: &str) -> Result<()> {
    sqlx::query!(
        "UPDATE pending_speech_installs SET status = 'installed', last_error = NULL WHERE locale = ?",
        locale,
    )
    .execute(ephemeral.pool())
    .await
    .map_err(|e| ab_core::Error::Database(format!("mark installed: {e}")))?;
    Ok(())
}

async fn mark_failed(ephemeral: &EphemeralDb, locale: &str, err: &str) -> Result<()> {
    sqlx::query!(
        "UPDATE pending_speech_installs SET status = 'failed', last_error = ? WHERE locale = ?",
        err,
        locale,
    )
    .execute(ephemeral.pool())
    .await
    .map_err(|e| ab_core::Error::Database(format!("mark failed: {e}")))?;
    Ok(())
}

async fn mark_attempted(ephemeral: &EphemeralDb, locale: &str, err: &str) -> Result<()> {
    sqlx::query!(
        "UPDATE pending_speech_installs \
         SET status = 'pending', last_error = ?, last_attempted_at = strftime('%s','now') \
         WHERE locale = ?",
        err,
        locale,
    )
    .execute(ephemeral.pool())
    .await
    .map_err(|e| ab_core::Error::Database(format!("mark attempted: {e}")))?;
    Ok(())
}

/// Read + delete all `book_locale_blocks` rows for the given
/// locale in a single transaction. Returns the books that were
/// blocked. After this returns, the scheduler resubmit is the
/// caller's responsibility.
async fn take_blocked_books(ephemeral: &EphemeralDb, locale: &str) -> Result<Vec<BookId>> {
    let rows = sqlx::query!(
        "SELECT book_id FROM book_locale_blocks WHERE locale = ?",
        locale,
    )
    .fetch_all(ephemeral.pool())
    .await
    .map_err(|e| ab_core::Error::Database(format!("read locale blocks: {e}")))?;
    sqlx::query!("DELETE FROM book_locale_blocks WHERE locale = ?", locale,)
        .execute(ephemeral.pool())
        .await
        .map_err(|e| ab_core::Error::Database(format!("delete locale blocks: {e}")))?;
    Ok(rows.into_iter().map(|r| BookId(r.book_id)).collect())
}
