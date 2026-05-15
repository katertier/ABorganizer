-- Ephemeral schema — single consolidated migration.
--
-- Squashed from 7 incremental migrations on 2026-05-15 per the
-- schema-as-if-planned-from-day-one retrospective (item #1). Pre-1.0
-- migrations are explicitly NOT a one-way ratchet (.claude/CLAUDE.md §
-- "Migrations during development"), so the chain was consolidated
-- ahead of first tagged release.
--
-- The historical chain is preserved in git history; the section
-- dividers below mark the original boundaries for readers tracing a
-- specific column / constraint back to its motivating decision.
--
-- After first tagged release, this file is append-only; new schema
-- changes land as their own numbered migration.

-- ── original migration: 001_initial.sql ─────────────────────────────────────────────

-- ABorganizer ephemeral schema — v0.1
--
-- Holds restartable state: job queue, pipeline progress per book,
-- rate-limit state, metrics. Wiping this DB never loses user data;
-- the daemon recovers by re-scanning + re-running pending jobs.

-- ── Jobs ──────────────────────────────────────────────────────────
CREATE TABLE jobs (
    job_id          INTEGER PRIMARY KEY AUTOINCREMENT,
    -- Job kind = pipeline stage name (scan, fingerprint, enrich, ...)
    -- or named operation (rescan-book, regenerate-cover, ...).
    kind            TEXT NOT NULL,
    -- JSON params keyed off the kind's typed struct.
    params          TEXT NOT NULL DEFAULT '{}',
    priority        TEXT NOT NULL DEFAULT 'background'
                    CHECK (priority IN ('interactive', 'background')),
    status          TEXT NOT NULL DEFAULT 'pending'
                    CHECK (status IN ('pending', 'running', 'succeeded', 'failed', 'cancelled')),
    attempts        INTEGER NOT NULL DEFAULT 0,
    max_attempts    INTEGER NOT NULL DEFAULT 3,
    last_error      TEXT,
    book_id         INTEGER,            -- optional FK-shaped pointer to library.books
    enqueued_at     INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    started_at      INTEGER,
    finished_at     INTEGER,
    next_run_at     INTEGER,             -- for retries with backoff
    actor           TEXT                 -- "user", "daemon", "scheduler"
) STRICT;
CREATE INDEX idx_jobs_status_priority ON jobs(status, priority, enqueued_at);
CREATE INDEX idx_jobs_book ON jobs(book_id) WHERE book_id IS NOT NULL;

-- ── Pipeline progress per book ────────────────────────────────────
-- One row per book per stage; NULL completion_at means "pending".
-- Used to drive the "finalize when all required stages done" event.
CREATE TABLE pipeline_progress (
    book_id         INTEGER NOT NULL,
    stage           TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'pending'
                    CHECK (status IN ('pending','running','succeeded','failed','skipped')),
    last_chunk_idx  INTEGER,             -- for resumable stages (transcribe)
    started_at      INTEGER,
    completed_at    INTEGER,
    failure_reason  TEXT,
    PRIMARY KEY (book_id, stage)
) STRICT;
CREATE INDEX idx_pipeline_status ON pipeline_progress(status);

-- ── Rate-limit state (per-host, per-endpoint) ─────────────────────
CREATE TABLE rate_limits (
    host           TEXT NOT NULL,
    endpoint       TEXT NOT NULL,
    window_started INTEGER NOT NULL,
    count          INTEGER NOT NULL,
    PRIMARY KEY (host, endpoint, window_started)
) STRICT;

-- ── Pairing codes ─────────────────────────────────────────────────
CREATE TABLE pairing_codes (
    code           TEXT PRIMARY KEY,            -- "WDJB-MJHT"
    device_label   TEXT NOT NULL,
    scopes_json    TEXT NOT NULL,
    issued_at      INTEGER NOT NULL,
    expires_at     INTEGER NOT NULL,
    consumed_token_id INTEGER                   -- once paired
) STRICT;

-- ── Metrics ───────────────────────────────────────────────────────
-- Roll-up counters; reset on daemon restart unless aggregated to disk.
CREATE TABLE metrics (
    metric         TEXT NOT NULL,
    label          TEXT NOT NULL DEFAULT '',
    bucket         INTEGER NOT NULL,            -- minute-bucket unix time
    count          INTEGER NOT NULL,
    sum            REAL NOT NULL DEFAULT 0,
    PRIMARY KEY (metric, label, bucket)
) STRICT;

-- ── Meta ─────────────────────────────────────────────────────────
CREATE TABLE meta (
    key TEXT PRIMARY KEY,
    value TEXT
) STRICT;
INSERT INTO meta (key, value) VALUES ('schema_version', '1');
INSERT INTO meta (key, value) VALUES ('created_at', strftime('%s','now'));

-- ── original migration: 002_speech_installs.sql ─────────────────────────────────────

-- 3A.4.1: idle-priority Speech model installer state.
--
-- Two tables that together drive the daemon's idle install loop:
--
--   1. `pending_speech_installs` — one row per locale that some
--      book wanted but couldn't get because the on-device model
--      isn't installed (`BridgeError::ModelNotInstalled`). The
--      daemon's idle task wakes periodically, picks the oldest
--      pending row, calls `install_speech_model`, and either
--      marks it `installed` (then re-queues blocked books) or
--      bumps `last_attempted_at` and leaves it pending for the
--      next wake.
--
--   2. `book_locale_blocks` — (book_id, locale) pairs marking
--      which books are waiting on which locale. When the install
--      succeeds, the idle task reads matching rows and resubmits
--      each book's transcribe stage at Background priority, then
--      deletes the unblock rows.
--
-- Both tables are ephemeral: wiping the DB just means books with
-- `ModelNotInstalled` outcomes re-block on next scan, which is
-- the correct behaviour for "lost state."

CREATE TABLE pending_speech_installs (
    -- BCP-47 / NLLanguage raw value. Locale is the install
    -- granularity; one row covers every book that needs it.
    locale            TEXT NOT NULL PRIMARY KEY,
    -- 'pending' = waiting for the next idle wake.
    -- 'installing' = task is running install_speech_model right
    --   now (de-dup guard, in case the task wake overlaps).
    -- 'installed' = installed; this row is kept for a short
    --   window so the unblock pass can find blocked books, then
    --   garbage-collected.
    -- 'failed' = terminal error from Apple Intelligence
    --   (FrameworkUnavailable, LocaleUnsupported). Don't retry;
    --   the books in book_locale_blocks for this locale stay
    --   blocked until manual intervention.
    status            TEXT NOT NULL DEFAULT 'pending'
                      CHECK (status IN ('pending', 'installing', 'installed', 'failed')),
    queued_at         INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    last_attempted_at INTEGER,
    last_error        TEXT
) STRICT;
CREATE INDEX idx_pending_speech_installs_status
    ON pending_speech_installs(status, queued_at);

CREATE TABLE book_locale_blocks (
    -- book_id is shaped like a foreign key but the ephemeral DB
    -- has no FK to the library DB (different file). The library-
    -- side cascade is the source of truth; orphan rows here just
    -- get harmlessly skipped on the next unblock pass.
    book_id      INTEGER NOT NULL,
    locale       TEXT NOT NULL,
    queued_at    INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    PRIMARY KEY (book_id, locale)
) STRICT;
CREATE INDEX idx_book_locale_blocks_locale ON book_locale_blocks(locale);

-- ── original migration: 003_jobs_priority_idle.sql ──────────────────────────────────

-- Add 'idle' to the jobs.priority CHECK set.
--
-- Slice 3A.1 added `Priority::Idle` to the scheduler
-- (crates/pipeline/src/scheduler.rs), and follow-up slices
-- (transcribe-full, audiologo-detect) actually submit jobs at that
-- priority via the API surface. But this table's CHECK only ever
-- listed ('interactive', 'background') — so writes from the
-- scheduler at Priority::Idle were one PR away from a hard
-- SQLITE_CONSTRAINT_CHECK failure. Discovered during the cross-
-- model code review (REVIEW.md § 4.1 #1).
--
-- SQLite (3.35+) supports `ALTER TABLE ... RENAME COLUMN` /
-- `DROP COLUMN`, but NOT modifying a CHECK constraint in place.
-- The canonical fix is the rebuild dance: new table → INSERT
-- SELECT → drop old → rename → recreate indexes.
--
-- The 'cancelled' status column CHECK is preserved (no widening
-- needed there; the bug is only on `priority`).

CREATE TABLE jobs_new (
    job_id          INTEGER PRIMARY KEY AUTOINCREMENT,
    kind            TEXT NOT NULL,
    params          TEXT NOT NULL DEFAULT '{}',
    priority        TEXT NOT NULL DEFAULT 'background'
                    CHECK (priority IN ('interactive', 'background', 'idle')),
    status          TEXT NOT NULL DEFAULT 'pending'
                    CHECK (status IN ('pending', 'running', 'succeeded', 'failed', 'cancelled')),
    attempts        INTEGER NOT NULL DEFAULT 0,
    max_attempts    INTEGER NOT NULL DEFAULT 3,
    last_error      TEXT,
    book_id         INTEGER,
    enqueued_at     INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    started_at      INTEGER,
    finished_at     INTEGER,
    next_run_at     INTEGER,
    actor           TEXT
) STRICT;

INSERT INTO jobs_new (
    job_id, kind, params, priority, status, attempts, max_attempts,
    last_error, book_id, enqueued_at, started_at, finished_at,
    next_run_at, actor
)
SELECT
    job_id, kind, params, priority, status, attempts, max_attempts,
    last_error, book_id, enqueued_at, started_at, finished_at,
    next_run_at, actor
FROM jobs;

DROP TABLE jobs;
ALTER TABLE jobs_new RENAME TO jobs;

-- Recreate the two indexes from migration 001.
CREATE INDEX idx_jobs_status_priority ON jobs(status, priority, enqueued_at);
CREATE INDEX idx_jobs_book ON jobs(book_id) WHERE book_id IS NOT NULL;

-- ── original migration: 004_pairing_codes_argon2.sql ────────────────────────────────

-- Migration 004 — pairing_codes: argon2id-hashed code storage.
--
-- Backlog item 4b: pairing-code flow. The original schema (001)
-- stored `code TEXT PRIMARY KEY` plaintext, which is fine for
-- the dev-only scaffold but unfit for the real flow — a leaked
-- ephemeral.db would directly leak every pending pairing code.
--
-- Pairing codes are LOW ENTROPY (8 ASCII chars ≈ 40 bits; the
-- format we issue is `XXXX-XXXX` from a 26-letter alphabet). At
-- that entropy level, plain blake3 would be brute-forceable
-- offline. argon2id with the workspace defaults (m=19456,
-- t=2, p=1) takes ~50ms per verify on Apple Silicon, putting
-- ten million tries of an 8-char code at ~15 GPU-years — well
-- past the operational lifetime of a code (default 10 min).
--
-- The verify-against-every-row cost is acceptable here because
-- the table is tiny (operators rarely have >5 codes pending at
-- once) and consume is a one-shot human flow, not a hot path.
-- 5 verifies × 50ms = ~250ms per attempted consume.
--
-- Schema change:
--   - `code TEXT PRIMARY KEY` → `code_id INTEGER PRIMARY KEY
--     AUTOINCREMENT, code_hash TEXT NOT NULL` — the hash is the
--     verify target, code_id is the surrogate for revoke/list.
--
-- No data preserved: ABorganizer is pre-alpha, the pairing flow
-- never shipped, the only rows in this table would be from
-- tests. The cleanup target's queries don't reference the
-- `code` column at all — they only filter on `consumed_token_id`
-- and `expires_at` — so existing logic keeps working.

DROP TABLE IF EXISTS pairing_codes;

CREATE TABLE pairing_codes (
    code_id           INTEGER PRIMARY KEY AUTOINCREMENT,
    -- argon2id PHC-format string. Verify via
    -- `ab_core::auth::verify_password(presented_code, &row.code_hash)`.
    code_hash         TEXT NOT NULL,
    -- Operator-friendly label captured at issue time. Stays
    -- attached on the issued token's `nickname` column post-
    -- consume so device listings stay consistent across the
    -- two tables.
    device_label      TEXT NOT NULL,
    -- JSON-encoded array of scope strings (free-form today,
    -- typed in a future slice).
    scopes_json       TEXT NOT NULL,
    issued_at         INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    expires_at        INTEGER NOT NULL,
    -- NULL until consumed. On a successful `POST /pairing/consume`
    -- this FK is set to the freshly-issued `tokens.token_id`.
    -- The FK is across two databases (library.db.tokens vs.
    -- ephemeral.db.pairing_codes), which SQLite can't enforce —
    -- so it's an unenforced `INTEGER` reference, semantically
    -- documented here.
    consumed_token_id INTEGER
) STRICT;

-- Cleanup target filters by `consumed_token_id IS NULL` on every
-- pass; the partial index makes that filter ~O(eligible rows)
-- instead of O(all rows). Costs nothing at the deployment scale
-- we target.
CREATE INDEX idx_pairing_codes_pending
    ON pairing_codes(expires_at)
    WHERE consumed_token_id IS NULL;

-- ── original migration: 005_sleep_timer_state.sql ───────────────────────────────────

-- ADR-0046: sleep_timer_state — session-local countdown for player.
--
-- Lives in ephemeral.db: a sleep timer is a session-local concept and
-- resets on daemon restart. Persisting it across restarts would mean
-- the operator's "30 minutes from now" timer fires hours later
-- because the daemon was offline — surprising UX. Restart-reset is
-- intentional; the operator re-sets the timer if they want it back.
--
-- One active timer per session (pairing token). `mode` distinguishes
-- a fixed wall-clock target from "pause at the next chapter
-- boundary". `paused_at_ms` is non-NULL when playback is paused so
-- the remaining time can be preserved.

CREATE TABLE sleep_timer_state (
    session_token    TEXT PRIMARY KEY,
    book_id          INTEGER,                   -- book currently playing
    target_unix_ms   INTEGER NOT NULL,          -- when to pause
    mode             TEXT NOT NULL CHECK (mode IN (
                        'fixed',                 -- N ms from start
                        'end_of_chapter'         -- pause at chapter_end_ms
                    )),
    started_at_ms    INTEGER NOT NULL,
    paused_at_ms     INTEGER                    -- NULL while running
) STRICT;

-- ── original migration: 006_background_task_state.sql ───────────────────────────────

-- ADR-0035: background-task registry state.
--
-- One row per registered task, keyed by the task's `NAME` constant.
-- Lives in ephemeral.db because nothing here needs backup — a
-- daemon restart simply forfeits the in-flight cadence and lets
-- the next scheduled run land naturally. `last_run_at` survives
-- the restart so we don't immediately re-fire every task on every
-- boot.

CREATE TABLE background_task_state (
    task_name            TEXT PRIMARY KEY,
    last_run_at          INTEGER,             -- unix seconds
    last_status          TEXT,                -- 'ok' | 'error'
    last_summary         TEXT,                -- human-readable line
    consecutive_failures INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE INDEX idx_background_task_state_last_run
    ON background_task_state(last_run_at);

-- ── original migration: 007_rename_stages.sql ───────────────────────────────────────

-- Stage naming harmonisation (2026-05-15 retrospective, item #8).
--
-- Two bare-noun stage names that read as state, not action, get the
-- verb-noun shape used by every other production stage:
--
--   * `consensus`   → `promote-consensus`
--   * `fingerprint` → `fingerprint-book`
--
-- The new names disambiguate from neighbouring concepts:
--
--   * "consensus" is a noun (the result) but also reads as a state.
--     "promote-consensus" makes it clear the stage's job is to
--     promote per-source candidate rows into a single winner.
--
--   * "fingerprint" collides with the audiologo stage's per-window
--     fingerprint matching. "fingerprint-book" clarifies this is the
--     whole-book identity fingerprint stage (chromaprint windows at
--     0/25/50/75% used by `aborg library duplicates`), distinct from
--     `detect-audiologo`'s per-jingle fingerprint slide.
--
-- pipeline_progress rows referencing the old names get rewritten in
-- place so the dispatcher's stage-name lookup keeps finding them on
-- the first scheduler sweep after the rename lands.

UPDATE pipeline_progress
   SET stage = 'promote-consensus'
 WHERE stage = 'consensus';

UPDATE pipeline_progress
   SET stage = 'fingerprint-book'
 WHERE stage = 'fingerprint';

