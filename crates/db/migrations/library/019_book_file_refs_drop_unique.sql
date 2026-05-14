-- Drop the redundant UNIQUE on book_file_refs.
--
-- Migration 018 originally declared:
--   UNIQUE (file_id, holder_stage, holder_book_id, acquired_at)
--
-- Two problems with this clause:
--
--  1. `ref_id INTEGER PRIMARY KEY AUTOINCREMENT` already
--     guarantees row uniqueness. The UNIQUE adds no semantic
--     value over the PK.
--
--  2. `acquired_at` defaults to `strftime('%s','now')` — 1-second
--     resolution. Two `acquire()` calls from the same stage on
--     the same file for the same book within a single second
--     trigger SQLITE_CONSTRAINT_UNIQUE. Not common in practice
--     (one stage runs once per book), but it's a hidden failure
--     mode for zero benefit.
--
-- Surfaced by the cross-model code review (MYREVIEW.md § 4.1 +
-- REVIEW.md § 2.5). Rebuild the table without the clause; keep
-- both partial indexes from migration 018.

CREATE TABLE book_file_refs_new (
    ref_id          INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id         INTEGER NOT NULL REFERENCES book_files(file_id)
                        ON DELETE CASCADE,
    holder_stage    TEXT NOT NULL,
    -- Add FK so deleting a book reaps its refs (MYREVIEW.md § 4.2).
    holder_book_id  INTEGER NOT NULL REFERENCES books(book_id)
                        ON DELETE CASCADE,
    acquired_at     INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    released_at     INTEGER
) STRICT;

INSERT INTO book_file_refs_new (
    ref_id, file_id, holder_stage, holder_book_id, acquired_at, released_at
)
SELECT
    ref_id, file_id, holder_stage, holder_book_id, acquired_at, released_at
FROM book_file_refs;

DROP TABLE book_file_refs;
ALTER TABLE book_file_refs_new RENAME TO book_file_refs;

-- Recreate the live-ref partial index from migration 018. The
-- acquired_at index was YAGNI — no production query reads by
-- acquired_at; both live_ref_count() and post-transcode-sources
-- filter by file_id (covered by idx_book_file_refs_live). If
-- `aborg doctor` later adds a staleness check by acquired_at,
-- it can add the index in its own migration.
CREATE INDEX idx_book_file_refs_live
    ON book_file_refs(file_id) WHERE released_at IS NULL;
