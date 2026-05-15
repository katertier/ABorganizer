-- Library schema — single consolidated migration.
--
-- Squashed from 38 incremental migrations on 2026-05-15 per the
-- schema-as-if-planned-from-day-one retrospective (item #1). Pre-1.0
-- migrations are explicitly NOT a one-way ratchet (.claude/CLAUDE.md §
-- "Migrations during development"), so the chain was consolidated
-- ahead of first tagged release.
--
-- The historical chain is preserved in git history; the section
-- dividers below mark the original boundaries for readers tracing a
-- specific column / constraint back to its motivating decision.
--
-- After first tagged release, this file is append-only; new schema
-- changes land as their own numbered migration.

-- ── original migration: 001_initial.sql ─────────────────────────────────────────────

-- ABorganizer library schema — v0.1
--
-- Holds canonical, user-facing, persistent data. Designed for ~100k
-- books, multiple users, multiple devices. WAL mode + foreign keys ON.
--
-- Naming: snake_case tables; singular for one-to-one tables, plural for
-- multi-row collections. All timestamps are unix seconds in UTC; the
-- display layer converts to local timezone using the OS locale.

-- ── Provenance + audit ─────────────────────────────────────────────
-- Every value on a book has multiple candidates (Audnexus says X,
-- Audible says Y, MP4 tag says Z); merge picks one. We keep every
-- candidate so re-merge with new weights is free + the conflict
-- surface is visible to the user.
CREATE TABLE book_field_provenance (
    provenance_id  INTEGER PRIMARY KEY AUTOINCREMENT,
    book_id        INTEGER NOT NULL REFERENCES books(book_id) ON DELETE CASCADE,
    field          TEXT NOT NULL,         -- title, author, narrator, asin, language, ...
    value          TEXT,                  -- NULL when the candidate is "absent"
    source         TEXT NOT NULL,         -- audnexus_asin, audible_search, tag_mp4, transcript_match, nl_language, manual
    confidence     REAL NOT NULL,         -- 0.0 - 1.0
    is_winner      INTEGER NOT NULL DEFAULT 0,
    -- Canonical external identifier attached to this candidate
    -- when the source supplies one. Audnexus contributor rows
    -- carry the Audnexus author/narrator ASIN here; identity-
    -- resolve uses it to match against authors.audible_id /
    -- narrators.audible_id before falling back to name matching.
    -- Other sources may populate this with MusicBrainz IDs, ISBNs,
    -- etc. as they're added.
    external_id    TEXT,
    recorded_at    INTEGER NOT NULL DEFAULT (strftime('%s','now'))
) STRICT;

-- ── Identities ─────────────────────────────────────────────────────
CREATE TABLE authors (
    author_id      INTEGER PRIMARY KEY AUTOINCREMENT,
    name           TEXT NOT NULL,
    name_sort      TEXT,
    bio            TEXT,
    image_url      TEXT,
    audible_id     TEXT,
    aliases        TEXT,    -- newline-delimited variant names (post-consolidate)
    created_at     INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    updated_at     INTEGER NOT NULL DEFAULT (strftime('%s','now'))
) STRICT;
CREATE UNIQUE INDEX idx_authors_audible ON authors(audible_id) WHERE audible_id IS NOT NULL;
CREATE INDEX idx_authors_name ON authors(name);

CREATE TABLE narrators (
    narrator_id    INTEGER PRIMARY KEY AUTOINCREMENT,
    name           TEXT NOT NULL,
    name_sort      TEXT,
    bio            TEXT,
    image_url      TEXT,
    audible_id     TEXT,
    aliases        TEXT,
    created_at     INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    updated_at     INTEGER NOT NULL DEFAULT (strftime('%s','now'))
) STRICT;
CREATE UNIQUE INDEX idx_narrators_audible ON narrators(audible_id) WHERE audible_id IS NOT NULL;
CREATE INDEX idx_narrators_name ON narrators(name);

CREATE TABLE series (
    series_id         INTEGER PRIMARY KEY AUTOINCREMENT,
    name              TEXT NOT NULL,
    name_sort         TEXT,
    franchise_prefix  TEXT,     -- common title prefix across the series (for title_sort)
    audible_id        TEXT,
    ended_state       INTEGER DEFAULT 0,    -- 0=unknown 1=ongoing 2=ended
    created_at        INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    updated_at        INTEGER NOT NULL DEFAULT (strftime('%s','now'))
) STRICT;
CREATE INDEX idx_series_name ON series(name);

CREATE TABLE publishers (
    publisher_id   INTEGER PRIMARY KEY AUTOINCREMENT,
    name           TEXT NOT NULL UNIQUE,
    canonical_name TEXT,                   -- normalized form (e.g. "Audible Studios")
    created_at     INTEGER NOT NULL DEFAULT (strftime('%s','now'))
) STRICT;

-- ── Books ──────────────────────────────────────────────────────────
CREATE TABLE books (
    book_id              INTEGER PRIMARY KEY AUTOINCREMENT,
    title                TEXT NOT NULL,
    title_sort           TEXT,
    subtitle             TEXT,
    author_id            INTEGER REFERENCES authors(author_id),
    publisher_id         INTEGER REFERENCES publishers(publisher_id),
    description          TEXT,
    language             TEXT,                -- BCP-47 (en, de, ...)
    duration_ms          INTEGER,             -- post-audiologo trim
    raw_duration_ms      INTEGER,             -- pre-trim total
    audiologo_intro_ms   INTEGER,
    audiologo_outro_ms   INTEGER,
    asin                 TEXT,
    original_asin        TEXT,                -- pre-correction (region walk source)
    isbn                 TEXT,
    abridged             INTEGER,
    explicit             INTEGER,
    release_date         TEXT,                -- ISO 8601 date string
    cover_url            TEXT,
    -- Whole-book fingerprint (chromaprint, 30-second windows joined).
    -- NULL until the `fingerprint` stage runs.
    book_fingerprint     BLOB,
    fingerprint_offsets  TEXT,                -- JSON array of offsets used
    created_at           INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    updated_at           INTEGER NOT NULL DEFAULT (strftime('%s','now'))
) STRICT;
CREATE INDEX idx_books_author ON books(author_id);
CREATE INDEX idx_books_publisher ON books(publisher_id);
CREATE INDEX idx_books_asin ON books(asin);
CREATE INDEX idx_books_language ON books(language);

-- ── Files ──────────────────────────────────────────────────────────
CREATE TABLE book_files (
    file_id        INTEGER PRIMARY KEY AUTOINCREMENT,
    book_id        INTEGER NOT NULL REFERENCES books(book_id) ON DELETE CASCADE,
    file_path      TEXT NOT NULL UNIQUE,
    file_size      INTEGER,
    modified_at    INTEGER,
    format         TEXT,            -- m4b, m4a, mp3, flac, opus, aax, ...
    bitrate_kbps   INTEGER,
    sample_rate_hz INTEGER,
    channels       INTEGER,
    codec          TEXT,
    duration_ms    INTEGER,
    loudness_lufs  REAL,
    is_active      INTEGER NOT NULL DEFAULT 1,
    file_hash      TEXT,            -- blake3 of (size, mtime, first 4KB) — quick re-scan dedupe
    checked_at     INTEGER
) STRICT;
CREATE INDEX idx_book_files_book ON book_files(book_id);
CREATE INDEX idx_book_files_active ON book_files(is_active);
CREATE INDEX idx_book_files_hash ON book_files(file_hash);

-- ── Joins ──────────────────────────────────────────────────────────
CREATE TABLE book_narrator (
    book_id        INTEGER REFERENCES books(book_id) ON DELETE CASCADE,
    narrator_id    INTEGER REFERENCES narrators(narrator_id) ON DELETE CASCADE,
    PRIMARY KEY (book_id, narrator_id)
) STRICT;

CREATE TABLE book_series (
    book_id        INTEGER REFERENCES books(book_id) ON DELETE CASCADE,
    series_id      INTEGER REFERENCES series(series_id) ON DELETE CASCADE,
    position       REAL,
    PRIMARY KEY (book_id, series_id)
) STRICT;
CREATE INDEX idx_book_series_series ON book_series(series_id);

CREATE TABLE genres (
    genre_id       INTEGER PRIMARY KEY AUTOINCREMENT,
    canonical_id   TEXT NOT NULL UNIQUE,     -- "fantasy-urban"
    display_name   TEXT NOT NULL,            -- "Urban Fantasy"
    audible_id     TEXT
) STRICT;

CREATE TABLE book_genre (
    book_id        INTEGER REFERENCES books(book_id) ON DELETE CASCADE,
    genre_id       INTEGER REFERENCES genres(genre_id) ON DELETE CASCADE,
    confidence     REAL DEFAULT 1.0,
    PRIMARY KEY (book_id, genre_id)
) STRICT;

CREATE TABLE book_tags (
    tag_id         INTEGER PRIMARY KEY AUTOINCREMENT,
    book_id        INTEGER REFERENCES books(book_id) ON DELETE CASCADE,
    tag            TEXT NOT NULL,
    source         TEXT NOT NULL,            -- audible_subcat, dna, manual, ...
    UNIQUE (book_id, tag, source)
) STRICT;

CREATE TABLE chapters (
    chapter_id     INTEGER PRIMARY KEY AUTOINCREMENT,
    book_id        INTEGER NOT NULL REFERENCES books(book_id) ON DELETE CASCADE,
    idx            INTEGER NOT NULL,         -- 0-based position
    start_ms       INTEGER NOT NULL,
    end_ms         INTEGER NOT NULL,
    title          TEXT NOT NULL,
    source         TEXT NOT NULL,            -- audnexus, embedded, cue, epub, transcript, silence
    -- Set by the `chapter-pick-winner` stage. Exactly one source's
    -- rows per book are marked winners; the player joins on
    -- `is_winner = 1` to get a single consistent ToC even when
    -- multiple sources have populated chapters.
    is_winner      INTEGER NOT NULL DEFAULT 0,
    -- UNIQUE includes `source` so the same book can carry chapter
    -- lists from multiple sources concurrently. Indexing on
    -- `(book_id, source)` keeps the per-source clear-then-add path
    -- cheap.
    UNIQUE (book_id, idx, source)
) STRICT;
CREATE INDEX idx_chapters_book_source ON chapters(book_id, source);
CREATE INDEX idx_chapters_winners ON chapters(book_id) WHERE is_winner = 1;

-- ── Audiologo fingerprints ─────────────────────────────────────────
-- These are the RMS thumbprints used for intro/outro trim. Each row
-- has a real FK back to the source book it came from — no string-LIKE
-- joins (a problem in the previous codebase).
CREATE TABLE audiologos (
    audiologo_id      INTEGER PRIMARY KEY AUTOINCREMENT,
    name              TEXT NOT NULL,
    kind              TEXT NOT NULL CHECK (kind IN ('intro','outro')),
    publisher_id      INTEGER REFERENCES publishers(publisher_id),
    fingerprint       BLOB NOT NULL,
    duration_ms       INTEGER NOT NULL,
    match_threshold   REAL NOT NULL DEFAULT 0.85,
    match_count       INTEGER NOT NULL DEFAULT 0,
    last_matched_at   INTEGER,
    -- Source book this fingerprint was derived from. NULL only for
    -- seeded / imported fingerprints with no in-library source.
    source_book_id    INTEGER REFERENCES books(book_id) ON DELETE SET NULL,
    source_offset_ms  INTEGER NOT NULL DEFAULT 0,
    verified_via      TEXT NOT NULL CHECK (verified_via IN
        ('manual','review_confirmed','silence','transcription','seed','import')),
    confidence        REAL NOT NULL DEFAULT 0.0,
    created_at        INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    updated_at        INTEGER NOT NULL DEFAULT (strftime('%s','now'))
) STRICT;
CREATE INDEX idx_audiologos_kind ON audiologos(kind);
CREATE INDEX idx_audiologos_match_count ON audiologos(match_count DESC);

-- ── Identity (whole-book chromaprint) ──────────────────────────────
-- Whole-book identity fingerprints for duplicate detection. Stored
-- separately from audiologos because the algorithm + use-case differ.
CREATE TABLE book_fingerprints (
    fingerprint_id   INTEGER PRIMARY KEY AUTOINCREMENT,
    book_id          INTEGER NOT NULL REFERENCES books(book_id) ON DELETE CASCADE,
    offset_sec       INTEGER NOT NULL,        -- 0, 25%, 50%, 75% etc.
    duration_sec     INTEGER NOT NULL,        -- typically 30
    fingerprint      BLOB NOT NULL,           -- chromaprint hash sequence
    algorithm        TEXT NOT NULL DEFAULT 'chromaprint-v2',
    UNIQUE (book_id, offset_sec, algorithm)
) STRICT;
CREATE INDEX idx_book_fingerprints_book ON book_fingerprints(book_id);

-- ── Users + auth (single-user-friendly, multi-user-ready) ──────────
CREATE TABLE users (
    user_id        INTEGER PRIMARY KEY AUTOINCREMENT,
    name           TEXT NOT NULL UNIQUE,
    display_name   TEXT,
    is_admin       INTEGER NOT NULL DEFAULT 0,
    created_at     INTEGER NOT NULL DEFAULT (strftime('%s','now'))
) STRICT;
-- Default single-user row.
INSERT INTO users (user_id, name, display_name, is_admin) VALUES (1, 'default', 'Default', 1);

CREATE TABLE tokens (
    token_id       INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id        INTEGER NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
    token_hash     TEXT NOT NULL UNIQUE,    -- blake3 of the raw token
    nickname       TEXT,                    -- "iPad", "Plappa-iPhone", ...
    scopes         TEXT NOT NULL,           -- JSON array of scope strings
    issued_at      INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    last_used_at   INTEGER,
    expires_at     INTEGER                  -- NULL = no expiry
) STRICT;
CREATE INDEX idx_tokens_user ON tokens(user_id);

-- ── Sessions (playback position per-user-per-book) ─────────────────
CREATE TABLE play_sessions (
    session_id     TEXT PRIMARY KEY,        -- UUID
    user_id        INTEGER NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
    book_id        INTEGER NOT NULL REFERENCES books(book_id) ON DELETE CASCADE,
    position_ms    INTEGER NOT NULL DEFAULT 0,
    duration_ms    INTEGER NOT NULL,
    started_at     INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    updated_at     INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    closed_at      INTEGER,
    device         TEXT,                    -- token nickname at session open
    speed          REAL DEFAULT 1.0
) STRICT;
CREATE INDEX idx_play_sessions_book ON play_sessions(book_id);
CREATE INDEX idx_play_sessions_user_open ON play_sessions(user_id, closed_at);

CREATE TABLE play_progress (
    user_id        INTEGER NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
    book_id        INTEGER NOT NULL REFERENCES books(book_id) ON DELETE CASCADE,
    position_ms    INTEGER NOT NULL DEFAULT 0,
    finished       INTEGER NOT NULL DEFAULT 0,
    updated_at     INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    PRIMARY KEY (user_id, book_id)
) STRICT;

-- bookmarks + play_queue + audiologo_seed_export live in migration
-- 025 (ADR-0046 player-state tables). The legacy user-scoped
-- bookmarks + playlists shapes that previously sat here were
-- replaced before any production data existed.

-- ── AI cache (compressed transcript + dna tags) ────────────────────
-- BLOB content with `compressed=1` is zstd; otherwise plain UTF-8.
CREATE TABLE ai_cache (
    book_id        INTEGER NOT NULL REFERENCES books(book_id) ON DELETE CASCADE,
    cache_type     TEXT NOT NULL,           -- transcript, transcript_full, dna_tags, ...
    content        BLOB,
    compressed     INTEGER NOT NULL DEFAULT 0,
    confidence     REAL,
    model_version  TEXT,
    created_at     INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    PRIMARY KEY (book_id, cache_type)
) STRICT;

-- ── Mass-edit history (undo support) ───────────────────────────────
CREATE TABLE mass_edit_history (
    edit_id        INTEGER PRIMARY KEY AUTOINCREMENT,
    target_kind    TEXT NOT NULL,           -- "book", "book_files", "tags"
    target_id      INTEGER NOT NULL,
    field          TEXT NOT NULL,
    before_value   TEXT,                    -- JSON
    after_value    TEXT,                    -- JSON
    batch_id       TEXT,                    -- UUID linking edits made in one operation
    actor          TEXT,                    -- user id or "system"
    recorded_at    INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    undone_at      INTEGER
) STRICT;
CREATE INDEX idx_mass_edit_batch ON mass_edit_history(batch_id);
CREATE INDEX idx_mass_edit_recorded ON mass_edit_history(recorded_at);

-- ── Schema meta ────────────────────────────────────────────────────
CREATE TABLE meta (
    key TEXT PRIMARY KEY,
    value TEXT
) STRICT;
INSERT INTO meta (key, value) VALUES ('seed_version', '0');
INSERT INTO meta (key, value) VALUES ('created_at', strftime('%s','now'));

-- ── original migration: 002_description_lang.sql ────────────────────────────────────

-- slice 3D.1: language-normalize.
--
-- Adds `books.description_lang` so the UI can render the
-- description with the correct directionality / font even when
-- it differs from `books.language` (a German user with a
-- Japanese book; an English UI displaying a description in
-- the book's original language).
--
-- `description_lang` is stored in the canonical BCP-47
-- primary-subtag form normalize() produces — same shape as
-- `books.language`. NULL = unknown; the UI falls back to
-- LTR Latin-script rendering.

ALTER TABLE books ADD COLUMN description_lang TEXT;

-- ── original migration: 003_llm_extractor_tables.sql ────────────────────────────────

-- slice 3K.2: schema for LLM-driven extractors.
--
-- The four extractors landing in 3K.3-6 each need a place to
-- persist their promoted output. Raw LLM responses (with
-- `model_version`) are cached in the existing `ai_cache` table;
-- this migration adds the promoted views on `books` plus a new
-- `characters` table for the character extractor.
--
-- Re-extraction policy: each extractor stage clears its rows
-- for the affected book and re-writes. The cache_type row in
-- `ai_cache` carries the model_version that produced them;
-- `merge` invalidates a promoted value when the cache row
-- predates the current model version.

-- ── books: spoiler-free summary ────────────────────────────────────
-- LLM-rewritten plot summary safe for browsing. Lives next to
-- `description` rather than replacing it — `description` is the
-- catalog text (Audible / Audnexus / publisher blurb) and may
-- include spoilers; `summary_spoiler_free` is the version the
-- UI surfaces when "hide spoilers" is on (default).
--
-- `summary_spoiler_free_lang` is the language the summary was
-- written in. Same BCP-47 primary-subtag form as `language` and
-- `description_lang`. The extractor honours the library locale
-- (so a German library gets German summaries even for English
-- books); NULL = unknown.
ALTER TABLE books ADD COLUMN summary_spoiler_free TEXT;
ALTER TABLE books ADD COLUMN summary_spoiler_free_lang TEXT;

-- ── books: story arc ───────────────────────────────────────────────
-- JSON-encoded array of {step, label, summary} objects laying
-- out the book's narrative beats. Used by the UI for the
-- "story arc" sidebar (spoiler-aware: each step has its own
-- reveal toggle). Stored as JSON rather than a relational
-- arc_steps table because rows are always read all-or-nothing
-- and there's no cross-book querying on individual beats.
ALTER TABLE books ADD COLUMN story_arc_json TEXT;

-- ── characters ─────────────────────────────────────────────────────
-- Extracted from the transcript by the LLM character pass.
-- One row per canonical character per book.
--
-- `aliases` is a JSON array of alternate names the extractor
-- saw in the text (nicknames, titles, etc.). Identity-resolve
-- in the extractor collapses these into a single canonical
-- name before writing.
--
-- `role` is one of {protagonist, antagonist, supporting,
-- mentioned} — kept as a free-form TEXT rather than a CHECK
-- constraint so future extractor revisions can introduce new
-- categories (e.g. "narrator-character" for first-person
-- audiobooks) without a schema change.
--
-- `description` is a brief, deliberately spoiler-free blurb
-- (one or two sentences) — the LLM is instructed to describe
-- the character without revealing plot twists.
--
-- `lang` carries the language `name` + `description` are
-- written in (BCP-47 primary subtag). Honours library locale
-- just like `summary_spoiler_free_lang`.
--
-- UNIQUE (book_id, name) keeps re-extraction idempotent: the
-- characters stage clears the book's rows then re-inserts.
CREATE TABLE characters (
    character_id   INTEGER PRIMARY KEY AUTOINCREMENT,
    book_id        INTEGER NOT NULL REFERENCES books(book_id) ON DELETE CASCADE,
    name           TEXT NOT NULL,
    aliases        TEXT,
    role           TEXT,
    description    TEXT,
    lang           TEXT,
    created_at     INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    UNIQUE (book_id, name)
) STRICT;
CREATE INDEX idx_characters_book ON characters(book_id);
CREATE INDEX idx_characters_name ON characters(name);

-- ── original migration: 004_ai_cache_locale_extractor_version.sql ───────────────────

-- slice B2: ai_cache schema cleanup.
--
-- Two changes to the cache row, motivated by the course-
-- correction conversation that landed in slice A/B:
--
-- 1. ADD COLUMN `locale TEXT` — every transcript / LLM cache
--    row carries the BCP-47 locale it was produced in. Up to
--    now we embedded it inside the JSON BLOB and decoded the
--    blob just to read the locale for freshness comparisons.
--    A column means freshness becomes a one-row SQL check.
--
-- 2. RENAME COLUMN `model_version` → `extractor_version` — the
--    column tracks the version of whatever extractor wrote
--    the row. Today that's either a Speech engine version
--    (`speech-26.0-v1`) or a Foundation Models version
--    (`fm-26.0-v1`). The new name generalises across both
--    + future non-Apple backends (whisper, llama) without a
--    second rename.
--
-- SQLite supports both forms since 3.25 / 3.35. No data
-- migration needed: pre-existing rows on a fresh dev DB will
-- be wiped on next scan anyway (the DB is not yet user-facing
-- per PROJECT.md).

ALTER TABLE ai_cache ADD COLUMN locale TEXT;
ALTER TABLE ai_cache RENAME COLUMN model_version TO extractor_version;

-- ── original migration: 005_book_field_provenance_check.sql ─────────────────────────

-- Slice C5.3: pin `book_field_provenance.field` vocabulary at
-- the DB layer with a CHECK constraint matching the
-- `ab_core::Field` enum.
--
-- Up through slice C5.2 the typed-vocabulary invariant
-- ("`field` values come from a closed set") was enforced
-- workspace-wide on the Rust side: extractors and consumers go
-- through `ab_core::Field::*`, with `.as_str()` at the SQL bind
-- site. The CHECK adds storage-layer enforcement so a future
-- runtime `sqlx::query()` site (or a future direct-SQL admin tool)
-- can't sneak an off-vocabulary value past the typed Rust
-- surface.
--
-- ── Why a table rebuild ────────────────────────────────────────────
--
-- SQLite has no `ALTER TABLE ... ADD CONSTRAINT`. The canonical
-- pattern (sqlite.org `lang_altertable.html` § 7) is "build a
-- new table with the constraint, copy rows, drop the old,
-- rename the new". The four statements run inside the sqlx
-- migration transaction so the rebuild is atomic.
--
-- ── Vocabulary ────────────────────────────────────────────────────
--
-- Mirrors `Field::as_str()` exactly. New variants land here in
-- the same commit that adds them to the enum (a stray DB write
-- with an unrecognised `field` would fail the CHECK; the test
-- suite catches this for new variants because
-- `book_field_provenance` rows are written end-to-end in
-- `crates/catalog/tests/promote_drift.rs`).
--
-- ── On migration failure ──────────────────────────────────────────
--
-- If a dev or fixture DB has rows whose `field` value is
-- outside the enum, the rebuild's `INSERT INTO ... SELECT`
-- will fail the CHECK and the migration aborts. That's the
-- intended behaviour — loud failure beats silent data drift.
-- A fresh dev DB after `aborg library scan` populates only
-- enum-valid values (every extractor goes through `Field::*`
-- post-C5.1).

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
    confidence     REAL NOT NULL,
    is_winner      INTEGER NOT NULL DEFAULT 0,
    external_id    TEXT,
    recorded_at    INTEGER NOT NULL DEFAULT (strftime('%s','now'))
) STRICT;

INSERT INTO book_field_provenance_new (
    provenance_id, book_id, field, value, source, confidence,
    is_winner, external_id, recorded_at
)
SELECT
    provenance_id, book_id, field, value, source, confidence,
    is_winner, external_id, recorded_at
FROM book_field_provenance;

DROP TABLE book_field_provenance;

ALTER TABLE book_field_provenance_new RENAME TO book_field_provenance;

-- ── original migration: 006_book_series_candidate.sql ───────────────────────────────

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

-- ── original migration: 007_series_summary.sql ──────────────────────────────────────

-- Migration 007: series-level spoiler-free summary (slice 3K.4.1).
--
-- Per ADR-0019 + ADR-pending-for-3K.4.1, the spoiler-free summary
-- extractor produces both per-book (3K.4 → `books.summary_spoiler_free`)
-- and per-series content (this slice). Series-level content is
-- regenerated when the set of books in the series changes (a new
-- `book_series` row is inserted, or a book's individual summary
-- is re-extracted at a newer `extractor_version`).
--
-- ## Schema additions
--
-- Three new columns on `series`:
--
-- - `summary TEXT` — the spoiler-free synopsis for the series as
--   a whole. NULL until the extractor runs.
-- - `summary_lang TEXT` — BCP-47 tag. Set to the predominant
--   `books.language` across the series' books (see ADR-0019's
--   locale rule); fall back to `library_locale` when tied or no
--   books yet contribute.
-- - `summary_extractor_version TEXT` — version stamp. The stage
--   compares this against `LlmTunables.extractor_version` to
--   decide whether to regenerate. NULL = never extracted.
--
-- ## Why columns vs. a separate `series_ai_cache` table
--
-- The existing `ai_cache` PK is `(book_id, cache_type)` — no fit
-- for series-level content. Two options were considered:
--
-- 1. Parallel `series_ai_cache` table with `(series_id, cache_type)`
--    PK. Mirrors `ai_cache` shape; more schema surface.
-- 2. Columns on `series` itself.
--
-- We picked #2 because:
--
-- - Series-level content is small (one summary per series, not
--   per cache_type × series), so the table's columns can hold
--   the whole shape.
-- - Compressed-blob caching is unnecessary; summaries are short.
-- - Re-extraction triggers are simpler — single column
--   comparison, no join.
-- - The number of series in a library is small (10² order) vs.
--   books (10³-10⁴ order), so per-series stage runs cheap.
--
-- ## Migration shape
--
-- All three columns are nullable; `ALTER TABLE ADD COLUMN` is
-- the natural SQLite move. No rebuild needed.

