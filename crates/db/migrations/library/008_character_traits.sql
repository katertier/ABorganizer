-- slice 3K.6: extend `characters` with PoV flag + six trait columns.
--
-- The 3K.6 LLM character extractor (`extract-characters`)
-- produces per-character traits alongside the existing
-- name/aliases/role/description/lang. ADR-0022's "Character
-- trait taxonomy" table is the source of truth for what each
-- column means and when it's filled.
--
-- All six trait columns are nullable: the extractor only fills
-- a column when the transcript gives signal. Contemporary
-- fiction stays NULL on `species`; characters whose occupation
-- isn't named stay NULL on `occupation`; etc. Queries that
-- want "books with non-human characters" use
-- `WHERE species IS NOT NULL`.
--
-- `is_pov` is NOT NULL DEFAULT 0 — the extractor always emits
-- a 0/1 value, never elides it. The flag is structural rather
-- than descriptive: an `is_pov` of NULL would force every UI
-- "show PoV characters" filter to disambiguate
-- "definitely-not-PoV" vs "unknown".
--
-- Vocabulary policy per ADR-0022 § Vocabulary policy: free-
-- form in v1, learned-canonical mapping in a post-library-
-- scan slice. `age` is the one closed-bracket field (child /
-- teen / adult / elderly / immortal), enforced at the
-- complete_structured schema layer rather than as a SQL CHECK
-- constraint — keeps the migration `ALTER TABLE ADD COLUMN`-
-- shaped and lets future bracket revisions ship without a
-- schema change.

ALTER TABLE characters ADD COLUMN is_pov INTEGER NOT NULL DEFAULT 0;
ALTER TABLE characters ADD COLUMN species TEXT;
ALTER TABLE characters ADD COLUMN condition TEXT;
ALTER TABLE characters ADD COLUMN occupation TEXT;
ALTER TABLE characters ADD COLUMN age TEXT;
ALTER TABLE characters ADD COLUMN gender TEXT;
ALTER TABLE characters ADD COLUMN affiliation TEXT;

-- Useful for the "show me books with PoV characters of
-- species X" query class. PoV filtering is the dominant
-- read pattern; the species + condition columns are second-
-- order facets that won't justify their own indices until
-- queries surface. Add then, not now.
CREATE INDEX idx_characters_is_pov ON characters(book_id, is_pov);
