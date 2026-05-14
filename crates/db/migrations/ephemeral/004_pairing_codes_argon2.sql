-- Migration 004 — pairing_codes: argon2id-hashed code storage.
--
-- Backlog item 4b: pairing-code flow. The original schema (001)
-- stored `code TEXT PRIMARY KEY` plaintext, which is fine for
-- the dev-only scaffold but unfit for the real flow — a leaked
-- ephemeral.db would directly leak every pending pairing code.
--
-- Pairing codes are LOW ENTROPY (8 ASCII chars ≈ 40 bits; the
-- format we issue is `XXXX-XXXX` from a 26-letter alphabet). At
-- that entropy level, plain blake3 would be brute-forceable
-- offline. argon2id with the workspace defaults (m=19456,
-- t=2, p=1) takes ~50ms per verify on Apple Silicon, putting
-- ten million tries of an 8-char code at ~15 GPU-years — well
-- past the operational lifetime of a code (default 10 min).
--
-- The verify-against-every-row cost is acceptable here because
-- the table is tiny (operators rarely have >5 codes pending at
-- once) and consume is a one-shot human flow, not a hot path.
-- 5 verifies × 50ms = ~250ms per attempted consume.
--
-- Schema change:
--   - `code TEXT PRIMARY KEY` → `code_id INTEGER PRIMARY KEY
--     AUTOINCREMENT, code_hash TEXT NOT NULL` — the hash is the
--     verify target, code_id is the surrogate for revoke/list.
--
-- No data preserved: ABorganizer is pre-alpha, the pairing flow
-- never shipped, the only rows in this table would be from
-- tests. The cleanup target's queries don't reference the
-- `code` column at all — they only filter on `consumed_token_id`
-- and `expires_at` — so existing logic keeps working.

DROP TABLE IF EXISTS pairing_codes;

CREATE TABLE pairing_codes (
    code_id           INTEGER PRIMARY KEY AUTOINCREMENT,
    -- argon2id PHC-format string. Verify via
    -- `ab_core::auth::verify_password(presented_code, &row.code_hash)`.
    code_hash         TEXT NOT NULL,
    -- Operator-friendly label captured at issue time. Stays
    -- attached on the issued token's `nickname` column post-
    -- consume so device listings stay consistent across the
    -- two tables.
    device_label      TEXT NOT NULL,
    -- JSON-encoded array of scope strings (free-form today,
    -- typed in a future slice).
    scopes_json       TEXT NOT NULL,
    issued_at         INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    expires_at        INTEGER NOT NULL,
    -- NULL until consumed. On a successful `POST /pairing/consume`
    -- this FK is set to the freshly-issued `tokens.token_id`.
    -- The FK is across two databases (library.db.tokens vs.
    -- ephemeral.db.pairing_codes), which SQLite can't enforce —
    -- so it's an unenforced `INTEGER` reference, semantically
    -- documented here.
    consumed_token_id INTEGER
) STRICT;

-- Cleanup target filters by `consumed_token_id IS NULL` on every
-- pass; the partial index makes that filter ~O(eligible rows)
-- instead of O(all rows). Costs nothing at the deployment scale
-- we target.
CREATE INDEX idx_pairing_codes_pending
    ON pairing_codes(expires_at)
    WHERE consumed_token_id IS NULL;
