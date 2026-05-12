-- 3A.4.1: idle-priority Speech model installer state.
--
-- Two tables that together drive the daemon's idle install loop:
--
--   1. `pending_speech_installs` — one row per locale that some
--      book wanted but couldn't get because the on-device model
--      isn't installed (`BridgeError::ModelNotInstalled`). The
--      daemon's idle task wakes periodically, picks the oldest
--      pending row, calls `install_speech_model`, and either
--      marks it `installed` (then re-queues blocked books) or
--      bumps `last_attempted_at` and leaves it pending for the
--      next wake.
--
--   2. `book_locale_blocks` — (book_id, locale) pairs marking
--      which books are waiting on which locale. When the install
--      succeeds, the idle task reads matching rows and resubmits
--      each book's transcribe stage at Background priority, then
--      deletes the unblock rows.
--
-- Both tables are ephemeral: wiping the DB just means books with
-- `ModelNotInstalled` outcomes re-block on next scan, which is
-- the correct behaviour for "lost state."

CREATE TABLE pending_speech_installs (
    -- BCP-47 / NLLanguage raw value. Locale is the install
    -- granularity; one row covers every book that needs it.
    locale            TEXT NOT NULL PRIMARY KEY,
    -- 'pending' = waiting for the next idle wake.
    -- 'installing' = task is running install_speech_model right
    --   now (de-dup guard, in case the task wake overlaps).
    -- 'installed' = installed; this row is kept for a short
    --   window so the unblock pass can find blocked books, then
    --   garbage-collected.
    -- 'failed' = terminal error from Apple Intelligence
    --   (FrameworkUnavailable, LocaleUnsupported). Don't retry;
    --   the books in book_locale_blocks for this locale stay
    --   blocked until manual intervention.
    status            TEXT NOT NULL DEFAULT 'pending'
                      CHECK (status IN ('pending', 'installing', 'installed', 'failed')),
    queued_at         INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    last_attempted_at INTEGER,
    last_error        TEXT
) STRICT;
CREATE INDEX idx_pending_speech_installs_status
    ON pending_speech_installs(status, queued_at);

CREATE TABLE book_locale_blocks (
    -- book_id is shaped like a foreign key but the ephemeral DB
    -- has no FK to the library DB (different file). The library-
    -- side cascade is the source of truth; orphan rows here just
    -- get harmlessly skipped on the next unblock pass.
    book_id      INTEGER NOT NULL,
    locale       TEXT NOT NULL,
    queued_at    INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    PRIMARY KEY (book_id, locale)
) STRICT;
CREATE INDEX idx_book_locale_blocks_locale ON book_locale_blocks(locale);
