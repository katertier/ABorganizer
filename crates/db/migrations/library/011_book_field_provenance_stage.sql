-- Slice H.1.2: add `book_field_provenance.stage` — the StageId
-- of whichever stage wrote the row.
--
-- ── Why add a stage column ────────────────────────────────────────
--
-- `Stage::reset(book_id)` (slice H.1.5) needs to clear only the
-- provenance rows the stage being reset wrote. Pre-H.1 the only
-- "who wrote this" indicator was `source` (free-form string like
-- 'audnexus_asin_us' or 'tag_file'). The mapping `source → stage`
-- works for hand-coded cases but is brittle:
--
-- - `run-transcript-extractors` writes one row per sub-extractor,
--   each with a different `source` (the extractor's `name()`).
--   Reset would need to enumerate every current + future
--   extractor name.
-- - `audnexus-enrich` writes rows with `source =
--   'audnexus_asin_<region>'` — a prefix match works but breaks
--   if a future stage's source string happens to start the same
--   way.
-- - Manual / future-imported rows (e.g. ABtagger import) carry
--   `source = 'manual'` or similar with no clear stage owner.
--
-- An explicit `stage` column makes reset's WHERE clause exact,
-- mirrors the same column on `pipeline_progress`, and gives the
-- gap-reporting view a way to show "what each stage produced for
-- this book."
--
-- ── Why a table rebuild ───────────────────────────────────────────
--
-- SQLite has no `ALTER TABLE ... ADD COLUMN ... NOT NULL` without
-- a default. Following the same rebuild pattern as migration 005
-- (the `field` CHECK constraint).
--
-- ── Backfill mapping ──────────────────────────────────────────────
--
-- The mapping below recovers `stage` from `source` for rows
-- already in pre-alpha dev DBs. New writers (post-this-migration)
-- pass `stage` explicitly. Unknown `source` values fall back to
-- the most-likely producing stage (`run-transcript-extractors`),
-- which is what writes the catch-all extractor candidates.
--
-- ── Vocabulary ────────────────────────────────────────────────────
--
-- No CHECK constraint on `stage` — the Stage trait's `name()`
-- value is the source of truth, validated at the Rust layer.
-- Adding a CHECK would force every new stage to update this
-- migration on top of its own; the dispatcher's
-- `KNOWN_STAGE_NAMES` table already serves as the
-- registered-universe source of truth.

CREATE TABLE book_field_provenance_new (
    provenance_id  INTEGER PRIMARY KEY AUTOINCREMENT,
    book_id        INTEGER NOT NULL REFERENCES books(book_id) ON DELETE CASCADE,
    field          TEXT NOT NULL CHECK (field IN (
        'title', 'subtitle', 'description', 'language',
        'release_date', 'duration_seconds', 'asin', 'isbn',
        'author', 'narrator', 'publisher', 'series', 'genre',
        'cover_url', 'abridged', 'explicit'
    )),
    value          TEXT,
    source         TEXT NOT NULL,
    stage          TEXT NOT NULL,
    confidence     REAL NOT NULL,
    is_winner      INTEGER NOT NULL DEFAULT 0,
    external_id    TEXT,
    recorded_at    INTEGER NOT NULL DEFAULT (strftime('%s','now'))
) STRICT;

INSERT INTO book_field_provenance_new (
    provenance_id, book_id, field, value, source, stage, confidence,
    is_winner, external_id, recorded_at
)
SELECT
    provenance_id,
    book_id,
    field,
    value,
    source,
    CASE
        WHEN source = 'tag_file'              THEN 'tag-read'
        WHEN source = 'audible_search'        THEN 'audible-search'
        WHEN source LIKE 'audnexus_asin%'     THEN 'audnexus-enrich'
        WHEN source = 'nl_language_samples'   THEN 'transcribe-samples'
        WHEN source = 'nl_language'           THEN 'transcribe-head-tail'
        -- Best-effort fallback. Pre-alpha dev DBs don't carry
        -- rows from unenumerated stages; if a row hits this
        -- branch it's almost certainly a transcript-extractors
        -- sub-source. New writers (post-migration) bind `stage`
        -- explicitly, so this branch decays to dead code.
        ELSE 'run-transcript-extractors'
    END AS stage,
    confidence,
    is_winner,
    external_id,
    recorded_at
FROM book_field_provenance;

DROP TABLE book_field_provenance;

ALTER TABLE book_field_provenance_new RENAME TO book_field_provenance;

CREATE INDEX idx_book_field_provenance_stage
    ON book_field_provenance(stage);
CREATE INDEX idx_book_field_provenance_book_stage
    ON book_field_provenance(book_id, stage);
