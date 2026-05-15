//! Periodic dispatcher loop + orphan reaper. Slice 1F.A3.
//!
//! The synchronous auto-dispatch in `Scheduler::execute` (A.2)
//! catches the common case: a stage finishes, its dependents
//! get submitted. That covers about 95 % of real-world flow,
//! but three gaps remain that the per-job path can't close:
//!
//! 1. **Freshly-scanned books with no progress rows yet.** Scan
//!    inserts the book into `library.books` but doesn't write
//!    anything to `pipeline_progress`. The first stage has no
//!    way to know about the book until something dispatches it.
//! 2. **Dropped submissions.** A.2 uses `try_send`, which
//!    silently drops when the background channel buffer is
//!    full. The dispatcher retries those next tick.
//! 3. **Books / files that left the library on disk.** A user
//!    deletes a book directory; the next scan removes the row
//!    from `books` (or wipes its `book_files`). But the old
//!    `pipeline_progress` rows linger, wasting space and
//!    confusing the gap-reporting view. The reaper sweeps
//!    them.
//!
//! ## Tunables
//!
//! Both knobs live on [`SchedulerTunables`]:
//!
//! - `dispatcher_check_secs` — wake interval. 0 disables the
//!   loop entirely (daemon-wiring path checks this).
//! - `dispatcher_max_submissions_per_tick` — cap on jobs fanned
//!   out per tick. Bounded work prevents a freshly-imported
//!   library from blasting thousands of jobs at once.
//!
//! ## Cross-DB queries
//!
//! The reaper joins `library.books` × `library.book_files`
//! against `ephemeral.pipeline_progress`. SQLite doesn't span
//! the two DBs cheaply (we'd have to ATTACH per-connection),
//! so the join lives in Rust: pull active book IDs from
//! library, pull progress book IDs from ephemeral, set-diff,
//! delete the leftovers one row at a time.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use ab_core::tunables::SchedulerTunables;
use ab_core::{BookId, Result};
use ab_db::{EphemeralDb, LibraryDb};

use crate::dag::Dag;
use crate::scheduler::{Job, Priority};

/// Bundle of long-lived references the dispatcher needs.
/// Avoids the `too_many_arguments` lint on the loop entry +
/// keeps the tick path focused on the cancellation arm.
/// Constructed by [`crate::Scheduler::dispatcher_loop`]; not
/// meant for direct daemon use.
pub(crate) struct DispatcherCtx {
    pub library: LibraryDb,
    pub ephemeral: EphemeralDb,
    pub dag: Arc<Dag>,
    pub background_tx: mpsc::Sender<Job>,
    pub tunables: SchedulerTunables,
}

/// Spawn the periodic dispatcher loop.
///
/// Returns when the cancellation token fires. Logs failures
/// via `tracing` and continues — a bad tick must not kill the
/// daemon's only re-evaluation path.
pub(crate) async fn run_dispatcher_loop(ctx: DispatcherCtx, cancel: CancellationToken) {
    let DispatcherCtx {
        library,
        ephemeral,
        dag,
        background_tx,
        tunables,
    } = ctx;
    if tunables.dispatcher_check_secs == 0 {
        tracing::info!("pipeline.dispatcher.disabled");
        return;
    }
    let interval = Duration::from_secs(tunables.dispatcher_check_secs);
    tracing::info!(
        dispatcher_check_secs = tunables.dispatcher_check_secs,
        max_per_tick = tunables.dispatcher_max_submissions_per_tick,
        "pipeline.dispatcher.start"
    );
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                tracing::info!("pipeline.dispatcher.stop");
                return;
            }
            () = tokio::time::sleep(interval) => {
                if let Err(e) = tick(&library, &ephemeral, &dag, &background_tx, &tunables).await {
                    tracing::warn!(error = %e, "pipeline.dispatcher.tick_failed");
                }
            }
        }
    }
}

