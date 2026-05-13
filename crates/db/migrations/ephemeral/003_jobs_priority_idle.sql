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
