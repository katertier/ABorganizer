-- slice 4A: per-file audiologo splice schema + status column +
-- chapter-boundary-verified column. See ADR-0024 for the full
-- design context.
--
-- This is the foundational migration for the audiologo theme.
-- Three concerns:
--
-- 1. Per-file mid-text splice rows (`book_file_audiologos`).
--    The old `books.audiologo_intro_ms` / `_outro_ms` columns
--    encoded head-cut semantics; the actual cut targets a
--    range INSIDE the file (`[jingle_start_ms, jingle_end_ms]`)
--    to preserve "Title by Author" voiceovers that follow
--    publisher jingles. The old columns stay (NULL-default)
--    for backward compatibility but are not written by 4A+.
--
-- 2. First-class absence on books (`books.audiologo_status`).
--    The Libation case (Audnexus says intro_ms=N but the
--    audio has been stripped) needs a positive metadata
--    state, not NULL ambiguity.
--
-- 3. Chapter boundary verification flag
--    (`chapters.boundary_verified`). When an audiologo trim
--    applies, chapter offsets shift; the result is verified
--    against transcript content + detected silences. A
--    chapter that lands mid-utterance after the shift is
--    flagged.

-- ── per-file audiologo rows ──────────────────────────────────────
CREATE TABLE book_file_audiologos (
    audiologo_row_id    INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id             INTEGER NOT NULL REFERENCES book_files(file_id)
                            ON DELETE CASCADE,
    kind                TEXT NOT NULL
                            CHECK (kind IN ('intro','outro')),
    -- Splice range in the file's own time-base (ms from file
    -- start). For outros, jingle_start_ms / _end_ms are still
    -- offsets-from-file-start (NOT offset-from-file-end), so
    -- the chapter-shift maths is uniform.
    jingle_start_ms     INTEGER NOT NULL,
    jingle_end_ms       INTEGER NOT NULL,
    -- Silence inserted after the cut to soften mid-utterance
    -- splices. NULL = use AudiologoTunables.{intro|outro}_padding_ms
    -- default. Detector overrides when the boundary lands
    -- mid-utterance (the cut needs more padding); leaves NULL
    -- when the boundary is a clean sentence/silence break.
    padding_ms          INTEGER,
    -- Which Method produced this row. See ab_audiologo::Method.
    method              TEXT NOT NULL CHECK (method IN (
        'catalog_brand_duration',
        'fingerprint_full',
        'fingerprint_bookend',
        'fingerprint_and_transcript',
        'transcript_only',
        'manual')),
    -- The audiologos row that matched (NULL for transcript_only +
    -- manual-without-fingerprint-add cases).
    audiologo_id        INTEGER REFERENCES audiologos(audiologo_id)
                            ON DELETE SET NULL,
    confidence          REAL NOT NULL DEFAULT 0.0,
    -- State machine: candidate / applied / rejected / re_detected.
    -- See ADR-0024 § state-machine diagram for valid transitions.
    status              TEXT NOT NULL CHECK (status IN (
        'candidate','applied','rejected','re_detected')),
    detected_at         INTEGER NOT NULL
                            DEFAULT (strftime('%s','now')),
    applied_at          INTEGER,
    rejected_at         INTEGER
) STRICT;

CREATE INDEX idx_book_file_audiologos_file
    ON book_file_audiologos(file_id);
CREATE INDEX idx_book_file_audiologos_status
    ON book_file_audiologos(status);
CREATE INDEX idx_book_file_audiologos_audiologo
    ON book_file_audiologos(audiologo_id);

-- A file can only have one 'applied' row per kind at a time;
-- multiple 'candidate' rows are allowed (different methods
-- competing for the same trim location). UNIQUE on the
-- partial index covers the applied-uniqueness invariant.
CREATE UNIQUE INDEX idx_book_file_audiologos_applied_unique
    ON book_file_audiologos(file_id, kind)
    WHERE status = 'applied';

