-- slice 3K.2: schema for LLM-driven extractors.
--
-- The four extractors landing in 3K.3-6 each need a place to
-- persist their promoted output. Raw LLM responses (with
-- `model_version`) are cached in the existing `ai_cache` table;
-- this migration adds the promoted views on `books` plus a new
-- `characters` table for the character extractor.
--
-- Re-extraction policy: each extractor stage clears its rows
-- for the affected book and re-writes. The cache_type row in
-- `ai_cache` carries the model_version that produced them;
-- `merge` invalidates a promoted value when the cache row
-- predates the current model version.

-- ── books: spoiler-free summary ────────────────────────────────────
-- LLM-rewritten plot summary safe for browsing. Lives next to
-- `description` rather than replacing it — `description` is the
-- catalog text (Audible / Audnexus / publisher blurb) and may
-- include spoilers; `summary_spoiler_free` is the version the
-- UI surfaces when "hide spoilers" is on (default).
--
-- `summary_spoiler_free_lang` is the language the summary was
-- written in. Same BCP-47 primary-subtag form as `language` and
-- `description_lang`. The extractor honours the library locale
-- (so a German library gets German summaries even for English
-- books); NULL = unknown.
ALTER TABLE books ADD COLUMN summary_spoiler_free TEXT;
ALTER TABLE books ADD COLUMN summary_spoiler_free_lang TEXT;

-- ── books: story arc ───────────────────────────────────────────────
-- JSON-encoded array of {step, label, summary} objects laying
-- out the book's narrative beats. Used by the UI for the
-- "story arc" sidebar (spoiler-aware: each step has its own
-- reveal toggle). Stored as JSON rather than a relational
-- arc_steps table because rows are always read all-or-nothing
-- and there's no cross-book querying on individual beats.
ALTER TABLE books ADD COLUMN story_arc_json TEXT;

-- ── characters ─────────────────────────────────────────────────────
-- Extracted from the transcript by the LLM character pass.
-- One row per canonical character per book.
--
-- `aliases` is a JSON array of alternate names the extractor
-- saw in the text (nicknames, titles, etc.). Identity-resolve
-- in the extractor collapses these into a single canonical
-- name before writing.
--
-- `role` is one of {protagonist, antagonist, supporting,
-- mentioned} — kept as a free-form TEXT rather than a CHECK
-- constraint so future extractor revisions can introduce new
-- categories (e.g. "narrator-character" for first-person
-- audiobooks) without a schema change.
--
-- `description` is a brief, deliberately spoiler-free blurb
-- (one or two sentences) — the LLM is instructed to describe
-- the character without revealing plot twists.
--
-- `lang` carries the language `name` + `description` are
-- written in (BCP-47 primary subtag). Honours library locale
-- just like `summary_spoiler_free_lang`.
--
-- UNIQUE (book_id, name) keeps re-extraction idempotent: the
-- characters stage clears the book's rows then re-inserts.
CREATE TABLE characters (
    character_id   INTEGER PRIMARY KEY AUTOINCREMENT,
    book_id        INTEGER NOT NULL REFERENCES books(book_id) ON DELETE CASCADE,
    name           TEXT NOT NULL,
    aliases        TEXT,
    role           TEXT,
    description    TEXT,
    lang           TEXT,
    created_at     INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    UNIQUE (book_id, name)
) STRICT;
CREATE INDEX idx_characters_book ON characters(book_id);
CREATE INDEX idx_characters_name ON characters(name);
