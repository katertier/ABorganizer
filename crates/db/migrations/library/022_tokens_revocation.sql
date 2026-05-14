-- Migration 022 — tokens.revoked_at column + index.
--
-- Backlog item 4a: per-user token CRUD + auth middleware wiring.
--
-- The `tokens` table from migration 001 has `expires_at` for
-- natural expiry. `revoked_at` is operator-initiated revocation
-- (DELETE /api/v1/tokens/{token_id}). The auth middleware
-- filters by both — a token is valid iff
-- `revoked_at IS NULL AND (expires_at IS NULL OR expires_at > now)`.
--
-- We don't overload `expires_at` for revocation because the two
-- carry different semantics:
--   - `expires_at` is set at issue time and never mutates.
--   - `revoked_at` mutates exactly once (NULL → unix-seconds).
-- Keeping them separate lets `aborg tokens list` show both an
-- expected expiry AND a revocation timestamp on rows that got
-- revoked early.

ALTER TABLE tokens ADD COLUMN revoked_at INTEGER;

-- The existing UNIQUE on `token_hash` (migration 001) already
-- covers the auth middleware's hot lookup path; we filter by
-- `revoked_at IS NULL` in the WHERE clause, not via the index.
-- A partial index here would shave µs at the cost of a second
-- B-tree to maintain on every INSERT/UPDATE — not worth it at
-- the deployment scales we target.
