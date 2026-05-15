-- Stage naming harmonisation (2026-05-15 retrospective, item #8).
--
-- Two bare-noun stage names that read as state, not action, get the
-- verb-noun shape used by every other production stage:
--
--   * `consensus`   → `promote-consensus`
--   * `fingerprint` → `fingerprint-book`
--
-- The new names disambiguate from neighbouring concepts:
--
--   * "consensus" is a noun (the result) but also reads as a state.
--     "promote-consensus" makes it clear the stage's job is to
--     promote per-source candidate rows into a single winner.
--
--   * "fingerprint" collides with the audiologo stage's per-window
--     fingerprint matching. "fingerprint-book" clarifies this is the
--     whole-book identity fingerprint stage (chromaprint windows at
--     0/25/50/75% used by `aborg library duplicates`), distinct from
--     `detect-audiologo`'s per-jingle fingerprint slide.
--
-- pipeline_progress rows referencing the old names get rewritten in
-- place so the dispatcher's stage-name lookup keeps finding them on
-- the first scheduler sweep after the rename lands.

UPDATE pipeline_progress
   SET stage = 'promote-consensus'
 WHERE stage = 'consensus';

UPDATE pipeline_progress
   SET stage = 'fingerprint-book'
 WHERE stage = 'fingerprint';
