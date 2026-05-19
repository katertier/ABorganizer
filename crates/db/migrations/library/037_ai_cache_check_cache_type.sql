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
        'transcript_fm_polished',
        'transcript_chapter_marks',
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