ALTER TABLE series ADD COLUMN summary TEXT;
ALTER TABLE series ADD COLUMN summary_lang TEXT;
ALTER TABLE series ADD COLUMN summary_extractor_version TEXT;

-- ── original migration: 008_character_traits.sql ────────────────────────────────────

-- slice 3K.6: extend `characters` with PoV flag + six trait columns.
--
-- The 3K.6 LLM character extractor (`extract-characters`)
-- produces per-character traits alongside the existing
-- name/aliases/role/description/lang. ADR-0022's "Character
-- trait taxonomy" table is the source of truth for what each
-- column means and when it's filled.
--
-- All six trait columns are nullable: the extractor only fills
-- a column when the transcript gives signal. Contemporary
-- fiction stays NULL on `species`; characters whose occupation
-- isn't named stay NULL on `occupation`; etc. Queries that
-- want "books with non-human characters" use
-- `WHERE species IS NOT NULL`.
--
-- `is_pov` is NOT NULL DEFAULT 0 — the extractor always emits
-- a 0/1 value, never elides it. The flag is structural rather
-- than descriptive: an `is_pov` of NULL would force every UI
-- "show PoV characters" filter to disambiguate
-- "definitely-not-PoV" vs "unknown".
--
-- Vocabulary policy per ADR-0022 § Vocabulary policy: free-
-- form in v1, learned-canonical mapping in a post-library-
-- scan slice. `age` is the one closed-bracket field (child /
-- teen / adult / elderly / immortal), enforced at the
-- complete_structured schema layer rather than as a SQL CHECK
-- constraint — keeps the migration `ALTER TABLE ADD COLUMN`-
-- shaped and lets future bracket revisions ship without a
-- schema change.

