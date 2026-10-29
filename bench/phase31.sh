#!/usr/bin/env bash
# Final-source Phase 2 / candidate comparison. Build and record both binaries
# first (scripts/build-manifest.py); never compile while measurements run.
# Usage: bench/phase31.sh BASELINE_BIN CANDIDATE_BIN
set -euo pipefail
cd "$(dirname "$0")/.."
[ "$#" = 2 ] || { echo "usage: $0 BASELINE_BIN CANDIDATE_BIN" >&2; exit 2; }
for binary in "$@"; do
  test -x "$binary"
  test -s "$binary.build.json"
done
export PERF_CONNECTIONS="1 8 32 100 128 512"
export PERF_SIZE_CONNECTIONS="$PERF_CONNECTIONS"
export PERF_MISS_CONNECTIONS="$PERF_CONNECTIONS"
export PERF_ROUNDS=${PERF_ROUNDS:-3}
export PERF_DURATION=${PERF_DURATION:-8}
export PERF_WARMUP=${PERF_WARMUP:-2}
export PERF_OUT=${PERF_OUT:-"$PWD/benchmark-results/phase31/perf"}
for policy in clock tinyufo; do
  CACHE_EVICTION=$policy bash bench/perf-ab.sh "$1" "$2" hit sizes miss coalesce stream
done
# Compare the explicit single-worker and bounded two-worker alternatives
# against the default above. Identical traffic, machine pinning and warmups.
for workers in 1 2; do
  CACHE_EVICTION=clock TANOD_BIN=$2 PERF_THREADS=$workers \
    PERF_LABEL="workers-$workers" PERF_TRIALS="$PERF_ROUNDS" \
    bash bench/perf.sh hit sizes
done
# Benefit demonstration: capacity-limited origin, then protected requests.
# This is deliberately separate from the proxy-overhead A/B measurements.
CACHE_EVICTION=clock TANOD_BIN=$2 PERF_LABEL=demonstration \
  PERF_TRIALS="$PERF_ROUNDS" PERF_WARMUP="${DEMO_WARMUP:-5}" \
  PERF_DURATION="${DEMO_DURATION:-4}" bash bench/perf.sh overload coalesce
