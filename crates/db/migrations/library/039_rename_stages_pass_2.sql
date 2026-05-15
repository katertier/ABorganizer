-- Stage naming harmonisation pass 2 (2026-05-15 follow-up to #38).
--
-- The first pass (#38) renamed only the two most-egregious bare-noun
-- stages (`consensus` and `fingerprint`). Per operator follow-up
-- ("please fix all"), the remaining inconsistent stage names get
-- harmonised to the verb-noun shape used elsewhere:
--
--   * `tag-read`            → `read-tags`
--   * `audnexus-enrich`     → `enrich-from-audnexus`
--   * `audnexus-chapters`   → `fetch-audnexus-chapters`
--   * `audible-search`      → `search-audible`
--   * `identity-resolve`    → `resolve-identity`
--   * `chapter-pick-winner` → `pick-chapter-winner`
--   * `embedded-chapters`   → `read-embedded-chapters`
--   * `tag-write-early`     → `write-tags-early`
--   * `tag-write-final`     → `write-tags-final`
--
-- All of these stages write `book_field_provenance` rows; rewrite
-- them in place so `Stage::reset(book_id)` still finds them by the
-- new names.

UPDATE book_field_provenance SET stage = 'read-tags'              WHERE stage = 'tag-read';
UPDATE book_field_provenance SET stage = 'enrich-from-audnexus'   WHERE stage = 'audnexus-enrich';
UPDATE book_field_provenance SET stage = 'fetch-audnexus-chapters' WHERE stage = 'audnexus-chapters';
UPDATE book_field_provenance SET stage = 'search-audible'         WHERE stage = 'audible-search';
UPDATE book_field_provenance SET stage = 'resolve-identity'       WHERE stage = 'identity-resolve';
UPDATE book_field_provenance SET stage = 'pick-chapter-winner'    WHERE stage = 'chapter-pick-winner';
UPDATE book_field_provenance SET stage = 'read-embedded-chapters' WHERE stage = 'embedded-chapters';
UPDATE book_field_provenance SET stage = 'write-tags-early'       WHERE stage = 'tag-write-early';
UPDATE book_field_provenance SET stage = 'write-tags-final'       WHERE stage = 'tag-write-final';
