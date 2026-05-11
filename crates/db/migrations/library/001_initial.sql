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
    -- UNIQUE includes `source` so the same book can carry chapter
    -- lists from multiple sources concurrently. A future
    -- "chapter-pick-winner" step decides which source the player
    -- surfaces. Indexing on `(book_id, source)` keeps the
    -- per-source clear-then-add path cheap.
    UNIQUE (book_id, idx, source)
) STRICT;
CREATE INDEX idx_chapters_book_source ON chapters(book_id, source);

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

CREATE TABLE bookmarks (
    bookmark_id    INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id        INTEGER NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
    book_id        INTEGER NOT NULL REFERENCES books(book_id) ON DELETE CASCADE,
    position_ms    INTEGER NOT NULL,
    title          TEXT,
    note           TEXT,
    created_at     INTEGER NOT NULL DEFAULT (strftime('%s','now'))
) STRICT;
CREATE INDEX idx_bookmarks_user_book ON bookmarks(user_id, book_id);

-- ── Playlists ──────────────────────────────────────────────────────
CREATE TABLE playlists (
    playlist_id    INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id        INTEGER NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
    name           TEXT NOT NULL,
    description    TEXT,
    created_at     INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    updated_at     INTEGER NOT NULL DEFAULT (strftime('%s','now'))
) STRICT;

CREATE TABLE playlist_items (
    playlist_id    INTEGER NOT NULL REFERENCES playlists(playlist_id) ON DELETE CASCADE,
    book_id        INTEGER NOT NULL REFERENCES books(book_id) ON DELETE CASCADE,
    position       INTEGER NOT NULL,
    added_at       INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    PRIMARY KEY (playlist_id, book_id)
) STRICT;

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
