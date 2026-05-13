-- Enforce one is_winner=1 row per (book_id, field).
--
-- The consensus stage's contract has always been "pick one
-- winner per field" but nothing in the schema enforced it.
-- A race condition (two consensus runs against the same book,
-- or a bug in winner-flag logic) could leave the table with
-- multiple is_winner=1 rows for the same (book_id, field) —
-- and the future tag-write stage would silently emit
-- inconsistent on-disk tags.
--
-- Surfaced by the cross-model code review (MYREVIEW.md § 4.1
-- + § 2.4). Two phases:
--
--   1. Dedupe any pre-existing offenders. For each duplicate
--      group, keep the row with the highest confidence
--      (tiebreak by latest recorded_at) and unset is_winner
--      on the rest. Their provenance survives at is_winner=0
--      — same audit trail.
--
--   2. Add a partial UNIQUE INDEX. From this point on,
--      INSERT/UPDATE that would create a second winner for
--      the same (book_id, field) fails with
--      SQLITE_CONSTRAINT_UNIQUE — converting a silent
--      consistency bug into an explicit error consensus
--      must handle.

-- ── Phase 1: dedupe ──────────────────────────────────────────
-- "Keep the survivor" = max(confidence) with recorded_at as
-- tiebreak. Demote everyone else.
UPDATE book_field_provenance
   SET is_winner = 0
 WHERE is_winner = 1
   AND provenance_id NOT IN (
       SELECT provenance_id
         FROM (
           SELECT provenance_id,
                  ROW_NUMBER() OVER (
                      PARTITION BY book_id, field
                      ORDER BY confidence DESC, recorded_at DESC, provenance_id DESC
                  ) AS rn
             FROM book_field_provenance
            WHERE is_winner = 1
         ) ranked
        WHERE rn = 1
   );

-- ── Phase 2: enforce going forward ───────────────────────────
CREATE UNIQUE INDEX ux_book_field_winner
    ON book_field_provenance(book_id, field)
    WHERE is_winner = 1;
