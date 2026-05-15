-- ADR-0041: loudness measurement foundation.
--
-- Adds two REAL columns on `books`:
--   * lufs_integrated — EBU R-128 integrated loudness in LUFS.
--     NULL when the loudness-measurement stage hasn't run.
--     Typical audiobook range: -25 to -16 LUFS. Audible
--     publishes -18 ±2 LU as their target band; podcast spec
--     commonly targets -16 LUFS.
--   * lufs_truepeak   — true-peak (dBTP) under EBU R-128 with
--     4x oversampling. NULL when unmeasured. Clipping ceiling
--     for the optional gain stage on transcode (-1.0 dBTP is
--     the common ceiling that survives lossy re-encoding).
--
-- Both columns are populated by a future `loudness-measure`
-- stage that calls AVFoundation's loudness analyser via Swift
-- FFI on `ab-audio`. Schema lands now so:
--   1. Saved-query filters can reference them (loud-soft sort,
--      "louder than -20 LUFS" filters in the dashboard).
--   2. The optional ReplayGain-style gain on transcode can read
--      the per-book target without waiting on the FFI bridge
--      landing in the same slice.
--
-- Neither column has a default — NULL is the "not yet measured"
-- state. Indexes deferred until query patterns settle; both
-- columns are cheap to scan across 20k–100k rows.

ALTER TABLE books ADD COLUMN lufs_integrated REAL;
ALTER TABLE books ADD COLUMN lufs_truepeak REAL;