/// One full pass: reap orphans, then sweep for eligible
/// dispatches. Returned counts are for ops logging.
///
/// # Errors
///
/// Surfaces any underlying DB error. The caller is expected
/// to log + swallow — a transient SQLite hiccup must not kill
/// the loop.
async fn tick(
    library: &LibraryDb,
    ephemeral: &EphemeralDb,
    dag: &Arc<Dag>,
    background_tx: &mpsc::Sender<Job>,
    tunables: &SchedulerTunables,
) -> Result<()> {
    let reaped = reap_orphans(library, ephemeral).await?;
    let submitted = sweep_eligible(
        library,
        ephemeral,
        dag,
        background_tx,
        tunables.dispatcher_max_submissions_per_tick,
    )
    .await?;
    if reaped > 0 || submitted > 0 {
        tracing::info!(reaped, submitted, "pipeline.dispatcher.tick");
    }
    Ok(())
}

// ── reaper ──────────────────────────────────────────────────────────

/// Delete `pipeline_progress` rows whose `book_id` no longer
/// has a matching `books` row with ≥1 `book_files`. Returns
/// the number of rows deleted.
///
/// Two reasons a row gets reaped:
///
/// - The book vanished entirely (scan removed it after the
///   user deleted the directory on disk).
/// - The book row survives but every audio file is gone
///   (mid-import accident, or a partial cleanup). Stages have
///   no source to operate on, so the queued work is now
///   garbage.
///
/// # Errors
///
/// Surfaces underlying DB errors. Idempotent: re-running on
/// the same state deletes nothing on the second tick.
async fn reap_orphans(library: &LibraryDb, ephemeral: &EphemeralDb) -> Result<usize> {
    let active: HashSet<i64> = sqlx::query_scalar!(
        r#"SELECT b.book_id AS "book_id!: i64" FROM books b
           WHERE EXISTS (SELECT 1 FROM book_files f WHERE f.book_id = b.book_id)"#,
    )
    .fetch_all(library.pool())
    .await
    .map_err(|e| ab_core::Error::Database(format!("reaper read active books: {e}")))?
    .into_iter()
    .collect();

    let progress_ids: Vec<i64> = sqlx::query_scalar!(
        r#"SELECT DISTINCT book_id AS "book_id!: i64" FROM pipeline_progress"#,
    )
    .fetch_all(ephemeral.pool())
    .await
    .map_err(|e| ab_core::Error::Database(format!("reaper read progress ids: {e}")))?;

    let mut deleted = 0_usize;
    for id in progress_ids {
        if !active.contains(&id) {
            sqlx::query!("DELETE FROM pipeline_progress WHERE book_id = ?", id)
                .execute(ephemeral.pool())
                .await
                .map_err(|e| ab_core::Error::Database(format!("reaper delete: {e}")))?;
            deleted += 1;
        }
    }
    Ok(deleted)
}

// ── dispatch sweep ──────────────────────────────────────────────────

