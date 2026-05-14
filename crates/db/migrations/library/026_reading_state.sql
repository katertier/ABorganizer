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
