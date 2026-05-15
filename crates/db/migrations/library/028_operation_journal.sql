-- ADR-0039: operation journal — undo + crash recovery + diff.
--
-- Every mutating operation writes its journal row BEFORE the
-- file-system / DB mutation:
--
--   1. Build `pre_state_json` (current state of the target).
--   2. INSERT into `operation_journal` with `progress='pending'`.
--   3. Perform the mutation.
--   4. UPDATE to `progress='done'` + write `post_state_json`.
--
-- Mid-batch crash: pending rows survive. On daemon restart,
-- `ab_journal::recover_pending_batches()` re-attempts or marks
-- the row failed if the target has drifted since.
--
-- 90-day retention via `StaleOperationJournalCleanupTarget`
-- (ADR-0025). The audit-trail `mass_edit_history` stays forever;
-- this table is the reversible journal.

CREATE TABLE operation_journal (
    op_id           INTEGER PRIMARY KEY AUTOINCREMENT,
    op_kind         TEXT NOT NULL,             -- 'tag-write-final', 'batch-edit', ...
    target_kind     TEXT NOT NULL,             -- 'book', 'file', 'companion', ...
    target_id       INTEGER NOT NULL,
    pre_state_json  TEXT NOT NULL,
    post_state_json TEXT,                      -- NULL while pending or for dry-run
    created_at      INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    reversible      INTEGER NOT NULL DEFAULT 1,
    batch_id        TEXT,                      -- ULID or similar; NULL for single ops
    progress        TEXT NOT NULL DEFAULT 'pending'
                        CHECK (progress IN ('pending', 'done', 'failed', 'reversed')),
    failed_reason   TEXT
) STRICT;

CREATE INDEX idx_op_journal_batch ON operation_journal(batch_id);
CREATE INDEX idx_op_journal_target ON operation_journal(target_kind, target_id);
CREATE INDEX idx_op_journal_created ON operation_journal(created_at);
CREATE INDEX idx_op_journal_pending ON operation_journal(progress)
    WHERE progress = 'pending';
