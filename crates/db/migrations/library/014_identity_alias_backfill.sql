-- 014_identity_alias_backfill.sql
--
-- Populate the new alias junctions from existing identity rows
-- (slice H.3.1, ADR-0026). Two parts per identity kind:
--
--   1. Every parent row gets one `is_prime=1` alias entry holding
--      the current canonical `name`. Stamp `source='canonical'`.
--   2. Any value in the legacy `authors.aliases` / `narrators.aliases`
--      newline-delimited column gets one alias per non-blank line.
--      Stamp `source='legacy_aliases'`, `is_prime=0`.
--
-- Series didn't have an aliases column to backfill, so its
-- backfill is just the canonical pass.
--
-- All inserts use `OR IGNORE` so re-running the migration on a
-- partially-populated junction is a no-op. (Production migrations
-- run once; the `OR IGNORE` is defensive against the prep-DB path
-- the sqlx-prepare script uses.)

-- ── Canonical (one prime per parent) ──────────────────────────────
INSERT OR IGNORE INTO author_aliases (author_id, alias, source, is_prime)
SELECT author_id, name, 'canonical', 1 FROM authors
WHERE name IS NOT NULL AND trim(name) <> '';

INSERT OR IGNORE INTO narrator_aliases (narrator_id, alias, source, is_prime)
SELECT narrator_id, name, 'canonical', 1 FROM narrators
WHERE name IS NOT NULL AND trim(name) <> '';

INSERT OR IGNORE INTO series_aliases (series_id, alias, source, is_prime)
SELECT series_id, name, 'canonical', 1 FROM series
WHERE name IS NOT NULL AND trim(name) <> '';

-- ── Legacy aliases (one row per non-blank line) ───────────────────
-- SQLite has no built-in `string_split`. The classic recursive-CTE
-- pattern below splits on newlines without depending on extensions.
-- Empty / whitespace-only lines are filtered.

WITH RECURSIVE author_split(author_id, line, rest) AS (
    -- Seed: each author with a non-empty aliases column. Trailing
    -- newline appended so the recursion's `instr` always finds one.
    SELECT
        author_id,
        '',
        aliases || char(10)
    FROM authors
    WHERE aliases IS NOT NULL AND trim(aliases) <> ''

    UNION ALL

    -- Recurse: cut off the next line up to and including its newline.
    SELECT
        author_id,
        substr(rest, 1, instr(rest, char(10)) - 1),
        substr(rest, instr(rest, char(10)) + 1)
    FROM author_split
    WHERE instr(rest, char(10)) > 0
)
INSERT OR IGNORE INTO author_aliases (author_id, alias, source, is_prime)
SELECT author_id, trim(line), 'legacy_aliases', 0
FROM author_split
WHERE trim(line) <> '';

WITH RECURSIVE narrator_split(narrator_id, line, rest) AS (
    SELECT
        narrator_id,
        '',
        aliases || char(10)
    FROM narrators
    WHERE aliases IS NOT NULL AND trim(aliases) <> ''

    UNION ALL

    SELECT
        narrator_id,
        substr(rest, 1, instr(rest, char(10)) - 1),
        substr(rest, instr(rest, char(10)) + 1)
    FROM narrator_split
    WHERE instr(rest, char(10)) > 0
)
INSERT OR IGNORE INTO narrator_aliases (narrator_id, alias, source, is_prime)
SELECT narrator_id, trim(line), 'legacy_aliases', 0
FROM narrator_split
WHERE trim(line) <> '';