ALTER TABLE characters ADD COLUMN is_pov INTEGER NOT NULL DEFAULT 0;
ALTER TABLE characters ADD COLUMN species TEXT;
ALTER TABLE characters ADD COLUMN condition TEXT;
ALTER TABLE characters ADD COLUMN occupation TEXT;
ALTER TABLE characters ADD COLUMN age TEXT;
ALTER TABLE characters ADD COLUMN gender TEXT;
ALTER TABLE characters ADD COLUMN affiliation TEXT;

-- Useful for the "show me books with PoV characters of
-- species X" query class. PoV filtering is the dominant
-- read pattern; the species + condition columns are second-
-- order facets that won't justify their own indices until
-- queries surface. Add then, not now.
CREATE INDEX idx_characters_is_pov ON characters(book_id, is_pov);

-- ── original migration: 009_book_setting.sql ────────────────────────────────────────

-- slice 3K.8: add setting columns to `books`.
--
-- The 3K.8 LLM setting extractor (`extract-setting`) produces
-- a one-paragraph setting summary PLUS a list of `$`-prefixed
-- tags (10 categories per ADR-0022). Paragraph goes to a new
-- `books.setting` column; tags land in `book_tags` with
-- `source='setting_llm'`.
--
-- `setting_lang` carries the BCP-47 tag the paragraph is
-- written in. Same locale rule as `summary_spoiler_free`:
-- output stays in `books.language` regardless of
-- `library_locale` (ADR-0019).
--
-- `setting_extractor_version` mirrors the `summary_extractor_
-- version` pattern from migration 007 but at the book level —
-- not used by the cache-freshness path (that's still on
-- `ai_cache.extractor_version` keyed by `CacheKey::Setting`),
-- but reserved so a future "promotion freshness" check can
-- distinguish "promoted by this extractor_version" from
-- "promoted by an older one." Cheap to add now alongside the
-- value columns.

