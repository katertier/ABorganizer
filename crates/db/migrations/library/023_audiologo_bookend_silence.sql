-- Migration 023 — bookend-silence persistence on book_file_audiologos.
--
-- ADR-0024 Revision 3 (2026-05-14) adds two synthetic-silence
-- parameters (`head_silence_ms` / `tail_silence_ms`) to
-- `ApplyCutParams`. They're only applied to the audio when the
-- corresponding side of the cut does NOT land in natural silence;
-- when natural silence is already there, no synthetic silence is
-- prepended/appended.
--
-- The two flags below persist the detector's report on whether
-- each cut boundary lands in natural silence, so the audio-cut
-- path (future slice 4B.x.2) can short-circuit correctly without
-- re-inspecting the waveform each time it applies a row.
--
-- Defaults:
--   - 0 (false) = "boundary lands in speech / needs synthetic
--     silence". This is the conservative default per ADR-0024 Rev 3
--     "Out-of-scope for 4B" note — until detector logic flips the
--     flags (slice 4B.x.3, post-calibration), every applied row
--     gets synthetic padding.
--   - The detector (slice 4B.x.3) will eventually set the flag to
--     1 (true) when the waveform analysis confirms natural silence
--     at the boundary.
--
-- No backfill: every existing row defaults to 0 / "always pad",
-- which matches the conservative behavior expected when older
-- detector runs didn't analyze silence.

ALTER TABLE book_file_audiologos
    ADD COLUMN head_lands_in_silence INTEGER NOT NULL DEFAULT 0
        CHECK (head_lands_in_silence IN (0, 1));

ALTER TABLE book_file_audiologos
    ADD COLUMN tail_lands_in_silence INTEGER NOT NULL DEFAULT 0
        CHECK (tail_lands_in_silence IN (0, 1));

-- No new indexes — these columns are read alongside the row via
-- `audiologo_row_id` lookup (the audio-cut path fetches one row by
-- PK). Independent filtering on the flags is not a planned
-- workload.
