-- ADR-0042: tag hierarchy + content warnings extension to the
-- DNA-tag LLM extractor.
--
-- Two tables:
--
--   * tag_hierarchy — global parent/child relationships emitted
--     by the LLM ("High Fantasy is a kind of Fantasy"). Not
--     per-book; multiple books contributing the same pair is a
--     PK no-op. Cycle prevention is enforced at the application
--     layer (the executor walks descendants before inserting).
--
--   * book_content_warnings — per-book content-warning entries
--     drawn from a fixed canonical vocabulary (violence,
--     sexual_assault, gore, addiction, suicide, etc.). The DNA
--     prompt enumerates the vocabulary; the executor rejects
--     freeform labels. Per-locale UI translation is handled at
--     render time on the canonical English label.
--
-- Both tables are STRICT so the column types are enforced at the
-- engine layer rather than coerced silently. `tag_hierarchy` is
-- WITHOUT ROWID — small table, two-column PK, the rowid is pure
-- overhead. `book_content_warnings` keeps the rowid because the
-- ON DELETE CASCADE FK + the `extracted_at` ordering benefit from
-- having one.

CREATE TABLE tag_hierarchy (
    parent_tag      TEXT NOT NULL,
    child_tag       TEXT NOT NULL,
    PRIMARY KEY (parent_tag, child_tag)
) STRICT, WITHOUT ROWID;

CREATE INDEX idx_tag_hierarchy_child
    ON tag_hierarchy(child_tag);

CREATE TABLE book_content_warnings (
    book_id         INTEGER NOT NULL
                        REFERENCES books(book_id) ON DELETE CASCADE,
    label           TEXT NOT NULL,
    severity        TEXT NOT NULL
                        CHECK (severity IN ('mild', 'moderate', 'intense', 'graphic')),
    extracted_at    INTEGER NOT NULL,
    PRIMARY KEY (book_id, label)
) STRICT;

CREATE INDEX idx_book_content_warnings_label
    ON book_content_warnings(label);
