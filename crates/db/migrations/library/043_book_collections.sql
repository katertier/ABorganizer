-- Box-set / collection foundation (BACKLOG cycle 35).
--
-- A box set is a grouping of standalone audiobooks marketed
-- together — typically a publisher bundle ("The Complete Stormlight
-- Archive Box Set"), a thematic compilation ("Halloween Horrors
-- Collection"), or a season-based release ("True Crime All-Stars
-- Vol. 3"). A box set is NOT a series (series have an authoritative
-- ordering; collections may or may not).
--
-- This schema slice lands the tables; the scanner heuristic
-- (looking at directory layout + tag metadata + Audible
-- collection-of-titles ASINs) ships in a follow-up slice once the
-- schema is observable from the API.
--
-- ── Design choices ───────────────────────────────────────────
--
-- 1. **Separate from series.** Series live in `series` + the
--    `books.series_id` / `books.series_index` FK. A book CAN
--    belong to both a series and a collection (a Stormlight book
--    is in the "Stormlight Archive" series AND the "Stormlight
--    Box Set" collection). Modelling as a junction table keeps
--    both relationships.
--
-- 2. **Junction table over `books.collection_id` FK.** A book
--    can belong to multiple collections (publisher bundle +
--    operator's "best of 2025" custom collection). Single-FK
--    would force one. Use `book_collection_members` to land
--    the M:N relation.
--
-- 3. **`source` column on the membership row.** Distinguishes
--    scanner-detected memberships from operator-curated ones.
--    Future scanner heuristic rewrites set `source = 'scanner'`;
--    operator wires from the GUI / API set `source = 'manual'`.
--    Manual rows survive scanner reruns; scanner rows can be
--    rebuilt without trampling manual curation.
--
-- 4. **`position` is optional.** Some box sets have an order
--    (volume 1 / volume 2 / volume 3); others are just a bag
--    (a "horror collection" with no canonical sequence). NULL
--    means "no canonical position" — the UI sorts by title
--    fallback.
--
-- 5. **No `is_active` soft-delete column today.** Pre-1.0;
--    collections that should disappear get DELETEd. If
--    soft-delete becomes a need post-1.0 (e.g. "operator
--    archived this collection but wants undo"), add an
--    `is_active INTEGER NOT NULL DEFAULT 1` column then.

CREATE TABLE book_collections (
    collection_id   INTEGER PRIMARY KEY AUTOINCREMENT,
    name            TEXT NOT NULL,
    -- Optional normalized form for lookup; canonical-collection
    -- enrichment will populate this when it lands.
    canonical_name  TEXT,
    -- Audible "collection ASIN" — the box-set's own product ID
    -- on Audible, distinct from its member books' ASINs. NULL
    -- for scanner-detected or operator-curated collections that
    -- don't correspond to an Audible product.
    audible_id      TEXT,
    description     TEXT,
    -- Free-text classification: 'box_set', 'compilation',
    -- 'curated', etc. Vocabulary is open today; pin to an enum
    -- when scanner + GUI usage settles.
    kind            TEXT,
    created_at      INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    updated_at      INTEGER NOT NULL DEFAULT (strftime('%s','now'))
) STRICT;

CREATE UNIQUE INDEX ux_book_collections_name ON book_collections(name);
-- audible_id is a strong identity key when present; lookup-only.
-- Partial index keeps it NULL-friendly.
CREATE UNIQUE INDEX ux_book_collections_audible_id
    ON book_collections(audible_id)
    WHERE audible_id IS NOT NULL;

CREATE TABLE book_collection_members (
    member_id       INTEGER PRIMARY KEY AUTOINCREMENT,
    collection_id   INTEGER NOT NULL
                        REFERENCES book_collections(collection_id)
                        ON DELETE CASCADE,
    book_id         INTEGER NOT NULL
                        REFERENCES books(book_id)
                        ON DELETE CASCADE,
    -- Optional ordinal in the collection (1-indexed, like
    -- books.series_index). NULL = unordered bag.
    position        INTEGER,
    -- 'scanner' | 'manual' (see header note 3). Default scanner
    -- to mirror the dominant path; manual rows set it explicitly.
    source          TEXT NOT NULL DEFAULT 'scanner',
    added_at        INTEGER NOT NULL DEFAULT (strftime('%s','now'))
) STRICT;

CREATE UNIQUE INDEX ux_book_collection_members
    ON book_collection_members(collection_id, book_id);
CREATE INDEX idx_book_collection_members_book
    ON book_collection_members(book_id);