ALTER TABLE books ADD COLUMN setting TEXT;
ALTER TABLE books ADD COLUMN setting_lang TEXT;
ALTER TABLE books ADD COLUMN setting_extractor_version TEXT;

-- ── original migration: 010_audiologo_per_file_splice.sql ───────────────────────────

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

-- ── original migration: 011_book_field_provenance_stage.sql ─────────────────────────

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

-- ── original migration: 012_audiologo_re_detected_at.sql ────────────────────────────

-- Slice H.1.3: add `book_file_audiologos.re_detected_at`.
--
-- ADR-0024's state machine includes `re_detected` as a valid
-- status alongside `candidate` / `applied` / `rejected`. The
-- table already has `applied_at` + `rejected_at` timestamps
-- recording WHEN each terminal transition happened, but no
-- column was added for `re_detected`. That meant a row flipped
-- from `applied` → `re_detected` (which slice H.1.5's
-- `Stage::reset(...)` for `audiologo-detect` does) lost the
-- "when did this reset happen" information.
--
-- Adding the column is a straight `ALTER TABLE ADD COLUMN`
-- (nullable INTEGER, no DEFAULT) — no rebuild needed because
-- SQLite supports adding a nullable column without one.
--
-- ## Conventions
--
-- Mirrors `applied_at` / `rejected_at`:
--   - NULL when the row has never been in `re_detected` status.
--   - Set to `strftime('%s','now')` (unix-seconds) when the
--     status transitions TO `re_detected`.
--   - Preserved across subsequent transitions back to
--     `candidate`/`applied`/`rejected` — the timestamp records
--     the most recent re-detection event, not the current
--     status.
--
-- Per ADR-0024 § state-machine the valid transitions into
-- `re_detected` are:
--   - `applied` → `re_detected` (the H.1.5 reset path).
--   - `candidate` → `re_detected` (a future "detection was
--     wrong, retry" path).

ALTER TABLE book_file_audiologos
    ADD COLUMN re_detected_at INTEGER;

-- ── original migration: 013_identity_alias_junctions.sql ────────────────────────────

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

-- ── original migration: 014_identity_alias_backfill.sql ─────────────────────────────

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

-- ── original migration: 015_drop_legacy_aliases_columns.sql ─────────────────────────

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

-- ── original migration: 016_identity_disambiguation_pending.sql ─────────────────────

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

-- ── original migration: 017_rename_brand_duration_columns.sql ───────────────────────

-- slice 4B.0: rename + un-deprecate the brand-duration columns
--
-- Original `books.audiologo_intro_ms` / `audiologo_outro_ms` were
-- introduced in migration 001 as head-cut columns. Migration 010
-- (slice 4A) deprecated them when per-file mid-text splice rows
-- in `book_file_audiologos` became the cut-storage shape.
--
-- ADR-0024 Revision 2 (2026-05-13) reverses the deprecation under
-- a rename. The columns hold Audnexus's reported brand-jingle
-- duration — distinct from the actual cut location (which lives
-- in `book_file_audiologos.jingle_start_ms` / `_end_ms`). The
-- brand duration is needed for two things:
--
-- 1. Chapter-mark recomputation when an audiologo is applied —
--    the "original jingle length" baseline.
-- 2. Libation-stripped detection: when brand duration is non-NULL
--    but no fingerprint matches the head-of-file window, the
--    audio has already been cut elsewhere; we set
--    `audiologo_status='stripped'` and shift chapter offsets by
--    `-brand_intro_duration_ms` to compensate.
--
-- The audnexus-chapters stage restores its writeback to these
-- columns (slice 4B.0 code change, same commit as this migration).
--
-- `ALTER TABLE RENAME COLUMN` (SQLite 3.25+) preserves existing
-- values, indexes, and foreign-key references. Both columns are
-- currently NULL across pre-alpha databases (slice 4A stopped
-- writing them), so the rename is a no-op for data.

ALTER TABLE books RENAME COLUMN audiologo_intro_ms TO brand_intro_duration_ms;
ALTER TABLE books RENAME COLUMN audiologo_outro_ms TO brand_outro_duration_ms;

-- ── original migration: 018_book_file_refs.sql ──────────────────────────────────────

-- ADR-0027: source-file refcount for the transcode pipeline.
--
-- Per ADR-0027: transcode-to-m4b runs at Background priority in
-- parallel with AI jobs. Source files are kept alive by reference-
-- counting rather than by a pipeline-pause, so AI consumers reading
-- the source mid-transcode never see a half-written or missing
-- file. The post-transcode-sources cleanup target reaps a source
-- only when (a) a successful m4b transcode output exists AND (b)
-- live_ref_count(source_file_id) == 0.
--
-- Rows are acquired at stage-run start and released at stage-run
-- end (RAII handle on the Rust side). A live row has released_at
-- NULL. The partial index on (file_id) WHERE released_at IS NULL
-- gives the live_ref_count predicate O(log n) lookup without
-- scanning the historical-acquire log.
--
-- Held refs from a panicked stage leak (released_at stays NULL);
-- the cleanup target ignores those files. The future `aborg
-- doctor` (Theme 6 hardening) lists refs older than 1 hour with
-- no live stage as suspect leaks.

CREATE TABLE book_file_refs (
    ref_id          INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id         INTEGER NOT NULL REFERENCES book_files(file_id)
                        ON DELETE CASCADE,
    holder_stage    TEXT NOT NULL,        -- StageId of the holder
    holder_book_id  INTEGER NOT NULL,     -- book this holder ran for
    acquired_at     INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    released_at     INTEGER,              -- NULL while the ref is live
    UNIQUE (file_id, holder_stage, holder_book_id, acquired_at)
) STRICT;

CREATE INDEX idx_book_file_refs_live
    ON book_file_refs(file_id) WHERE released_at IS NULL;

-- ── original migration: 019_book_file_refs_drop_unique.sql ──────────────────────────

-- Drop the redundant UNIQUE on book_file_refs.
--
-- Migration 018 originally declared:
--   UNIQUE (file_id, holder_stage, holder_book_id, acquired_at)
--
-- Two problems with this clause:
--
--  1. `ref_id INTEGER PRIMARY KEY AUTOINCREMENT` already
--     guarantees row uniqueness. The UNIQUE adds no semantic
--     value over the PK.
--
--  2. `acquired_at` defaults to `strftime('%s','now')` — 1-second
--     resolution. Two `acquire()` calls from the same stage on
--     the same file for the same book within a single second
--     trigger SQLITE_CONSTRAINT_UNIQUE. Not common in practice
--     (one stage runs once per book), but it's a hidden failure
--     mode for zero benefit.
--
-- Surfaced by the cross-model code review (MYREVIEW.md § 4.1 +
-- REVIEW.md § 2.5). Rebuild the table without the clause; keep
-- both partial indexes from migration 018.

CREATE TABLE book_file_refs_new (
    ref_id          INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id         INTEGER NOT NULL REFERENCES book_files(file_id)
                        ON DELETE CASCADE,
    holder_stage    TEXT NOT NULL,
    -- Add FK so deleting a book reaps its refs (MYREVIEW.md § 4.2).
    holder_book_id  INTEGER NOT NULL REFERENCES books(book_id)
                        ON DELETE CASCADE,
    acquired_at     INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    released_at     INTEGER
) STRICT;

INSERT INTO book_file_refs_new (
    ref_id, file_id, holder_stage, holder_book_id, acquired_at, released_at
)
SELECT
    ref_id, file_id, holder_stage, holder_book_id, acquired_at, released_at
FROM book_file_refs;

DROP TABLE book_file_refs;
ALTER TABLE book_file_refs_new RENAME TO book_file_refs;

-- Recreate the live-ref partial index from migration 018. The
-- acquired_at index was YAGNI — no production query reads by
-- acquired_at; both live_ref_count() and post-transcode-sources
-- filter by file_id (covered by idx_book_file_refs_live). If
-- `aborg doctor` later adds a staleness check by acquired_at,
-- it can add the index in its own migration.
CREATE INDEX idx_book_file_refs_live
    ON book_file_refs(file_id) WHERE released_at IS NULL;

-- ── original migration: 020_one_winner_per_field.sql ────────────────────────────────

-- Enforce one is_winner=1 row per (book_id, field).
--
-- The consensus stage's contract has always been "pick one
-- winner per field" but nothing in the schema enforced it.
-- A race condition (two consensus runs against the same book,
-- or a bug in winner-flag logic) could leave the table with
-- multiple is_winner=1 rows for the same (book_id, field) —
-- and the future tag-write stage would silently emit
-- inconsistent on-disk tags.
--
-- Surfaced by the cross-model code review (MYREVIEW.md § 4.1
-- + § 2.4). Two phases:
--
--   1. Dedupe any pre-existing offenders. For each duplicate
--      group, keep the row with the highest confidence
--      (tiebreak by latest recorded_at) and unset is_winner
--      on the rest. Their provenance survives at is_winner=0
--      — same audit trail.
--
--   2. Add a partial UNIQUE INDEX. From this point on,
--      INSERT/UPDATE that would create a second winner for
--      the same (book_id, field) fails with
--      SQLITE_CONSTRAINT_UNIQUE — converting a silent
--      consistency bug into an explicit error consensus
--      must handle.

-- ── Phase 1: dedupe ──────────────────────────────────────────
-- "Keep the survivor" = max(confidence) with recorded_at as
-- tiebreak. Demote everyone else.
UPDATE book_field_provenance
   SET is_winner = 0
 WHERE is_winner = 1
   AND provenance_id NOT IN (
       SELECT provenance_id
         FROM (
           SELECT provenance_id,
                  ROW_NUMBER() OVER (
                      PARTITION BY book_id, field
                      ORDER BY confidence DESC, recorded_at DESC, provenance_id DESC
                  ) AS rn
             FROM book_field_provenance
            WHERE is_winner = 1
         ) ranked
        WHERE rn = 1
   );

-- ── Phase 2: enforce going forward ───────────────────────────
CREATE UNIQUE INDEX ux_book_field_winner
    ON book_field_provenance(book_id, field)
    WHERE is_winner = 1;

-- ── original migration: 021_library_roots.sql ───────────────────────────────────────

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

-- ── original migration: 022_tokens_revocation.sql ───────────────────────────────────

-- Migration 022 — tokens.revoked_at column + index.
--
-- Backlog item 4a: per-user token CRUD + auth middleware wiring.
--
-- The `tokens` table from migration 001 has `expires_at` for
-- natural expiry. `revoked_at` is operator-initiated revocation
-- (DELETE /api/v1/tokens/{token_id}). The auth middleware
-- filters by both — a token is valid iff
-- `revoked_at IS NULL AND (expires_at IS NULL OR expires_at > now)`.
--
-- We don't overload `expires_at` for revocation because the two
-- carry different semantics:
--   - `expires_at` is set at issue time and never mutates.
--   - `revoked_at` mutates exactly once (NULL → unix-seconds).
-- Keeping them separate lets `aborg tokens list` show both an
-- expected expiry AND a revocation timestamp on rows that got
-- revoked early.

ALTER TABLE tokens ADD COLUMN revoked_at INTEGER;

-- The existing UNIQUE on `token_hash` (migration 001) already
-- covers the auth middleware's hot lookup path; we filter by
-- `revoked_at IS NULL` in the WHERE clause, not via the index.
-- A partial index here would shave µs at the cost of a second
-- B-tree to maintain on every INSERT/UPDATE — not worth it at
-- the deployment scales we target.

-- ── original migration: 023_audiologo_bookend_silence.sql ───────────────────────────

-- Migration 023 — bookend-silence persistence on book_file_audiologos.
--
-- ADR-0024 Revision 3 (2026-05-14) adds two synthetic-silence
-- parameters (`head_silence_ms` / `tail_silence_ms`) to
-- `ApplyCutParams`. They're only applied to the audio when the
-- corresponding side of the cut does NOT land in natural silence;
-- when natural silence is already there, no synthetic silence is
-- prepended/appended.
--
-- The two flags below persist the detector's report on whether
-- each cut boundary lands in natural silence, so the audio-cut
-- path (future slice 4B.x.2) can short-circuit correctly without
-- re-inspecting the waveform each time it applies a row.
--
-- Defaults:
--   - 0 (false) = "boundary lands in speech / needs synthetic
--     silence". This is the conservative default per ADR-0024 Rev 3
--     "Out-of-scope for 4B" note — until detector logic flips the
--     flags (slice 4B.x.3, post-calibration), every applied row
--     gets synthetic padding.
--   - The detector (slice 4B.x.3) will eventually set the flag to
--     1 (true) when the waveform analysis confirms natural silence
--     at the boundary.
--
-- No backfill: every existing row defaults to 0 / "always pad",
-- which matches the conservative behavior expected when older
-- detector runs didn't analyze silence.

ALTER TABLE book_file_audiologos
    ADD COLUMN head_lands_in_silence INTEGER NOT NULL DEFAULT 0
        CHECK (head_lands_in_silence IN (0, 1));

ALTER TABLE book_file_audiologos
    ADD COLUMN tail_lands_in_silence INTEGER NOT NULL DEFAULT 0
        CHECK (tail_lands_in_silence IN (0, 1));

-- No new indexes — these columns are read alongside the row via
-- `audiologo_row_id` lookup (the audio-cut path fetches one row by
-- PK). Independent filtering on the flags is not a planned
-- workload.

-- ── original migration: 024_books_deleted_at.sql ────────────────────────────────────

-- Migration 024 — soft-delete column on books.
--
-- API.md has always documented `DELETE /books/{id}` as soft-delete
-- by default (with `?force=true` for hard delete). Slice #93
-- shipped the hard-delete half but required `?force=true` since
-- the schema didn't yet support soft-delete. This migration adds
-- the column; the same slice that lands the migration also flips
-- the default behavior.
--
-- ## Semantics
--
-- - `deleted_at IS NULL` (the default) → book is active.
-- - `deleted_at = <unix-secs>` → book is soft-deleted at that
--   timestamp. Hidden from `GET /books` and `GET /books/{id}` by
--   default; not picked up by the pipeline dispatcher; remains
--   in the database (FKs intact) so a future `restore` endpoint
--   can un-mark it.
--
-- ## What stays unchanged
--
-- The 58-ish other SELECT-from-books queries across the workspace
-- (catalog / llm-extractors / transcript / audiologo / scan)
-- are mostly PK-driven (called with a specific `book_id`). Those
-- paths still work on a soft-deleted book; the soft-delete only
-- gates SCHEDULING of new pipeline work via the dispatcher. An
-- in-flight stage on a just-soft-deleted book completes its
-- work — that's the right behavior (don't leave broken state).
--
-- ## Why no separate index
--
-- The dispatcher's queries already filter via the book's
-- per-stage `pipeline_progress` join; adding a partial index on
-- `deleted_at IS NULL` would shave µs at the cost of a second
-- B-tree to maintain on every UPDATE. Defer until production
-- data shows the table-scan is a real cost.

