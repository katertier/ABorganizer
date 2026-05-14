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
