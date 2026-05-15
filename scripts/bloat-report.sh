#!/usr/bin/env bash
# Generate a binary-size breakdown of the release `aborg-daemon`
# binary, write the report to `target/bloat-report.txt`, and diff
# it against the tracked baseline at `target/bloat-baseline.txt`.
#
# Run on the pre-release checklist + whenever a slice touches a
# heavy dep (image/symphonia/reqwest/sqlx/etc.). Not run on every
# PR — the `cargo bloat` build is slow + the per-PR signal is
# usually noise.
#
# Output: top 30 crates by `.text` size. The total binary size in
# the header line is the file size, not the .text-only sum.
#
# Refresh the baseline after a deliberate size-changing slice
# lands:
#
#   ./scripts/bloat-report.sh --refresh-baseline
#   git add target/bloat-baseline.txt
#
# The baseline file is checked in so cross-machine comparisons stay
# stable. It's tiny (< 4 KiB).
#
# Refer to ~/dev/ABorganizer-docs/SAVINGS.md for the recurring
# size-cleanup taxonomy.

set -euo pipefail

REFRESH_BASELINE=0
for arg in "$@"; do
    case "$arg" in
        --refresh-baseline)
            REFRESH_BASELINE=1
            ;;
        *)
            echo "usage: $0 [--refresh-baseline]" >&2
            exit 2
            ;;
    esac
done

cd "$(git rev-parse --show-toplevel)"

if ! command -v cargo-bloat >/dev/null 2>&1; then
    echo "error: cargo-bloat not installed." >&2
    echo "       cargo install cargo-bloat --locked" >&2
    exit 1
fi

BASELINE="scripts/bloat-baseline.txt"
REPORT="target/bloat-report.txt"

mkdir -p target

echo "→ building release aborg-daemon"
CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}" cargo build --release -p aborg-daemon >/dev/null

echo "→ measuring binary size"
DAEMON_BIN="target/release/aborg-daemon"
if [[ ! -f "$DAEMON_BIN" ]]; then
    echo "error: $DAEMON_BIN missing after build" >&2
    exit 1
fi
DAEMON_BYTES=$(stat -f %z "$DAEMON_BIN" 2>/dev/null || stat -c %s "$DAEMON_BIN")
DAEMON_MIB=$(awk -v b="$DAEMON_BYTES" 'BEGIN{printf "%.2f", b/1024/1024}')

echo "→ running cargo bloat (top 30 crates)"
{
    echo "aborg-daemon: ${DAEMON_BYTES} bytes (${DAEMON_MIB} MiB)"
    echo "generated: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo
    cargo bloat --release --crates -p aborg-daemon -n 30 2>&1 | \
        grep -v "^   Compiling\|^    Building\|^    Finished\|^    Analyzing"
} > "$REPORT"

if [[ "$REFRESH_BASELINE" -eq 1 ]]; then
    cp "$REPORT" "$BASELINE"
    echo "→ refreshed baseline: $BASELINE"
    exit 0
fi

if [[ -f "$BASELINE" ]]; then
    BASELINE_BYTES=$(awk '/^aborg-daemon: / {print $2}' "$BASELINE")
    DELTA=$((DAEMON_BYTES - BASELINE_BYTES))
    DELTA_MIB=$(awk -v d="$DELTA" 'BEGIN{printf "%+.2f", d/1024/1024}')
    PCT=$(awk -v d="$DELTA" -v b="$BASELINE_BYTES" 'BEGIN{printf "%+.2f", (d/b)*100}')
    echo
    echo "baseline: ${BASELINE_BYTES} bytes"
    echo "current:  ${DAEMON_BYTES} bytes"
    echo "delta:    ${DELTA_MIB} MiB (${PCT}%)"
    echo
    diff -u "$BASELINE" "$REPORT" || true
else
    echo
    echo "no baseline at $BASELINE — run with --refresh-baseline to create one."
fi