ALTER TABLE books ADD COLUMN deleted_at INTEGER;

-- ── original migration: 025_player_state_tables.sql ─────────────────────────────────

-- ADR-0046: player state — bookmarks, play queue, audiologo seed export.
--
-- Three durable tables. The session-local sleep_timer_state lives in
-- ephemeral.db (migration 005 there). Schema lands now so player
-- features can be implemented incrementally without revisiting the
-- migration set ("no empty features" — POLICIES.md ground rule 7).
--
-- bookmarks   — operator-named markers at a timestamp, multi per book,
--                cross-device LWW on (bookmark_id, last_modified_at).
-- play_queue  — operator-curated "play next" list across books;
--                auto-increment position is the order.
-- audiologo_seed_export — schema-only for v1.0; records exports of the
--                reviewed-confirmed audiologo seed once that ships.

CREATE TABLE bookmarks (
    bookmark_id      INTEGER PRIMARY KEY AUTOINCREMENT,
    book_id          INTEGER NOT NULL REFERENCES books(book_id)
                        ON DELETE CASCADE,
    timestamp_ms     INTEGER NOT NULL,          -- absolute book-time
    label            TEXT,                      -- nullable
    note             TEXT,                      -- nullable
    created_at       INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    created_by_token TEXT NOT NULL,             -- pairing-token name
    last_modified_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
) STRICT;

