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
