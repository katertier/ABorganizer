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
