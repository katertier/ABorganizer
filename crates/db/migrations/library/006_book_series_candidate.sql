-- Slice C5.6: series resolution candidate table + primary/secondary
-- flag on `book_series`.
--
-- ── Why a dedicated candidate table ──────────────────────────────
--
-- `book_field_provenance` is shaped for scalar `(book_id, field,
-- value)` candidates. Series resolution needs three more pieces:
--   * series_asin   — Audnexus carries it; tag/filename don't
--   * position      — REAL, parsed from Audnexus's string form
--                     ("1", "1.5", "1.0a"); some sources omit it
--   * is_primary    — distinguishes `seriesPrimary` from
--                     `seriesSecondary` (one book can belong to
--                     multiple series; the UI defaults to primary)
--
-- A dedicated table mirrors the existing junction-input pattern in
-- ABtagger (verified at `~/dev/ABtagger/src/db/upsert.rs`) without
-- inheriting that codebase's schema choices wholesale: kept the
-- single REAL `position` (no `position_from` / `position_to` until
-- omnibus support is needed) and used `is_primary` boolean instead
-- of ABtagger's free-form `production_type`.
--
-- Identity-resolve reads from this table (clear-then-add into
-- `book_series` with case-insensitive name fallback after
-- `series.audible_id` lookup), so the consensus path is identical
-- in shape to `resolve_author` / `resolve_narrators`.
--
-- The `ab_core::Field::Series` enum variant stays as-is — tag-read's
-- audit row in `book_field_provenance` is moving to this table
-- because the shape fits, but other places (admin UI, future
-- series detection sources) might want to write provenance rows.
-- The migration 005 CHECK constraint already accepts 'series'.

CREATE TABLE book_series_candidate (
    candidate_id   INTEGER PRIMARY KEY AUTOINCREMENT,
    book_id        INTEGER NOT NULL REFERENCES books(book_id) ON DELETE CASCADE,
    -- Provenance source token. Mirrors `book_field_provenance.source`
    -- vocabulary: `audnexus_asin_<region>`, `tag_file`, `manual`, …
    source         TEXT NOT NULL,
    -- Series name as the source reported it. identity-resolve
    -- canonicalises at insert time (case-insensitive match against
    -- `series.name`).
    series_name    TEXT NOT NULL,
    -- Canonical external identifier when the source supplies one.
    -- Audnexus rows carry `seriesPrimary.asin` / `seriesSecondary.asin`;
    -- tag-read writes NULL (album-tag has no ASIN). identity-resolve
    -- prefers `series.audible_id` match when this is non-NULL.
    series_asin    TEXT,
    -- Parsed position. Audnexus stores it as a string ("1", "1.5",
    -- "1.0a"); the catalog writer parses to REAL and writes NULL
    -- on parse failure (logged for monitoring). Tag-read and
    -- filename sources typically write NULL.
    position       REAL,
    -- 1 = Audnexus seriesPrimary; 0 = Audnexus seriesSecondary
    -- (or any source's "secondary" variant). Tag-read defaults to
    -- 1 (album tag is typically the primary series).
    is_primary     INTEGER NOT NULL DEFAULT 1,
    -- 0.0 - 1.0, same scale as `book_field_provenance.confidence`.
    confidence     REAL NOT NULL,
    recorded_at    INTEGER NOT NULL DEFAULT (strftime('%s','now'))
) STRICT;

CREATE INDEX idx_book_series_candidate_book ON book_series_candidate(book_id);
-- Fast lookup for the identity-resolve "all candidates by series_asin"
-- path. The composite catches both `series_asin IS NULL` (tag-only
-- rows) and matched rows in one scan.
CREATE INDEX idx_book_series_candidate_asin ON book_series_candidate(series_asin);

-- Add primary/secondary distinction to the existing book_series
-- junction. Single-series-per-book is the common case; the
-- DEFAULT 1 keeps every existing row "primary" without needing a
-- backfill step (no production data yet anyway).
ALTER TABLE book_series ADD COLUMN is_primary INTEGER NOT NULL DEFAULT 1;
