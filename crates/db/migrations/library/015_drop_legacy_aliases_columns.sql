-- 015_drop_legacy_aliases_columns.sql
--
-- Drop the `aliases` column from `authors` and `narrators` (slice
-- H.3.1, ADR-0026). The data already lives in `author_aliases` /
-- `narrator_aliases` (migration 014); the legacy column is dead.
--
-- SQLite supports `ALTER TABLE … DROP COLUMN` from 3.35 onward
-- (we ship on 3.45+), so this is a one-liner per table. No
-- table-rebuild dance needed.

ALTER TABLE authors   DROP COLUMN aliases;
ALTER TABLE narrators DROP COLUMN aliases;