CREATE INDEX idx_bookmarks_book     ON bookmarks(book_id);
CREATE INDEX idx_bookmarks_modified ON bookmarks(last_modified_at);

CREATE TABLE play_queue (
    queue_position   INTEGER PRIMARY KEY AUTOINCREMENT,
    book_id          INTEGER NOT NULL REFERENCES books(book_id)
                        ON DELETE CASCADE,
    added_at         INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    added_by_token   TEXT NOT NULL
) STRICT;

CREATE INDEX idx_play_queue_book ON play_queue(book_id);

CREATE TABLE audiologo_seed_export (
    export_id        INTEGER PRIMARY KEY AUTOINCREMENT,
    exported_at      INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    exported_by      TEXT NOT NULL,             -- pairing-token name
    audiologo_ids    TEXT NOT NULL,             -- JSON array of ids
    sha256           TEXT NOT NULL              -- artifact hash
) STRICT;

-- ── original migration: 026_reading_state.sql ───────────────────────────────────────

-- ADR-0033: per-book curation + durable playback progress.
--
-- Adds three columns on `books`:
--   * reading_status — operator-set; defaults `want_to_read`. Closed
--     enum mirroring `ab_core::ReadingStatus` (`want_to_read`,
--     `reading`, `finished`, `dnf`). CHECK constraint enforces.
--   * rating          — 1..=5 stars, NULL = unrated.
--   * notes           — free-form text, NULL = no notes.
--
-- Plus a new `media_progress` table for cross-device player
-- position. Lives in `library.db` (canonical, nightly backup) —
-- the previous in-ephemeral.db scheme reset on daemon restart
-- which surprised users mid-listen.
--
-- LWW conflict resolution: `last_synced_at` wins on conflict
-- between two devices reporting in. `last_synced_from` records
-- which pairing token last wrote so the UI can surface "you
-- listened to this on Mac".

ALTER TABLE books ADD COLUMN reading_status TEXT NOT NULL DEFAULT 'want_to_read'
    CHECK (reading_status IN ('want_to_read', 'reading', 'finished', 'dnf'));

ALTER TABLE books ADD COLUMN rating INTEGER
    CHECK (rating IS NULL OR (rating >= 1 AND rating <= 5));

ALTER TABLE books ADD COLUMN notes TEXT;

CREATE INDEX idx_books_reading_status ON books(reading_status);

CREATE TABLE media_progress (
    book_id           INTEGER PRIMARY KEY
                          REFERENCES books(book_id) ON DELETE CASCADE,
    current_time_ms   INTEGER NOT NULL DEFAULT 0,
    is_finished       INTEGER NOT NULL DEFAULT 0
                          CHECK (is_finished IN (0, 1)),
    last_listened_at  INTEGER,            -- unix seconds
    last_synced_from  TEXT,               -- pairing-token name
    last_synced_at    INTEGER              -- unix seconds
) STRICT;

CREATE INDEX idx_media_progress_last_listened
    ON media_progress(last_listened_at);

-- ── original migration: 027_saved_queries.sql ───────────────────────────────────────

-- ADR-0034: saved queries — unified shape powering series views,
-- smart filters, dashboard tiles, recently-added, similar-books.
--
-- One table, one executor. `kind` discriminates the presentation
-- layer (UI renders dashboard_tile differently from smart_filter,
-- but they share the same `query_json` shape). `query_json`
-- carries a serialised `ab_query::QueryFilter` (ADR-0031);
-- `sort_json` optionally overrides the filter's own sort.
--
-- `owner_kind = 'system'` marks builtin rows that ship with the
-- DB and aren't user-editable; everything else is user-owned and
-- mutable via the CRUD endpoints.