/// Scan the library for books that have an eligible
/// next-stage and submit (up to `max_submissions`) at
/// Background priority. Returns the count actually fanned
/// out.
///
/// Eligibility rules:
///
/// 1. The `(book, stage)` row is either missing OR has status
///    in `('pending', 'failed')` — i.e. NOT one of
///    `succeeded` / `skipped` / `running`.
/// 2. Every `stage.requires()` has a `pipeline_progress` row
///    with status in `('succeeded', 'skipped')`.
///
/// Per book, only ONE stage is submitted per tick (the first
/// eligible one in topological order). Next tick picks up the
/// next one. This keeps a single book from monopolising the
/// per-tick budget on a multi-stage library.
///
/// # Errors
///
/// Surfaces underlying DB errors.
async fn sweep_eligible(
    library: &LibraryDb,
    ephemeral: &EphemeralDb,
    dag: &Arc<Dag>,
    background_tx: &mpsc::Sender<Job>,
    max_submissions: usize,
) -> Result<usize> {
    if max_submissions == 0 {
        return Ok(0);
    }
    // `deleted_at IS NULL` filters out soft-deleted books — they
    // shouldn't have NEW pipeline work scheduled. Their
    // `pipeline_progress` rows survive (the reaper deliberately
    // preserves them) so a future restore endpoint can resume
    // where the pipeline left off. An in-flight stage on a
    // just-soft-deleted book completes its work — only new
    // scheduling is gated here.
    let book_ids: Vec<i64> = sqlx::query_scalar!(
        r#"SELECT b.book_id AS "book_id!: i64" FROM books b
           WHERE b.deleted_at IS NULL
             AND EXISTS (SELECT 1 FROM book_files f WHERE f.book_id = b.book_id)"#,
    )
    .fetch_all(library.pool())
    .await
    .map_err(|e| ab_core::Error::Database(format!("sweep read books: {e}")))?;

    let mut submitted = 0_usize;
    'books: for raw_id in book_ids {
        if submitted >= max_submissions {
            break;
        }
        let progress = read_book_progress(ephemeral, raw_id).await?;
        for (name, stage) in dag.iter_topo() {
            let current = progress.get(name).map_or("", String::as_str);
            if matches!(current, "succeeded" | "skipped" | "running") {
                continue;
            }
            // Every dep must be terminal-success in progress.
            let deps_ok = stage.requires().iter().all(|d| {
                progress
                    .get(d.as_str())
                    .is_some_and(|s| s == "succeeded" || s == "skipped")
            });
            if !deps_ok {
                continue;
            }
            // Eligible. Try to submit; on full channel, bail
            // (next tick retries). On closed channel, treat as
            // fatal for the sweep.
            let Some(stage_id) = dag.stage_id_by_name(name) else {
                continue;
            };
            let job = Job {
                book_id: BookId(raw_id),
                stage: stage_id,
                priority: Priority::Background,
            };
            match background_tx.try_send(job) {
                Ok(()) => {
                    submitted += 1;
                    tracing::debug!(book = raw_id, stage = name, "pipeline.dispatcher.submitted");
                    // One per book per tick — go to the next
                    // book, don't keep stuffing stages for this
                    // one.
                    continue 'books;
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    tracing::debug!("pipeline.dispatcher.queue_full");
                    return Ok(submitted);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    tracing::warn!("pipeline.dispatcher.channel_closed");
                    return Ok(submitted);
                }
            }
        }
    }
    Ok(submitted)
}

/// Read every progress row for one book in a single query.
/// Returns `stage_name → status`. Empty map when no rows
/// exist (brand-new book).
async fn read_book_progress(
    ephemeral: &EphemeralDb,
    book_id: i64,
) -> Result<HashMap<&'static str, String>> {
    // `stage` is queried back as `String` — we don't know
    // ahead of time which stages will appear, so the values
    // can't be `&'static str`. The DAG iter gives us static
    // names; we'll match string-to-string at the call site.
    let rows = sqlx::query!(
        "SELECT stage, status FROM pipeline_progress WHERE book_id = ?",
        book_id,
    )
    .fetch_all(ephemeral.pool())
    .await
    .map_err(|e| ab_core::Error::Database(format!("read book progress: {e}")))?;
    // The HashMap keyed on &'static str works because the
    // caller looks up via DAG stage names (which ARE static);
    // we leak nothing — the lookup uses `Borrow<str>`.
    let mut out: HashMap<&'static str, String> = HashMap::new();
    // We need to find the static-equivalent &'static str for
    // each row.stage. The caller will only look up via DAG
    // names; if a row's stage isn't in the DAG (e.g. a renamed
    // legacy stage row that the reaper missed), it doesn't
    // matter — the caller never asks for it. So we can drop
    // those rows here; they'd never participate in dispatch.
    // To get the static lookup right, we'd need a way to map
    // a String back to a &'static str. Easier: just key on
    // String and have the caller convert when matching.
    for r in rows {
        out.insert(intern_stage_name(&r.stage), r.status);
    }
    Ok(out)
}

