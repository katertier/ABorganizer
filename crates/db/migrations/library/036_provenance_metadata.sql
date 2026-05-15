-- Add `book_field_provenance.metadata` JSON column.
--
-- Part of the schema-as-if-planned-from-day-one slice (2026-05-15
-- retrospective, item #2). The provenance shape today is scalar-only:
-- `(book_id, field, value, source, stage, confidence, is_winner, external_id)`.
-- Per-field extras that don't fit the scalar model — series position (REAL),
-- per-source date ranges, future confidence sub-scores — have been forcing
-- parallel side-tables (C5.6 spun up `book_series_candidate` for exactly
-- this reason).
--
-- A `metadata TEXT` column carrying free-form JSON keeps the scalar core
-- intact while letting any source attach typed extras without a new
-- migration per shape. Consumers parse the JSON only if they care; the
-- field stays NULL for the (overwhelming) common case.
--
-- The column is added unconditional NULL; no backfill. Existing rows
-- remain untouched. New writes opt in by emitting JSON when extras
-- exist.

ALTER TABLE book_field_provenance ADD COLUMN metadata TEXT;
