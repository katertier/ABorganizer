-- Slice C5.3: pin `book_field_provenance.field` vocabulary at
-- the DB layer with a CHECK constraint matching the
-- `ab_core::Field` enum.
--
-- Up through slice C5.2 the typed-vocabulary invariant
-- ("`field` values come from a closed set") was enforced
-- workspace-wide on the Rust side: extractors and consumers go
-- through `ab_core::Field::*`, with `.as_str()` at the SQL bind
-- site. The CHECK adds storage-layer enforcement so a future
-- runtime `sqlx::query()` site (or a future direct-SQL admin tool)
-- can't sneak an off-vocabulary value past the typed Rust
-- surface.
--
-- ── Why a table rebuild ────────────────────────────────────────────
--
-- SQLite has no `ALTER TABLE ... ADD CONSTRAINT`. The canonical
-- pattern (sqlite.org `lang_altertable.html` § 7) is "build a
-- new table with the constraint, copy rows, drop the old,
-- rename the new". The four statements run inside the sqlx
-- migration transaction so the rebuild is atomic.
--
-- ── Vocabulary ────────────────────────────────────────────────────
--
-- Mirrors `Field::as_str()` exactly. New variants land here in
-- the same commit that adds them to the enum (a stray DB write
-- with an unrecognised `field` would fail the CHECK; the test
-- suite catches this for new variants because
-- `book_field_provenance` rows are written end-to-end in
-- `crates/catalog/tests/promote_drift.rs`).
--
-- ── On migration failure ──────────────────────────────────────────
--
-- If a dev or fixture DB has rows whose `field` value is
-- outside the enum, the rebuild's `INSERT INTO ... SELECT`
-- will fail the CHECK and the migration aborts. That's the
-- intended behaviour — loud failure beats silent data drift.
-- A fresh dev DB after `aborg library scan` populates only
-- enum-valid values (every extractor goes through `Field::*`
-- post-C5.1).

CREATE TABLE book_field_provenance_new (
    provenance_id  INTEGER PRIMARY KEY AUTOINCREMENT,
    book_id        INTEGER NOT NULL REFERENCES books(book_id) ON DELETE CASCADE,
    field          TEXT NOT NULL CHECK (field IN (
        'title', 'subtitle', 'description', 'language',
        'release_date', 'duration_seconds', 'asin', 'isbn',
        'author', 'narrator', 'publisher', 'series', 'genre',
        'cover_url', 'abridged', 'explicit'
    )),
    value          TEXT,
    source         TEXT NOT NULL,
    confidence     REAL NOT NULL,
    is_winner      INTEGER NOT NULL DEFAULT 0,
    external_id    TEXT,
    recorded_at    INTEGER NOT NULL DEFAULT (strftime('%s','now'))
) STRICT;

INSERT INTO book_field_provenance_new (
    provenance_id, book_id, field, value, source, confidence,
    is_winner, external_id, recorded_at
)
SELECT
    provenance_id, book_id, field, value, source, confidence,
    is_winner, external_id, recorded_at
FROM book_field_provenance;

DROP TABLE book_field_provenance;

ALTER TABLE book_field_provenance_new RENAME TO book_field_provenance;
