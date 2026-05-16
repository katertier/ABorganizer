-- ADR-0036 follow-up: FTS5 mirrors for authors / narrators / series.
--
-- Pairs with migration 030 (books_fts). Each entity gets its own
-- external-content virtual table — index stores tokenized
-- payload + rowid; canonical text lives on the parent table.
-- Triggers keep the mirror in sync on INSERT / UPDATE / DELETE.
--
-- Tokenizer: `unicode61 remove_diacritics 2` — case-insensitive
-- Latin-script normalisation, same as books_fts.
--
-- Indexed columns:
--   * authors      — `name`
--   * narrators    — `name`
--   * series       — `name`
--
-- We deliberately do NOT index `bio` (authors / narrators) or
-- `franchise_prefix` (series) yet. Bio is enrichment-late
-- (often NULL); franchise_prefix is a sort affix, not a
-- search target. Adding columns later is non-breaking — drop
-- + recreate the virtual table or use `INSERT INTO <fts>(<fts>,
-- rank) VALUES('rebuild')`.
--
-- Backfill: existing rows enter the index at migration time.

-- ── Authors ──────────────────────────────────────────────────
CREATE VIRTUAL TABLE authors_fts USING fts5(
    name,
    content='authors',
    content_rowid='author_id',
    tokenize="unicode61 remove_diacritics 2"
);

CREATE TRIGGER authors_fts_insert AFTER INSERT ON authors BEGIN
    INSERT INTO authors_fts (rowid, name) VALUES (new.author_id, new.name);
END;

CREATE TRIGGER authors_fts_delete AFTER DELETE ON authors BEGIN
    INSERT INTO authors_fts (authors_fts, rowid, name)
    VALUES('delete', old.author_id, old.name);
END;

CREATE TRIGGER authors_fts_update AFTER UPDATE OF name ON authors BEGIN
    INSERT INTO authors_fts (authors_fts, rowid, name)
    VALUES('delete', old.author_id, old.name);
    INSERT INTO authors_fts (rowid, name) VALUES (new.author_id, new.name);
END;

INSERT INTO authors_fts (rowid, name)
SELECT author_id, name FROM authors;

-- ── Narrators ────────────────────────────────────────────────
CREATE VIRTUAL TABLE narrators_fts USING fts5(
    name,
    content='narrators',
    content_rowid='narrator_id',
    tokenize="unicode61 remove_diacritics 2"
);

CREATE TRIGGER narrators_fts_insert AFTER INSERT ON narrators BEGIN
    INSERT INTO narrators_fts (rowid, name) VALUES (new.narrator_id, new.name);
END;

CREATE TRIGGER narrators_fts_delete AFTER DELETE ON narrators BEGIN
    INSERT INTO narrators_fts (narrators_fts, rowid, name)
    VALUES('delete', old.narrator_id, old.name);
END;

CREATE TRIGGER narrators_fts_update AFTER UPDATE OF name ON narrators BEGIN
    INSERT INTO narrators_fts (narrators_fts, rowid, name)
    VALUES('delete', old.narrator_id, old.name);
    INSERT INTO narrators_fts (rowid, name) VALUES (new.narrator_id, new.name);
END;

INSERT INTO narrators_fts (rowid, name)
SELECT narrator_id, name FROM narrators;

-- ── Series ───────────────────────────────────────────────────
CREATE VIRTUAL TABLE series_fts USING fts5(
    name,
    content='series',
    content_rowid='series_id',
    tokenize="unicode61 remove_diacritics 2"
);

CREATE TRIGGER series_fts_insert AFTER INSERT ON series BEGIN
    INSERT INTO series_fts (rowid, name) VALUES (new.series_id, new.name);
END;

CREATE TRIGGER series_fts_delete AFTER DELETE ON series BEGIN
    INSERT INTO series_fts (series_fts, rowid, name)
    VALUES('delete', old.series_id, old.name);
END;

CREATE TRIGGER series_fts_update AFTER UPDATE OF name ON series BEGIN
    INSERT INTO series_fts (series_fts, rowid, name)
    VALUES('delete', old.series_id, old.name);
    INSERT INTO series_fts (rowid, name) VALUES (new.series_id, new.name);
END;

INSERT INTO series_fts (rowid, name)
SELECT series_id, name FROM series;
