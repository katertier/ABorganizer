-- 016_identity_disambiguation_pending.sql
--
-- Pending disambiguation surface (slice H.3.5, ADR-0026). When
-- identity-resolve's alias-junction lookup returns multiple parent
-- candidates AND the corroboration pass can't decide between them,
-- write a row here so the operator can resolve via `aborg names
-- resolve` (H.3.6). The book's `author_id` / linked
-- narrator/series stays NULL until resolution.
--
-- Three tables (one per identity kind) for symmetry with the
-- alias junctions. Polymorphic FKs would have been smaller in
-- schema but worse to query and CASCADE across. The shape mirrors
-- the alias junctions: parent FK + observed alias + audit
-- timestamps.
--
-- A separate candidate junction holds the ranked candidate set per
-- pending row, so the resolve surface can show "two David
-- Mitchells, candidate A (score 0.45) and candidate B (score
-- 0.42)" without having to re-run corroboration.

-- ── Author disambiguation ─────────────────────────────────────────
CREATE TABLE author_disambiguation_pending (
    pending_id        INTEGER PRIMARY KEY AUTOINCREMENT,
    book_id           INTEGER NOT NULL REFERENCES books(book_id) ON DELETE CASCADE,
    -- The spelling the source supplied (tag-read / Audnexus row that
    -- triggered the ambiguity). Stored for the resolve UI to show
    -- "what alias led to the conflict."
    observed_alias    TEXT NOT NULL,
    created_at        INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    resolved_at       INTEGER,
    -- Set when the operator picks one of the candidates (or
    -- supplies a `--create-new` row). NULL while pending.
    resolved_author_id INTEGER REFERENCES authors(author_id) ON DELETE SET NULL,
    -- One pending row per (book, alias) combo; re-running the stage
    -- after the row exists is an idempotent no-op.
    UNIQUE (book_id, observed_alias)
) STRICT;

CREATE INDEX idx_author_disambiguation_pending_unresolved
    ON author_disambiguation_pending(book_id)
    WHERE resolved_at IS NULL;

CREATE TABLE author_disambiguation_candidate (
    pending_id  INTEGER NOT NULL REFERENCES author_disambiguation_pending(pending_id)
                ON DELETE CASCADE,
    author_id   INTEGER NOT NULL REFERENCES authors(author_id) ON DELETE CASCADE,
    score       REAL NOT NULL,
    PRIMARY KEY (pending_id, author_id)
) STRICT;

CREATE INDEX idx_author_disambiguation_candidate_pending
    ON author_disambiguation_candidate(pending_id);

-- ── Narrator disambiguation ───────────────────────────────────────
CREATE TABLE narrator_disambiguation_pending (
    pending_id          INTEGER PRIMARY KEY AUTOINCREMENT,
    book_id             INTEGER NOT NULL REFERENCES books(book_id) ON DELETE CASCADE,
    observed_alias      TEXT NOT NULL,
    created_at          INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    resolved_at         INTEGER,
    resolved_narrator_id INTEGER REFERENCES narrators(narrator_id) ON DELETE SET NULL,
    UNIQUE (book_id, observed_alias)
) STRICT;

CREATE INDEX idx_narrator_disambiguation_pending_unresolved
    ON narrator_disambiguation_pending(book_id)
    WHERE resolved_at IS NULL;

CREATE TABLE narrator_disambiguation_candidate (
    pending_id   INTEGER NOT NULL REFERENCES narrator_disambiguation_pending(pending_id)
                 ON DELETE CASCADE,
    narrator_id  INTEGER NOT NULL REFERENCES narrators(narrator_id) ON DELETE CASCADE,
    score        REAL NOT NULL,
    PRIMARY KEY (pending_id, narrator_id)
) STRICT;

CREATE INDEX idx_narrator_disambiguation_candidate_pending
    ON narrator_disambiguation_candidate(pending_id);

-- ── Series disambiguation ─────────────────────────────────────────
CREATE TABLE series_disambiguation_pending (
    pending_id         INTEGER PRIMARY KEY AUTOINCREMENT,
    book_id            INTEGER NOT NULL REFERENCES books(book_id) ON DELETE CASCADE,
    observed_alias     TEXT NOT NULL,
    created_at         INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    resolved_at        INTEGER,
    resolved_series_id INTEGER REFERENCES series(series_id) ON DELETE SET NULL,
    UNIQUE (book_id, observed_alias)
) STRICT;

CREATE INDEX idx_series_disambiguation_pending_unresolved
    ON series_disambiguation_pending(book_id)
    WHERE resolved_at IS NULL;

CREATE TABLE series_disambiguation_candidate (
    pending_id INTEGER NOT NULL REFERENCES series_disambiguation_pending(pending_id)
               ON DELETE CASCADE,
    series_id  INTEGER NOT NULL REFERENCES series(series_id) ON DELETE CASCADE,
    score      REAL NOT NULL,
    PRIMARY KEY (pending_id, series_id)
) STRICT;

CREATE INDEX idx_series_disambiguation_candidate_pending
    ON series_disambiguation_candidate(pending_id);
