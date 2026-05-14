-- ADR-0027: source-file refcount for the transcode pipeline.
--
-- Per ADR-0027: transcode-to-m4b runs at Background priority in
-- parallel with AI jobs. Source files are kept alive by reference-
-- counting rather than by a pipeline-pause, so AI consumers reading
-- the source mid-transcode never see a half-written or missing
-- file. The post-transcode-sources cleanup target reaps a source
-- only when (a) a successful m4b transcode output exists AND (b)
-- live_ref_count(source_file_id) == 0.
--
-- Rows are acquired at stage-run start and released at stage-run
-- end (RAII handle on the Rust side). A live row has released_at
-- NULL. The partial index on (file_id) WHERE released_at IS NULL
-- gives the live_ref_count predicate O(log n) lookup without
-- scanning the historical-acquire log.
--
-- Held refs from a panicked stage leak (released_at stays NULL);
-- the cleanup target ignores those files. The future `aborg
-- doctor` (Theme 6 hardening) lists refs older than 1 hour with
-- no live stage as suspect leaks.

CREATE TABLE book_file_refs (
    ref_id          INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id         INTEGER NOT NULL REFERENCES book_files(file_id)
                        ON DELETE CASCADE,
    holder_stage    TEXT NOT NULL,        -- StageId of the holder
    holder_book_id  INTEGER NOT NULL,     -- book this holder ran for
    acquired_at     INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    released_at     INTEGER,              -- NULL while the ref is live
    UNIQUE (file_id, holder_stage, holder_book_id, acquired_at)
) STRICT;

CREATE INDEX idx_book_file_refs_live
    ON book_file_refs(file_id) WHERE released_at IS NULL;
