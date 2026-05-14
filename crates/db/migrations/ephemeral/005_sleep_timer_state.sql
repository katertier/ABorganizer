-- ADR-0046: sleep_timer_state — session-local countdown for player.
--
-- Lives in ephemeral.db: a sleep timer is a session-local concept and
-- resets on daemon restart. Persisting it across restarts would mean
-- the operator's "30 minutes from now" timer fires hours later
-- because the daemon was offline — surprising UX. Restart-reset is
-- intentional; the operator re-sets the timer if they want it back.
--
-- One active timer per session (pairing token). `mode` distinguishes
-- a fixed wall-clock target from "pause at the next chapter
-- boundary". `paused_at_ms` is non-NULL when playback is paused so
-- the remaining time can be preserved.

CREATE TABLE sleep_timer_state (
    session_token    TEXT PRIMARY KEY,
    book_id          INTEGER,                   -- book currently playing
    target_unix_ms   INTEGER NOT NULL,          -- when to pause
    mode             TEXT NOT NULL CHECK (mode IN (
                        'fixed',                 -- N ms from start
                        'end_of_chapter'         -- pause at chapter_end_ms
                    )),
    started_at_ms    INTEGER NOT NULL,
    paused_at_ms     INTEGER                    -- NULL while running
) STRICT;