-- ── first-class absence semantics on books ────────────────────────
ALTER TABLE books ADD COLUMN audiologo_status TEXT NOT NULL
    DEFAULT 'unknown'
    CHECK (audiologo_status IN (
        'unknown',    -- detection hasn't run
        'detected',   -- at least one candidate row exists
        'applied',    -- at least one applied row exists
        'stripped',   -- catalog said yes, fingerprint found nothing
                      -- (Libation-suspect); also fires on cross-kind
                      -- match warnings
        'none',       -- no catalog hint AND no detection match
                      -- (e.g. self-published or unbranded)
        'rejected')); -- user reviewed and said "don't trim this"

-- ── chapter boundary verification flag ────────────────────────────
-- When an audiologo trim applies, chapter offsets shift; the
-- result is verified against transcript content and detected
-- silences. NULL = not yet verified (no trim applied, or
-- chapter newer than the last verification pass). 0 = mid-
-- utterance (boundary lands inside a sentence — user should
-- review). 1 = clean boundary (sentence end / silence).
ALTER TABLE chapters ADD COLUMN boundary_verified INTEGER;

-- ── audiologos.verified_via — two new bootstrap provenance values ─
-- Migration only — the audiologos table's verified_via CHECK
-- constraint needs two new values for 4B (catalog_bootstrap)
-- and 4A (ab_tagger_import). SQLite doesn't support ALTER
-- TABLE on CHECK constraints, so we rebuild the table.

CREATE TABLE audiologos_v2 (
    audiologo_id      INTEGER PRIMARY KEY AUTOINCREMENT,
    name              TEXT NOT NULL,
    kind              TEXT NOT NULL CHECK (kind IN ('intro','outro')),
    publisher_id      INTEGER REFERENCES publishers(publisher_id),
    fingerprint       BLOB NOT NULL,
    duration_ms       INTEGER NOT NULL,
    match_threshold   REAL NOT NULL DEFAULT 0.85,
    match_count       INTEGER NOT NULL DEFAULT 0,
    last_matched_at   INTEGER,
    source_book_id    INTEGER REFERENCES books(book_id) ON DELETE SET NULL,
    source_offset_ms  INTEGER NOT NULL DEFAULT 0,
    verified_via      TEXT NOT NULL CHECK (verified_via IN (
        'manual',             -- aborg audiologos cut --add-fingerprint
        'review_confirmed',   -- user-confirmed during review pass
        'silence',            -- auto-bootstrapped from a silence cut
                              -- (needs transcript corroboration to fire)
        'transcription',      -- auto with transcript publisher hit
        'seed',               -- shipped via seed-data
        'import',             -- generic import (preserve compat)
        'catalog_bootstrap',  -- 4B: sampled at Audnexus brand_duration
                              --     offset, confirmed by fingerprint
                              --     match within the bootstrap window
        'ab_tagger_import')), -- 4A: imported from ABtagger; user
                              --     verifies each via the review pass
    confidence        REAL NOT NULL DEFAULT 0.0,
    created_at        INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    updated_at        INTEGER NOT NULL DEFAULT (strftime('%s','now'))
) STRICT;

INSERT INTO audiologos_v2 SELECT * FROM audiologos;
DROP TABLE audiologos;
ALTER TABLE audiologos_v2 RENAME TO audiologos;

CREATE INDEX idx_audiologos_kind ON audiologos(kind);
CREATE INDEX idx_audiologos_match_count
    ON audiologos(match_count DESC);

-- Reset the sqlite_sequence row to track the renamed table's
-- highest existing audiologo_id, so post-migration INSERTs
-- continue the existing PK sequence instead of starting from
-- 1 and colliding with existing rows. No-op when the table is
-- empty (current state); future-proofs when later slices
-- populate audiologos before this migration runs against
-- an older DB snapshot.
DELETE FROM sqlite_sequence WHERE name = 'audiologos_v2';
INSERT INTO sqlite_sequence (name, seq)
    SELECT 'audiologos', COALESCE(MAX(audiologo_id), 0) FROM audiologos
    WHERE NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = 'audiologos');
UPDATE sqlite_sequence
    SET seq = COALESCE((SELECT MAX(audiologo_id) FROM audiologos), 0)
    WHERE name = 'audiologos';
