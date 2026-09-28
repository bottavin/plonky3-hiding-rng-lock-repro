#!/usr/bin/env bash
# Usage: ./run-tests.sh <upstream|fixed> [test_name ...]
#
# Runs every test in its own process. After a deadlock the spinning worker threads
# keep running until the process exits, so they must not slow down the next test.
# Output goes to the terminal and to logs/<mode>-<timestamp>.log.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
MODE="${1:-}"
case "$MODE" in
    upstream|fixed) shift ;;
    *) echo "usage: $0 <upstream|fixed> [test_name ...]" >&2; exit 64 ;;
esac

if [ "$MODE" = fixed ] && [ ! -d "$ROOT/vendor/plonky3-fixed" ]; then
    echo "error: vendor/plonky3-fixed is missing. Run ./setup-fixed.sh first." >&2
    exit 1
fi

TESTS=("$@")
if [ ${#TESTS[@]} -eq 0 ]; then
    TESTS=(
        bug3_hiding_mmcs_roundtrip
        bug3_hiding_mmcs_concurrent_commit
        bug4_hiding_pcs_concurrent_get_quotient_ldes
        bug5_hiding_pcs_roundtrip
        bug5_hiding_pcs_concurrent_commit
    )
fi

CORES="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 8)"

mkdir -p "$ROOT/logs"
LOG="$ROOT/logs/$MODE-$(date +%Y%m%d-%H%M%S).log"
cd "$ROOT/$MODE"

{
    echo "mode: $MODE"
    rustc --version
    echo "cores: $CORES"
    env | grep '^REPRO_' || true
    echo
} | tee "$LOG"

if ! cargo test --release --no-run 2>&1 | tee -a "$LOG"; then
    echo "BUILD FAILED" | tee -a "$LOG"
    exit 1
fi

declare -a RESULTS
for t in "${TESTS[@]}"; do
    # bug3 hangs reliably only with more rayon workers than cores. On a 32-core
    # machine it passed with 8, 16 and 32 workers and hung with 64 and 128.
    # An explicit RAYON_NUM_THREADS from the caller always wins.
    rt="${RAYON_NUM_THREADS:-}"
    if [ "$t" = bug3_hiding_mmcs_concurrent_commit ] && [ -z "$rt" ]; then
        rt=$((2 * CORES))
    fi
    echo "=== $t${rt:+ (RAYON_NUM_THREADS=$rt)}" | tee -a "$LOG"
    if env ${rt:+RAYON_NUM_THREADS=$rt} cargo test --release --test hiding_rng_lock -- --exact "$t" --nocapture 2>&1 | tee -a "$LOG"; then
        RESULTS+=("PASS  $t")
    else
        RESULTS+=("FAIL  $t")
    fi
done

{
    echo
    echo "=== summary ($MODE)"
    printf '%s\n' "${RESULTS[@]}"
    echo "log: $LOG"
} | tee -a "$LOG"