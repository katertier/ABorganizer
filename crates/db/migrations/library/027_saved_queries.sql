-- ADR-0034: saved queries — unified shape powering series views,
-- smart filters, dashboard tiles, recently-added, similar-books.
--
-- One table, one executor. `kind` discriminates the presentation
-- layer (UI renders dashboard_tile differently from smart_filter,
-- but they share the same `query_json` shape). `query_json`
-- carries a serialised `ab_query::QueryFilter` (ADR-0031);
-- `sort_json` optionally overrides the filter's own sort.
--
-- `owner_kind = 'system'` marks builtin rows that ship with the
-- DB and aren't user-editable; everything else is user-owned and
-- mutable via the CRUD endpoints.

CREATE TABLE saved_queries (
    query_id        INTEGER PRIMARY KEY AUTOINCREMENT,
    kind            TEXT NOT NULL CHECK (kind IN (
                        'series_view',
                        'smart_filter',
                        'dashboard_tile',
                        'recently_added',
                        'similar_books',
                        'system'
                    )),
    name            TEXT NOT NULL,
    description     TEXT,
    query_json      TEXT NOT NULL,
    sort_json       TEXT,
    pin_position    INTEGER,
    owner_kind      TEXT NOT NULL DEFAULT 'user'
                        CHECK (owner_kind IN ('system', 'user')),
    created_at      INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    updated_at      INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    UNIQUE (kind, name)
) STRICT;

CREATE INDEX idx_saved_queries_kind ON saved_queries(kind);
CREATE INDEX idx_saved_queries_pin
    ON saved_queries(pin_position)
    WHERE pin_position IS NOT NULL;
