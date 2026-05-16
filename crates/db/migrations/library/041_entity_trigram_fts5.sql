-- ADR-0036 § typo recovery: trigram FTS5 mirrors.
--
-- Pairs with migration 030 (books_fts) + 040 (authors_fts /
-- narrators_fts / series_fts). Those use `unicode61` tokenizer
-- and excel at exact + prefix match ("mistbo*" → "Mistborn").
-- They miss internal-character typos: "Mistbron" doesn't match
-- "Mistborn" because the prefix "mistbron" doesn't align with
-- any token boundary.
--
-- The `trigram` tokenizer (SQLite >= 3.34) decomposes input into
-- overlapping 3-character sequences before indexing. A search
-- query gets the same treatment; rows whose trigram set overlaps
-- score well via bm25. "Mistborn" → ("mis", "ist", "stb", "tbo",
-- "bor", "orn") shares 5 of 6 trigrams with "Mistbron" — high
-- score even though the literal prefix doesn't match.
--
-- These tables are search-only fallbacks. The unicode61 path
-- in migrations 030 + 040 stays the primary surface — better
-- precision when the operator typed the name exactly. The
-- trigram path activates when unicode61 returns zero hits OR
-- (future) the operator explicitly opts into fuzzy mode.
--
-- Each table:
--   * Indexes the same column(s) as its unicode61 cousin.
--   * Triggers maintain it on INSERT / UPDATE / DELETE.
--   * Backfilled at migration time.

-- ── Books (title + subtitle + description) ───────────────────
CREATE VIRTUAL TABLE books_trigram USING fts5(
    title,
    subtitle,
    description,
    content='books',
    content_rowid='book_id',
    tokenize='trigram'
);

CREATE TRIGGER books_trigram_insert AFTER INSERT ON books BEGIN
    INSERT INTO books_trigram (rowid, title, subtitle, description)
    VALUES (new.book_id, new.title, new.subtitle, new.description);
END;

CREATE TRIGGER books_trigram_delete AFTER DELETE ON books BEGIN
    INSERT INTO books_trigram (books_trigram, rowid, title, subtitle, description)
    VALUES('delete', old.book_id, old.title, old.subtitle, old.description);
END;

CREATE TRIGGER books_trigram_update AFTER UPDATE OF title, subtitle, description ON books BEGIN
    INSERT INTO books_trigram (books_trigram, rowid, title, subtitle, description)
    VALUES('delete', old.book_id, old.title, old.subtitle, old.description);
    INSERT INTO books_trigram (rowid, title, subtitle, description)
    VALUES (new.book_id, new.title, new.subtitle, new.description);
END;

INSERT INTO books_trigram (rowid, title, subtitle, description)
SELECT book_id, title, subtitle, description FROM books;

-- ── Authors (name) ───────────────────────────────────────────
CREATE VIRTUAL TABLE authors_trigram USING fts5(
    name,
    content='authors',
    content_rowid='author_id',
    tokenize='trigram'
);

CREATE TRIGGER authors_trigram_insert AFTER INSERT ON authors BEGIN
    INSERT INTO authors_trigram (rowid, name) VALUES (new.author_id, new.name);
END;

CREATE TRIGGER authors_trigram_delete AFTER DELETE ON authors BEGIN
    INSERT INTO authors_trigram (authors_trigram, rowid, name)
    VALUES('delete', old.author_id, old.name);
END;

CREATE TRIGGER authors_trigram_update AFTER UPDATE OF name ON authors BEGIN
    INSERT INTO authors_trigram (authors_trigram, rowid, name)
    VALUES('delete', old.author_id, old.name);
    INSERT INTO authors_trigram (rowid, name) VALUES (new.author_id, new.name);
END;

INSERT INTO authors_trigram (rowid, name)
SELECT author_id, name FROM authors;

-- ── Narrators (name) ─────────────────────────────────────────
CREATE VIRTUAL TABLE narrators_trigram USING fts5(
    name,
    content='narrators',
    content_rowid='narrator_id',
    tokenize='trigram'
);

CREATE TRIGGER narrators_trigram_insert AFTER INSERT ON narrators BEGIN
    INSERT INTO narrators_trigram (rowid, name) VALUES (new.narrator_id, new.name);
END;

CREATE TRIGGER narrators_trigram_delete AFTER DELETE ON narrators BEGIN
    INSERT INTO narrators_trigram (narrators_trigram, rowid, name)
    VALUES('delete', old.narrator_id, old.name);
END;

CREATE TRIGGER narrators_trigram_update AFTER UPDATE OF name ON narrators BEGIN
    INSERT INTO narrators_trigram (narrators_trigram, rowid, name)
    VALUES('delete', old.narrator_id, old.name);
    INSERT INTO narrators_trigram (rowid, name) VALUES (new.narrator_id, new.name);
END;

INSERT INTO narrators_trigram (rowid, name)
SELECT narrator_id, name FROM narrators;

-- ── Series (name) ────────────────────────────────────────────
CREATE VIRTUAL TABLE series_trigram USING fts5(
    name,
    content='series',
    content_rowid='series_id',
    tokenize='trigram'
);

CREATE TRIGGER series_trigram_insert AFTER INSERT ON series BEGIN
    INSERT INTO series_trigram (rowid, name) VALUES (new.series_id, new.name);
END;

CREATE TRIGGER series_trigram_delete AFTER DELETE ON series BEGIN
    INSERT INTO series_trigram (series_trigram, rowid, name)
    VALUES('delete', old.series_id, old.name);
END;

CREATE TRIGGER series_trigram_update AFTER UPDATE OF name ON series BEGIN
    INSERT INTO series_trigram (series_trigram, rowid, name)
    VALUES('delete', old.series_id, old.name);
    INSERT INTO series_trigram (rowid, name) VALUES (new.series_id, new.name);
END;

INSERT INTO series_trigram (rowid, name)
SELECT series_id, name FROM series;
