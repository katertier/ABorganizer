-- Stage naming harmonisation pass 2 (2026-05-15 follow-up to #7).
--
-- See `library/039_rename_stages_pass_2.sql` for the full mapping +
-- rationale. This file rewrites `pipeline_progress` rows referencing
-- the old names so the dispatcher's stage-name lookup keeps finding
-- them on the first scheduler sweep after the rename lands.

UPDATE pipeline_progress SET stage = 'read-tags'              WHERE stage = 'tag-read';
UPDATE pipeline_progress SET stage = 'enrich-from-audnexus'   WHERE stage = 'audnexus-enrich';
UPDATE pipeline_progress SET stage = 'fetch-audnexus-chapters' WHERE stage = 'audnexus-chapters';
UPDATE pipeline_progress SET stage = 'search-audible'         WHERE stage = 'audible-search';
UPDATE pipeline_progress SET stage = 'resolve-identity'       WHERE stage = 'identity-resolve';
UPDATE pipeline_progress SET stage = 'pick-chapter-winner'    WHERE stage = 'chapter-pick-winner';
UPDATE pipeline_progress SET stage = 'read-embedded-chapters' WHERE stage = 'embedded-chapters';
UPDATE pipeline_progress SET stage = 'write-tags-early'       WHERE stage = 'tag-write-early';
UPDATE pipeline_progress SET stage = 'write-tags-final'       WHERE stage = 'tag-write-final';