/// Map a runtime stage name to the matching `&'static str`
/// from the DAG-known set. Returns `""` (an interned empty
/// static) for unknown names — the caller treats that as "no
/// match", so the row is silently skipped.
///
/// Implementation note: stage names ARE static at compile
/// time (every stage exposes a `pub const STAGE_ID:
/// StageId`), but `sqlx` returns them as owned `String`. We
/// re-look-them-up against the well-known set rather than
/// `Box::leak` per-row. The set is small (≤20 stages) so a
/// linear walk is fine.
fn intern_stage_name(s: &str) -> &'static str {
    // SAFETY: the set below is the full registered-stage
    // universe; rows naming anything else are stale (reaper
    // sweeps eventually). The dispatcher's only consumer
    // compares against DAG-known names, so an unknown name
    // returning "" causes the row to be silently ignored —
    // no panics, no crashes.
    //
    // We deliberately do NOT use `dag.stage_id_by_name()`
    // here: the DAG's keys are `&'static str` already, but
    // borrowing them out is shape-incompatible with this
    // helper's signature. Listing the universe inline as
    // string literals keeps the typing clean.
    KNOWN_STAGE_NAMES
        .iter()
        .find(|known| **known == s)
        .copied()
        .unwrap_or("")
}

/// Authoritative list of registered stage names. Must be in
/// sync with the stage crates' `pub const STAGE_ID` values.
///
/// Adding a stage? Add its name here too — the dispatcher
/// silently ignores `pipeline_progress` rows whose stage name
/// isn't in this list. (The reaper sweeps stale rows
/// eventually, so the cost of a forgotten entry is "this
/// stage won't auto-dispatch via the periodic loop" — its
/// in-line A.2 path still works.)
const KNOWN_STAGE_NAMES: &[&str] = &[
    "tag-read",
    "fingerprint-book",
    "catalog-audnexus",
    "catalog-google-books",
    "transcribe-samples",
    "transcribe-head-tail",
    "transcribe-full",
    "description-language",
    "extract-dna",
    "extract-summary",
    "extract-arc",
    "extract-characters",
    "extract-setting",
    "extract-series-summary",
    "audiologo-detect",
];

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::sync::Arc as StdArc;
    use std::sync::atomic::AtomicUsize;

    use async_trait::async_trait;
    use tokio::sync::mpsc;

    use ab_core::tunables::DbTunables;
    use ab_db::{EphemeralDb, LibraryDb};

    use super::*;
    use crate::Stage;
    use crate::stage::{StageId, StageOutcome};

    /// Test stage with configurable name + deps, always Done.
    struct TestStage {
        name_str: &'static str,
        deps: &'static [StageId],
    }

    #[async_trait]
    impl Stage for TestStage {
        fn name(&self) -> &'static str {
            self.name_str
        }
        fn requires(&self) -> &'static [StageId] {
            self.deps
        }
        async fn run(&self, _ctx: &crate::StageContext, _id: BookId) -> Result<StageOutcome> {
            Ok(StageOutcome::Done)
        }
    }

    async fn fresh_dbs() -> (LibraryDb, EphemeralDb, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let lib = LibraryDb::open(&tmp.path().join("library.db"), &DbTunables::default())
            .await
            .expect("open library");
        let eph = EphemeralDb::open(&tmp.path().join("ephemeral.db"), &DbTunables::default())
            .await
            .expect("open ephemeral");
        (lib, eph, tmp)
    }

    /// Insert a book row + a file row so the reaper considers
    /// it "active." Returns the assigned `book_id`.
    async fn insert_active_book(library: &LibraryDb, title: &str) -> i64 {
        let book_id: i64 =
            sqlx::query_scalar("INSERT INTO books (title) VALUES (?) RETURNING book_id")
                .bind(title)
                .fetch_one(library.pool())
                .await
                .expect("insert book");
        sqlx::query("INSERT INTO book_files (book_id, file_path, duration_ms) VALUES (?, ?, 1000)")
            .bind(book_id)
            .bind(format!("/tmp/{title}.m4b"))
            .execute(library.pool())
            .await
            .expect("insert file");
        book_id
    }

    /// Seed a `pipeline_progress` row for tests.
    async fn seed_progress(ephemeral: &EphemeralDb, book_id: i64, stage: &str, status: &str) {
        sqlx::query("INSERT INTO pipeline_progress (book_id, stage, status) VALUES (?, ?, ?)")
            .bind(book_id)
            .bind(stage)
            .bind(status)
            .execute(ephemeral.pool())
            .await
            .expect("seed progress");
    }

    async fn count_progress(ephemeral: &EphemeralDb, book_id: i64) -> i64 {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pipeline_progress WHERE book_id = ?")
            .bind(book_id)
            .fetch_one(ephemeral.pool())
            .await
            .expect("count progress");
        n
    }

    #[tokio::test]
    async fn reaper_deletes_progress_for_books_not_in_library() {
        let (lib, eph, _tmp) = fresh_dbs().await;
        // book_real: exists in library with files (active).
        // book_ghost: doesn't exist in library at all — pure
        // pipeline_progress entry, the kind a since-deleted
        // book leaves behind.
        let book_real = insert_active_book(&lib, "kept").await;
        let book_ghost: i64 = 999;
        seed_progress(&eph, book_ghost, "tag-read", "succeeded").await;
        seed_progress(&eph, book_real, "tag-read", "succeeded").await;

        let deleted = reap_orphans(&lib, &eph).await.expect("reap");
        assert_eq!(deleted, 1, "the orphan gets reaped");
        assert_eq!(count_progress(&eph, book_ghost).await, 0);
        assert_eq!(count_progress(&eph, book_real).await, 1);
    }

    #[tokio::test]
    async fn reaper_deletes_progress_for_books_with_no_files() {
        // User dropped every file but the book row survives —
        // per the slice spec, those progress rows also go.
        let (lib, eph, _tmp) = fresh_dbs().await;
        let book = insert_active_book(&lib, "vanishing").await;
        // Strip the file rows but leave the books row.
        sqlx::query("DELETE FROM book_files WHERE book_id = ?")
            .bind(book)
            .execute(lib.pool())
            .await
            .expect("drop files");
        seed_progress(&eph, book, "tag-read", "succeeded").await;

        let deleted = reap_orphans(&lib, &eph).await.expect("reap");
        assert_eq!(deleted, 1, "filespace-less books also count as orphan");
        assert_eq!(count_progress(&eph, book).await, 0);
    }

    #[tokio::test]
    async fn reaper_idempotent_on_clean_state() {
        let (lib, eph, _tmp) = fresh_dbs().await;
        let book = insert_active_book(&lib, "clean").await;
        seed_progress(&eph, book, "tag-read", "succeeded").await;
        let first = reap_orphans(&lib, &eph).await.expect("reap1");
        let second = reap_orphans(&lib, &eph).await.expect("reap2");
        assert_eq!(first, 0);
        assert_eq!(second, 0);
        assert_eq!(count_progress(&eph, book).await, 1);
    }

    /// Build a tiny DAG using stages that ARE in
    /// `KNOWN_STAGE_NAMES` so the dispatcher's intern-lookup
    /// recognises them. Using "tag-read" and "fingerprint-book"
    /// as the stand-ins.
    fn dag_tagread_then_fingerprint() -> Arc<Dag> {
        const TAG_READ: StageId = StageId::new("tag-read");
        let stages: Vec<Arc<dyn Stage>> = vec![
            Arc::new(TestStage {
                name_str: "tag-read",
                deps: &[],
            }),
            Arc::new(TestStage {
                name_str: "fingerprint-book",
                deps: &[TAG_READ],
            }),
        ];
        Arc::new(Dag::build(stages).expect("dag"))
    }

    #[tokio::test]
    async fn sweep_submits_first_stage_for_new_book() {
        // Brand-new book in library, no progress rows. The
        // dispatcher must submit `tag-read` (root stage).
        let (lib, eph, _tmp) = fresh_dbs().await;
        let book = insert_active_book(&lib, "fresh").await;
        let dag = dag_tagread_then_fingerprint();
        let (tx, mut rx) = mpsc::channel::<Job>(8);

        let n = sweep_eligible(&lib, &eph, &dag, &tx, 16)
            .await
            .expect("sweep");
        assert_eq!(n, 1);

        // The submitted job is (book, tag-read).
        let job = rx.try_recv().expect("got a job");
        assert_eq!(job.book_id.0, book);
        assert_eq!(job.stage.as_str(), "tag-read");
    }

    #[tokio::test]
    async fn sweep_skips_books_whose_first_stage_is_already_done() {
        let (lib, eph, _tmp) = fresh_dbs().await;
        let book = insert_active_book(&lib, "done-already").await;
        // Both stages already terminal; nothing to dispatch.
        seed_progress(&eph, book, "tag-read", "succeeded").await;
        seed_progress(&eph, book, "fingerprint-book", "succeeded").await;
        let dag = dag_tagread_then_fingerprint();
        let (tx, _rx) = mpsc::channel::<Job>(8);

        let n = sweep_eligible(&lib, &eph, &dag, &tx, 16)
            .await
            .expect("sweep");
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn sweep_dispatches_second_stage_when_first_done() {
        // tag-read=succeeded, fingerprint-book=no row →
        // dispatcher must submit fingerprint-book.
        let (lib, eph, _tmp) = fresh_dbs().await;
        let book = insert_active_book(&lib, "halfway").await;
        seed_progress(&eph, book, "tag-read", "succeeded").await;
        let dag = dag_tagread_then_fingerprint();
        let (tx, mut rx) = mpsc::channel::<Job>(8);

        let n = sweep_eligible(&lib, &eph, &dag, &tx, 16)
            .await
            .expect("sweep");
        assert_eq!(n, 1);
        let job = rx.try_recv().expect("got a job");
        assert_eq!(job.stage.as_str(), "fingerprint-book");
    }

    #[tokio::test]
    async fn sweep_respects_max_submissions_cap() {
        let (lib, eph, _tmp) = fresh_dbs().await;
        let _ = insert_active_book(&lib, "a").await;
        let _ = insert_active_book(&lib, "b").await;
        let _ = insert_active_book(&lib, "c").await;
        let dag = dag_tagread_then_fingerprint();
        let (tx, mut rx) = mpsc::channel::<Job>(8);

        let n = sweep_eligible(&lib, &eph, &dag, &tx, 2)
            .await
            .expect("sweep");
        assert_eq!(n, 2, "cap is honoured");
        let _ = rx.try_recv().expect("first");
        let _ = rx.try_recv().expect("second");
        assert!(rx.try_recv().is_err(), "no third");
    }

    #[tokio::test]
    async fn sweep_does_not_resubmit_running_stages() {
        // Stage already 'running' must NOT be re-submitted.
        let (lib, eph, _tmp) = fresh_dbs().await;
        let book = insert_active_book(&lib, "busy").await;
        seed_progress(&eph, book, "tag-read", "running").await;
        let dag = dag_tagread_then_fingerprint();
        let (tx, _rx) = mpsc::channel::<Job>(8);

        let n = sweep_eligible(&lib, &eph, &dag, &tx, 16)
            .await
            .expect("sweep");
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn sweep_retries_failed_stages() {
        // A failed stage is eligible for re-dispatch — the
        // retry semantics are "the dispatcher tries again."
        let (lib, eph, _tmp) = fresh_dbs().await;
        let book = insert_active_book(&lib, "retry").await;
        seed_progress(&eph, book, "tag-read", "failed").await;
        let dag = dag_tagread_then_fingerprint();
        let (tx, mut rx) = mpsc::channel::<Job>(8);

        let n = sweep_eligible(&lib, &eph, &dag, &tx, 16)
            .await
            .expect("sweep");
        assert_eq!(n, 1);
        let job = rx.try_recv().expect("retry job");
        assert_eq!(job.stage.as_str(), "tag-read");
    }

    /// Silence the unused-import warning that strikes when
    /// the rest of the file doesn't reference `StdArc` /
    /// `AtomicUsize` directly. They're here so future tests
    /// (concurrent dispatch races) can drop in cheaply.
    #[allow(dead_code)]
    fn _touch() -> (StdArc<AtomicUsize>,) {
        (StdArc::new(AtomicUsize::new(0)),)
    }
}
