-- Migration 021 — library_roots table.
--
-- Backlog item 3: move the operator-configurable scan-root list
-- from `tunables.security.library_roots` (config.toml) into a
-- proper DB table with a REST surface.
--
-- Why: a single config-file vector means operators have to ssh
-- in and edit TOML to add a library root, which doesn't compose
-- with the API-first rule (see the [api-first-cli-vs-gui-split]
-- memory). The CLI / GUI / future voice surfaces all need to
-- manage roots through the same API.
--
-- Migration semantics: the tunable stays defined (in
-- `SecurityTunables`) for one cycle as a **seed source** — on
-- daemon startup, if this table is empty AND the tunable list is
-- non-empty, each tunable root is INSERTed once with a log line.
-- Operators get a frictionless upgrade. After the next cycle the
-- tunable's docstring marks it deprecated and the daemon stops
-- seeding from it.

CREATE TABLE library_roots (
    root_id     INTEGER PRIMARY KEY AUTOINCREMENT,
    -- Canonicalised absolute path. Stored as TEXT (not a BLOB)
    -- because SQLite indexes / UNIQUE constraints are byte-exact
    -- and macOS/Linux paths are conventionally UTF-8.
    path        TEXT NOT NULL,
    -- Operator-friendly label ("Audiobooks NAS", "Local SSD").
    -- Optional — empty / NULL is fine; the canonical path is the
    -- identity.
    label       TEXT,
    -- Unix-seconds creation timestamp. Useful for the future
    -- "oldest-first" ordering on the management UI.
    created_at  INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    -- Soft-delete flag. DELETE handler sets `is_active = 0`
    -- instead of removing the row, so the path-validation gate
    -- (which queries `WHERE is_active = 1`) immediately stops
    -- accepting scans under that root, but the audit trail
    -- survives for forensics.
    is_active   INTEGER NOT NULL DEFAULT 1
        CHECK (is_active IN (0, 1)),
    UNIQUE(path)
) STRICT;

-- Path-validation lookups always filter by `is_active`; index
-- the column so scans against ~hundreds of roots stay fast.
CREATE INDEX idx_library_roots_active ON library_roots(is_active);
