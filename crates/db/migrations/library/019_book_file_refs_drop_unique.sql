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

-- Recreate the partial indexes from migration 018. The
-- acquired_at index is retained for the future `aborg doctor`
-- staleness check; if a follow-up review judges it YAGNI it
-- gets its own migration.
CREATE INDEX idx_book_file_refs_live
    ON book_file_refs(file_id) WHERE released_at IS NULL;
CREATE INDEX idx_book_file_refs_acquired_at
    ON book_file_refs(acquired_at) WHERE released_at IS NULL;
