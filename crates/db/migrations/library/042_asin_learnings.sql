-- ASIN auto-learn capture surface.
--
-- When the operator manually sets an ASIN on a book (via
-- PATCH /api/v1/books/{id} or any future tool), we capture the
-- (title, author, asin) mapping here. The audible-search stage
-- consults this table on next run: if the catalogue lookup
-- input matches a learned row, the resolver can short-circuit
-- the Audible API call entirely (or use the row as the top-
-- ranked candidate).
--
-- The consume side (audible-search hint) lands in a follow-up
-- slice; this migration is capture-only.
--
-- ── Schema design notes ──────────────────────────────────────
-- title_norm + author_norm are case-folded + whitespace-collapsed
-- variants of the raw fields. Normalising at insert keeps the
-- hot-path lookup index-friendly (one b-tree probe, no LIKE
-- gymnastics). source records where the learn came from
-- ('user_edit' from PATCH; future paths: 'cli', 'batch-edit',
-- 'voice'). learned_at is ISO-8601 UTC, same convention as the
-- rest of the schema (books.created_at, etc.).
--
-- Multi-write semantics: a single (title_norm, author_norm)
-- can map to multiple ASINs over time (operator changed mind;
-- book has both per-region and box-set ASINs). We keep all rows;
-- the consume side will sort by learned_at DESC and prefer the
-- most recent. The UNIQUE index is on (title_norm, author_norm,
-- asin) so identical re-learns don't duplicate.

CREATE TABLE asin_learnings (
    learning_id  INTEGER PRIMARY KEY AUTOINCREMENT,
    title_norm   TEXT    NOT NULL,
    author_norm  TEXT    NOT NULL,
    asin         TEXT    NOT NULL,
    source       TEXT    NOT NULL,
    learned_at   TEXT    NOT NULL,
    UNIQUE (title_norm, author_norm, asin)
);

CREATE INDEX idx_asin_learnings_lookup
    ON asin_learnings (title_norm, author_norm);

CREATE INDEX idx_asin_learnings_asin
    ON asin_learnings (asin);
