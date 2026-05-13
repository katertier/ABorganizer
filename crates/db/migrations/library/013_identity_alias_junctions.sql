-- 013_identity_alias_junctions.sql
--
-- Add per-identity alias junction tables (slice H.3.1, ADR-0026).
-- The legacy `authors.aliases` / `narrators.aliases` newline-string
-- columns are dead today (no writer in the workspace); migrations 014
-- (backfill from the legacy column if anything was there) + 015 (drop
-- the legacy columns) finish the transition.

-- ── Author aliases ────────────────────────────────────────────────
CREATE TABLE author_aliases (
    alias_id   INTEGER PRIMARY KEY AUTOINCREMENT,
    author_id  INTEGER NOT NULL REFERENCES authors(author_id) ON DELETE CASCADE,
    -- The observed spelling. Match-time comparisons use
    -- `COLLATE NOCASE` so "J. C. Williams" and "j. c. williams" hit
    -- the same row.
    alias      TEXT NOT NULL,
    -- Where the alias was first observed:
    --   'canonical'       — the row matching `authors.name` at
    --                       insert time (every parent gets one of
    --                       these at insert).
    --   'audnexus'        — added by `audnexus-enrich` from an
    --                       Audnexus payload (contributor name on a
    --                       different book, alternate spelling on
    --                       the bio page).
    --   'tag_file'        — added by `tag-read` when the embedded
    --                       tag spells the name differently.
    --   'manual'          — operator added via `aborg names alias`
    --                       or the web UI.
    --   'legacy_aliases'  — backfill from the dropped
    --                       `authors.aliases` newline column
    --                       (migration 014).
    source     TEXT NOT NULL,
    -- Exactly one row per author may have `is_prime = 1`. The
    -- partial unique index below enforces this. NULL not allowed;
    -- default 0 so insert paths can ignore the column unless they
    -- mean to set it.
    is_prime   INTEGER NOT NULL DEFAULT 0,
    added_at   INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    UNIQUE (author_id, alias)
) STRICT;

CREATE UNIQUE INDEX idx_author_aliases_one_prime
    ON author_aliases(author_id)
    WHERE is_prime = 1;

-- Match-by-alias path: case-insensitive. Used by
-- `identity-resolve` when no ASIN is available.
CREATE INDEX idx_author_aliases_alias
    ON author_aliases(alias COLLATE NOCASE);

-- ── Narrator aliases ──────────────────────────────────────────────
CREATE TABLE narrator_aliases (
    alias_id     INTEGER PRIMARY KEY AUTOINCREMENT,
    narrator_id  INTEGER NOT NULL REFERENCES narrators(narrator_id) ON DELETE CASCADE,
    alias        TEXT NOT NULL,
    source       TEXT NOT NULL,
    is_prime     INTEGER NOT NULL DEFAULT 0,
    added_at     INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    UNIQUE (narrator_id, alias)
) STRICT;

CREATE UNIQUE INDEX idx_narrator_aliases_one_prime
    ON narrator_aliases(narrator_id)
    WHERE is_prime = 1;

CREATE INDEX idx_narrator_aliases_alias
    ON narrator_aliases(alias COLLATE NOCASE);

-- ── Series aliases ────────────────────────────────────────────────
-- Series didn't have an aliases column at all before this slice.
-- Same shape as author/narrator junctions for symmetry.
CREATE TABLE series_aliases (
    alias_id   INTEGER PRIMARY KEY AUTOINCREMENT,
    series_id  INTEGER NOT NULL REFERENCES series(series_id) ON DELETE CASCADE,
    alias      TEXT NOT NULL,
    source     TEXT NOT NULL,
    is_prime   INTEGER NOT NULL DEFAULT 0,
    added_at   INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    UNIQUE (series_id, alias)
) STRICT;

CREATE UNIQUE INDEX idx_series_aliases_one_prime
    ON series_aliases(series_id)
    WHERE is_prime = 1;

CREATE INDEX idx_series_aliases_alias
    ON series_aliases(alias COLLATE NOCASE);