CREATE TABLE saved_queries (
    query_id        INTEGER PRIMARY KEY AUTOINCREMENT,
    kind            TEXT NOT NULL CHECK (kind IN (
                        'series_view',
                        'smart_filter',
                        'dashboard_tile',
                        'recently_added',
                        'similar_books',
                        'system'
                    )),
    name            TEXT NOT NULL,
    description     TEXT,
    query_json      TEXT NOT NULL,
    sort_json       TEXT,
    pin_position    INTEGER,
    owner_kind      TEXT NOT NULL DEFAULT 'user'
                        CHECK (owner_kind IN ('system', 'user')),
    created_at      INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    updated_at      INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    UNIQUE (kind, name)
) STRICT;

CREATE INDEX idx_saved_queries_kind ON saved_queries(kind);
CREATE INDEX idx_saved_queries_pin
    ON saved_queries(pin_position)
    WHERE pin_position IS NOT NULL;

-- ── original migration: 028_operation_journal.sql ───────────────────────────────────

-- ADR-0039: operation journal — undo + crash recovery + diff.
--
-- Every mutating operation writes its journal row BEFORE the
-- file-system / DB mutation:
--
--   1. Build `pre_state_json` (current state of the target).
--   2. INSERT into `operation_journal` with `progress='pending'`.
--   3. Perform the mutation.
--   4. UPDATE to `progress='done'` + write `post_state_json`.
--
-- Mid-batch crash: pending rows survive. On daemon restart,
-- `ab_journal::recover_pending_batches()` re-attempts or marks
-- the row failed if the target has drifted since.
--
-- 90-day retention via `StaleOperationJournalCleanupTarget`
-- (ADR-0025). The audit-trail `mass_edit_history` stays forever;
-- this table is the reversible journal.

CREATE TABLE operation_journal (
    op_id           INTEGER PRIMARY KEY AUTOINCREMENT,
    op_kind         TEXT NOT NULL,             -- 'tag-write-final', 'batch-edit', ...
    target_kind     TEXT NOT NULL,             -- 'book', 'file', 'companion', ...
    target_id       INTEGER NOT NULL,
    pre_state_json  TEXT NOT NULL,
    post_state_json TEXT,                      -- NULL while pending or for dry-run
    created_at      INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    reversible      INTEGER NOT NULL DEFAULT 1,
    batch_id        TEXT,                      -- ULID or similar; NULL for single ops
    progress        TEXT NOT NULL DEFAULT 'pending'
                        CHECK (progress IN ('pending', 'done', 'failed', 'reversed')),
    failed_reason   TEXT
) STRICT;

CREATE INDEX idx_op_journal_batch ON operation_journal(batch_id);
CREATE INDEX idx_op_journal_target ON operation_journal(target_kind, target_id);
CREATE INDEX idx_op_journal_created ON operation_journal(created_at);
CREATE INDEX idx_op_journal_pending ON operation_journal(progress)
    WHERE progress = 'pending';

-- ── original migration: 029_zip_archive_extracts.sql ────────────────────────────────

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

-- ── original migration: 030_books_fts5.sql ──────────────────────────────────────────

-- ADR-0036: FTS5 mirror of books title + subtitle + description.
--
-- External-content virtual table (`content='books'`,
-- `content_rowid='book_id'`) so the FTS index stores only the
-- tokenised payload + rowid; the canonical text lives in the
-- `books` row. Three triggers keep the mirror in sync on every
-- INSERT / UPDATE / DELETE.
--
-- Tokenizer: `unicode61 remove_diacritics 2` — case-insensitive
-- Latin-script normalisation. CJK / Arabic scripts get coarse
-- segmentation; a future ICU upgrade lands its own slice.
--
-- Trigram fuzzy adjacent table (operator-name typo recovery) is
-- a follow-up slice; this migration ships the exact-FTS path.

CREATE VIRTUAL TABLE books_fts USING fts5(
    title,
    subtitle,
    description,
    content='books',
    content_rowid='book_id',
    tokenize="unicode61 remove_diacritics 2"
);

-- Trigger: new row → INSERT into FTS using book_id as rowid.
CREATE TRIGGER books_fts_insert AFTER INSERT ON books BEGIN
    INSERT INTO books_fts (rowid, title, subtitle, description)
    VALUES (new.book_id, new.title, new.subtitle, new.description);
END;

-- Trigger: row deleted → 'delete' command (FTS5 idiom for
-- external-content).
CREATE TRIGGER books_fts_delete AFTER DELETE ON books BEGIN
    INSERT INTO books_fts (books_fts, rowid, title, subtitle, description)
    VALUES('delete', old.book_id, old.title, old.subtitle, old.description);
END;

-- Trigger: title / subtitle / description changed → delete-then-
-- insert. Other column changes don't touch FTS.
CREATE TRIGGER books_fts_update AFTER UPDATE OF title, subtitle, description ON books BEGIN
    INSERT INTO books_fts (books_fts, rowid, title, subtitle, description)
    VALUES('delete', old.book_id, old.title, old.subtitle, old.description);
    INSERT INTO books_fts (rowid, title, subtitle, description)
    VALUES (new.book_id, new.title, new.subtitle, new.description);
END;

-- Backfill: existing rows enter the index.
INSERT INTO books_fts (rowid, title, subtitle, description)
SELECT book_id, title, subtitle, description FROM books;

-- ── original migration: 031_books_loudness.sql ──────────────────────────────────────

-- ADR-0041: loudness measurement foundation.
--
-- Adds two REAL columns on `books`:
--   * lufs_integrated — EBU R-128 integrated loudness in LUFS.
--     NULL when the loudness-measurement stage hasn't run.
--     Typical audiobook range: -25 to -16 LUFS. Audible
--     publishes -18 ±2 LU as their target band; podcast spec
--     commonly targets -16 LUFS.
--   * lufs_truepeak   — true-peak (dBTP) under EBU R-128 with
--     4x oversampling. NULL when unmeasured. Clipping ceiling
--     for the optional gain stage on transcode (-1.0 dBTP is
--     the common ceiling that survives lossy re-encoding).
--
-- Both columns are populated by a future `loudness-measure`
-- stage that calls AVFoundation's loudness analyser via Swift
-- FFI on `ab-audio`. Schema lands now so:
--   1. Saved-query filters can reference them (loud-soft sort,
--      "louder than -20 LUFS" filters in the dashboard).
--   2. The optional ReplayGain-style gain on transcode can read
--      the per-book target without waiting on the FFI bridge
--      landing in the same slice.
--
-- Neither column has a default — NULL is the "not yet measured"
-- state. Indexes deferred until query patterns settle; both
-- columns are cheap to scan across 20k–100k rows.

ALTER TABLE books ADD COLUMN lufs_integrated REAL;
ALTER TABLE books ADD COLUMN lufs_truepeak REAL;

-- ── original migration: 032_tag_hierarchy_and_content_warnings.sql ──────────────────

-- ADR-0042: tag hierarchy + content warnings extension to the
-- DNA-tag LLM extractor.
--
-- Two tables:
--
--   * tag_hierarchy — global parent/child relationships emitted
--     by the LLM ("High Fantasy is a kind of Fantasy"). Not
--     per-book; multiple books contributing the same pair is a
--     PK no-op. Cycle prevention is enforced at the application
--     layer (the executor walks descendants before inserting).
--
--   * book_content_warnings — per-book content-warning entries
--     drawn from a fixed canonical vocabulary (violence,
--     sexual_assault, gore, addiction, suicide, etc.). The DNA
--     prompt enumerates the vocabulary; the executor rejects
--     freeform labels. Per-locale UI translation is handled at
--     render time on the canonical English label.
--
-- Both tables are STRICT so the column types are enforced at the
-- engine layer rather than coerced silently. `tag_hierarchy` is
-- WITHOUT ROWID — small table, two-column PK, the rowid is pure
-- overhead. `book_content_warnings` keeps the rowid because the
-- ON DELETE CASCADE FK + the `extracted_at` ordering benefit from
-- having one.

CREATE TABLE tag_hierarchy (
    parent_tag      TEXT NOT NULL,
    child_tag       TEXT NOT NULL,
    PRIMARY KEY (parent_tag, child_tag)
) STRICT, WITHOUT ROWID;

CREATE INDEX idx_tag_hierarchy_child
    ON tag_hierarchy(child_tag);

CREATE TABLE book_content_warnings (
    book_id         INTEGER NOT NULL
                        REFERENCES books(book_id) ON DELETE CASCADE,
    label           TEXT NOT NULL,
    severity        TEXT NOT NULL
                        CHECK (severity IN ('mild', 'moderate', 'intense', 'graphic')),
    extracted_at    INTEGER NOT NULL,
    PRIMARY KEY (book_id, label)
) STRICT;

