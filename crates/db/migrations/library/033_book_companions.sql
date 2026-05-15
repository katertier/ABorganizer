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
