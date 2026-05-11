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
MIGRATION="crates/db/migrations/library/001_initial.sql"

cd "$(git rev-parse --show-toplevel)"

if ! command -v sqlx >/dev/null 2>&1; then
    echo "error: sqlx-cli not installed." >&2
    echo "       cargo install --locked sqlx-cli --no-default-features --features sqlite,rustls --version '~0.8'" >&2
    exit 1
fi

rm -f "$PREP_DB"
sqlite3 "$PREP_DB" < "$MIGRATION"
echo "prep DB ready: $PREP_DB"

DATABASE_URL="sqlite://$PREP_DB" cargo sqlx prepare --workspace
echo "cache refreshed: .sqlx/ ($(ls .sqlx/ | wc -l | tr -d ' ') entries)"
