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
