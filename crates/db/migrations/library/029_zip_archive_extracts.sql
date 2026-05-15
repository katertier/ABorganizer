-- ADR-0047: ZIP archive extraction tracking.
--
-- One row per extracted source ZIP. `source_hash` enables
-- idempotent rescan — if the ZIP on disk still hashes to the
-- recorded value, the extract is reused as-is. A mismatch
-- triggers re-extraction.
--
-- `source_path` is UNIQUE so a single ZIP can't have two extract
-- rows; CBR / CB7 / CBT comics stay opaque (companion rows, no
-- extraction) per the NON-GOALS carve-out.

CREATE TABLE zip_archive_extracts (
    archive_id      INTEGER PRIMARY KEY AUTOINCREMENT,
    source_path     TEXT NOT NULL UNIQUE,
    extracted_path  TEXT NOT NULL,
    source_hash     TEXT NOT NULL,
    bytes_in        INTEGER NOT NULL,
    bytes_out       INTEGER NOT NULL,
    entries_count   INTEGER NOT NULL,
    extracted_at    INTEGER NOT NULL DEFAULT (strftime('%s','now'))
) STRICT;

CREATE INDEX idx_zip_archive_extracts_source ON zip_archive_extracts(source_path);
