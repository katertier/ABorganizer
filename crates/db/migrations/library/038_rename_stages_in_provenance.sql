-- Stage naming harmonisation (2026-05-15 retrospective, item #8).
--
-- Companion migration to `ephemeral/007_rename_stages.sql`. Rewrites
-- `book_field_provenance.stage` values so the
-- `Stage::reset(book_id)` flow (slice H.1.5) can still find rows by
-- the new stage names.
--
-- `consensus` and `fingerprint` both wrote `book_field_provenance`
-- rows in older pipeline runs. The provenance.stage column was added
-- in migration 011 (slice H.1.2); rows from before that migration
-- have NULL stage and aren't affected.

UPDATE book_field_provenance
   SET stage = 'promote-consensus'
 WHERE stage = 'consensus';

UPDATE book_field_provenance
   SET stage = 'fingerprint-book'
 WHERE stage = 'fingerprint';
