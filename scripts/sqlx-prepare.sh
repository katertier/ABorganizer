#!/usr/bin/env bash
# Refresh the .sqlx/ query cache used by sqlx::query!() macros.
#
# Run this whenever you add, remove, or change a `sqlx::query!()`
# (or `query_as!` / `query_scalar!`) call. CI's `sqlx-prepare` job
# fails if the committed cache disagrees with what the macros would
# produce against a fresh schema, so the workflow is:
#
#   1. Edit a query.
#   2. Update the schema migration if columns/tables changed.
#   3. Run this script.
#   4. `git add .sqlx/` and commit alongside the code change.
#
# Why a separate script: `cargo sqlx prepare` needs DATABASE_URL
# pointed at a real SQLite that has the schema applied. This script
# wires that up using the initial migration; no live `library.db`
# is touched.
set -euo pipefail

PREP_DB="${SQLX_PREP_DB:-/tmp/aborg-prep.db}"

cd "$(git rev-parse --show-toplevel)"

if ! command -v sqlx >/dev/null 2>&1; then
    echo "error: sqlx-cli not installed." >&2
    echo "       cargo install --locked sqlx-cli --no-default-features --features sqlite,rustls --version '~0.8'" >&2
    exit 1
fi

rm -f "$PREP_DB"
# Apply every library + ephemeral migration into a single prep DB.
# sqlx prepare validates query strings against ONE schema; we just
# need every CREATE TABLE referenced from any sqlx::query!() call
# to exist somewhere in the prep DB. The two production DBs stay
# separate at runtime — this is purely a compile-time concern.
#
# Some tables (e.g. `meta`) are declared in BOTH schemas with the
# same shape. Rewrite the migration text to `... IF NOT EXISTS` so
# the second declaration is a no-op in the prep DB. Production
# migrations run against separate DBs, so they stay strict — this
# rewrite never touches the source files.
for migration in crates/db/migrations/library/*.sql crates/db/migrations/ephemeral/*.sql; do
    sed -E 's/CREATE (UNIQUE )?(TABLE|INDEX) /CREATE \1\2 IF NOT EXISTS /g; s/INSERT INTO /INSERT OR IGNORE INTO /g' \
        "$migration" | sqlite3 "$PREP_DB"
done
echo "prep DB ready: $PREP_DB"

# `cargo sqlx prepare --workspace` invokes `cargo check` under the
# hood. By default the spawned `cargo check` honours the workspace's
# `default-members` (a small set of binaries), so any workspace
# member that isn't transitively depended on by one of those
# defaults — e.g. an early-slice scaffold crate — gets silently
# skipped. Passing `--workspace --all-targets` after `--` forces
# cargo to check every lib + test target in members[], which is
# the coverage we actually need for the cache.
DATABASE_URL="sqlite://$PREP_DB" cargo sqlx prepare --workspace -- --workspace --all-targets
echo "cache refreshed: .sqlx/ ($(ls .sqlx/ | wc -l | tr -d ' ') entries)"
