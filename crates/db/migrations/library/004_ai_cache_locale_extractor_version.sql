-- slice B2: ai_cache schema cleanup.
--
-- Two changes to the cache row, motivated by the course-
-- correction conversation that landed in slice A/B:
--
-- 1. ADD COLUMN `locale TEXT` — every transcript / LLM cache
--    row carries the BCP-47 locale it was produced in. Up to
--    now we embedded it inside the JSON BLOB and decoded the
--    blob just to read the locale for freshness comparisons.
--    A column means freshness becomes a one-row SQL check.
--
-- 2. RENAME COLUMN `model_version` → `extractor_version` — the
--    column tracks the version of whatever extractor wrote
--    the row. Today that's either a Speech engine version
--    (`speech-26.0-v1`) or a Foundation Models version
--    (`fm-26.0-v1`). The new name generalises across both
--    + future non-Apple backends (whisper, llama) without a
--    second rename.
--
-- SQLite supports both forms since 3.25 / 3.35. No data
-- migration needed: pre-existing rows on a fresh dev DB will
-- be wiped on next scan anyway (the DB is not yet user-facing
-- per PROJECT.md).

ALTER TABLE ai_cache ADD COLUMN locale TEXT;
ALTER TABLE ai_cache RENAME COLUMN model_version TO extractor_version;