CREATE INDEX idx_book_content_warnings_label
    ON book_content_warnings(label);

-- ── original migration: 033_book_companions.sql ─────────────────────────────────────

-- ADR-0043: companion files — schema foundation (slice C.1).
--
-- Two tables + one denormalised column on book_files:
--
--   * book_companions — one row per sidecar file (PDF / EPUB /
--     MOBI / CB* / etc.). `book_id` is the paired audiobook;
--     NULL marks an unpaired / orphan companion (true orphans
--     are never auto-deleted).
--
--   * companion_nearby_books — junction-hint table. When auto-
--     pair geometry is ambiguous (companion in an ancestor dir
--     with several audiobooks in its subtree) we record one row
--     per candidate audiobook so the ❓ indicator can list every
--     possibly-related book.
--
--   * book_files.companion_paired_count — denormalised hot-read
--     column maintained by the C.2 scanner / pair-toggle paths.
--     List views surface "this book has N companions" without a
--     JOIN.
--
-- Format / parse_tier enums match the ADR's vocabulary verbatim.
-- TXT / MD are deliberately absent — the scanner skips them
-- (README / LICENSE / notes are noise, not book content).

CREATE TABLE book_companions (
    companion_id    INTEGER PRIMARY KEY AUTOINCREMENT,
    -- NULL = unpaired / orphan. Persists forever; never auto-deleted.
    book_id         INTEGER
                        REFERENCES books(book_id) ON DELETE CASCADE,
    path            TEXT NOT NULL UNIQUE,
    format          TEXT NOT NULL
                        CHECK (format IN (
                            'epub', 'pdf', 'mobi', 'azw3', 'kfx',
                            'fb2', 'lit', 'djvu', 'lrf',
                            'cbz', 'cbr', 'cb7', 'cbt',
                            'unknown'
                        )),
    parse_tier      TEXT NOT NULL
                        CHECK (parse_tier IN (
                            'text_extractable', 'document',
                            'ebook_opaque', 'comic', 'unknown'
                        )),
    content_hash    TEXT NOT NULL,    -- BLAKE3 hex
    bytes           INTEGER NOT NULL,
    discovered_at   INTEGER NOT NULL, -- unix seconds
    parsed_at       INTEGER           -- NULL until C4 runs
) STRICT;

CREATE INDEX idx_book_companions_book
    ON book_companions(book_id);
CREATE INDEX idx_book_companions_format
    ON book_companions(format);
CREATE INDEX idx_book_companions_parse_tier
    ON book_companions(parse_tier);

CREATE TABLE companion_nearby_books (
    companion_id    INTEGER NOT NULL
                        REFERENCES book_companions ON DELETE CASCADE,
    book_id         INTEGER NOT NULL
                        REFERENCES books ON DELETE CASCADE,
    discovered_at   INTEGER NOT NULL,
    PRIMARY KEY (companion_id, book_id)
) STRICT, WITHOUT ROWID;

CREATE INDEX idx_companion_nearby_book
    ON companion_nearby_books(book_id);

-- Denormalised hot-read on book_files for the list view's
-- "this book has N companions" column. Maintained by the C.2
-- pair / unpair / scan paths (the trigger-vs-maintained-in-code
-- decision is in the C.2 slice).
ALTER TABLE book_files
    ADD COLUMN companion_paired_count INTEGER NOT NULL DEFAULT 0;

-- ── original migration: 034_transcript_corrected.sql ────────────────────────────────

-- ADR-0043 § C.5: `transcript-correct-via-epub` stage schema prep.
--
-- The C.5 stage replaces Levenshtein-close proper-noun
-- transcriptions in `transcript_full` with their canonical
-- spellings from a paired EPUB companion's name dictionary
-- (extracted by C.4 `epub-name-dict-extract`). The corrected
-- text persists on `books`:
--
--   * NULL — no EPUB companion is paired, or the book is
--     `abridged`, or its `language` doesn't match the EPUB's
--     `dc:language`. Downstream extractors (3K.x) fall back to
--     `transcript_full` from `ai_cache`.
--
--   * Non-NULL — C.5 wrote a corrected variant. Downstream
--     extractors prefer this; the original `transcript_full`
--     in `ai_cache` is preserved for the audiologo Tier-4
--     fingerprint path which still needs the raw transcribed
--     text.
--
-- C.5 stage `reset()` clears this column for one book.
-- Re-running the stage (e.g. on a new EPUB companion landing)
-- overwrites in place.

ALTER TABLE books ADD COLUMN transcript_corrected TEXT;

-- ── original migration: 035_rename_file_hash_to_content_hash.sql ────────────────────

-- Rename `book_files.file_hash` → `book_files.content_hash` for naming
-- consistency with `book_companions.content_hash`. The hash is BLAKE3 over
-- (size + mtime + first 4 KiB) per the existing scan/lib.rs contract; the
-- semantic is unchanged — only the column name moves.
--
-- Part of the schema-as-if-planned-from-day-one slice (2026-05-15
-- retrospective, item #4). The pair of column names was a historical accident
-- of two crates landing months apart; harmonising now avoids new code
-- continuing the drift.
--
-- SQLite 3.25+ supports `ALTER TABLE … RENAME COLUMN`. Both the
-- libsqlite3-sys bundled with sqlx 0.8 and the Homebrew sqlite used by
-- scripts/sqlx-prepare.sh are well past that version.

ALTER TABLE book_files RENAME COLUMN file_hash TO content_hash;

-- ── original migration: 036_provenance_metadata.sql ─────────────────────────────────

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

-- ── original migration: 037_ai_cache_check_cache_type.sql ───────────────────────────

-- Add a SQL-level CHECK constraint on `ai_cache.cache_type` matching the
-- closed `CacheKey` enum set in `crates/core/src/cache.rs`.
--
-- Part of the schema-as-if-planned-from-day-one slice (2026-05-15
-- retrospective, item #3). The `CacheKey` enum enforces the closed set
-- only at the Rust layer; bad rows can land via raw SQL. A CHECK
-- constraint catches that at the database boundary — same posture as the
-- explicit `kind`/`status`/`method` CHECKs on `book_file_audiologos`.
--
-- SQLite doesn't support adding a CHECK constraint via ALTER TABLE; the
-- table is rebuilt:
--   1. Create a parallel `ai_cache_new` with the CHECK in place.
--   2. Copy all rows verbatim.
--   3. Drop the old table.
--   4. Rename the new table.
--
-- The vocabulary string set must stay in sync with `CacheKey::as_str()`.
-- Adding a new variant in Rust requires extending this CHECK (caught at
-- prep time when the schema-parity test runs).

CREATE TABLE ai_cache_new (
    book_id        INTEGER NOT NULL REFERENCES books(book_id) ON DELETE CASCADE,
    cache_type     TEXT NOT NULL CHECK (cache_type IN (
        'transcript_head',
        'transcript_tail',
        'transcript_samples',
        'transcript_full',
        'dna_tags',
        'summary_spoiler_free',
        'story_arc',
        'characters',
        'setting',
        'epub_name_dict'
    )),
    content        BLOB,
    compressed     INTEGER NOT NULL DEFAULT 0,
    confidence     REAL,
    extractor_version  TEXT,
    created_at     INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    locale         TEXT,
    PRIMARY KEY (book_id, cache_type)
) STRICT;

INSERT INTO ai_cache_new (
    book_id, cache_type, content, compressed, confidence,
    extractor_version, created_at, locale
)
SELECT book_id, cache_type, content, compressed, confidence,
       extractor_version, created_at, locale
FROM ai_cache;

DROP TABLE ai_cache;

ALTER TABLE ai_cache_new RENAME TO ai_cache;

-- ── original migration: 038_rename_stages_in_provenance.sql ─────────────────────────

-- Stage naming harmonisation (2026-05-15 retrospective, item #8).
--
-- Companion migration to `ephemeral/007_rename_stages.sql`. Rewrites
-- `book_field_provenance.stage` values so the
-- `Stage::reset(book_id)` flow (slice H.1.5) can still find rows by
-- the new stage names.
--
-- `consensus` and `fingerprint` both wrote `book_field_provenance`
-- rows in older pipeline runs. The provenance.stage column was added
-- in migration 011 (slice H.1.2); rows from before that migration
-- have NULL stage and aren't affected.

UPDATE book_field_provenance
   SET stage = 'promote-consensus'
 WHERE stage = 'consensus';

UPDATE book_field_provenance
   SET stage = 'fingerprint-book'
 WHERE stage = 'fingerprint';

