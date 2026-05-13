-- slice 3K.8: add setting columns to `books`.
--
-- The 3K.8 LLM setting extractor (`extract-setting`) produces
-- a one-paragraph setting summary PLUS a list of `$`-prefixed
-- tags (10 categories per ADR-0022). Paragraph goes to a new
-- `books.setting` column; tags land in `book_tags` with
-- `source='setting_llm'`.
--
-- `setting_lang` carries the BCP-47 tag the paragraph is
-- written in. Same locale rule as `summary_spoiler_free`:
-- output stays in `books.language` regardless of
-- `library_locale` (ADR-0019).
--
-- `setting_extractor_version` mirrors the `summary_extractor_
-- version` pattern from migration 007 but at the book level —
-- not used by the cache-freshness path (that's still on
-- `ai_cache.extractor_version` keyed by `CacheKey::Setting`),
-- but reserved so a future "promotion freshness" check can
-- distinguish "promoted by this extractor_version" from
-- "promoted by an older one." Cheap to add now alongside the
-- value columns.

ALTER TABLE books ADD COLUMN setting TEXT;
ALTER TABLE books ADD COLUMN setting_lang TEXT;
ALTER TABLE books ADD COLUMN setting_extractor_version TEXT;
