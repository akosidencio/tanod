#!/usr/bin/env bash
# Interleaved A/B comparison of two Tanod binaries with bench/perf.sh.
#
#   bench/perf-ab.sh BASELINE_BIN CANDIDATE_BIN [scenario ...]
#
# Runs one trial of every scenario for the baseline, then one for the
# candidate, and repeats PERF_ROUNDS times (default 3), so slow drift on the
# machine — another build starting, thermal state, a noisy neighbour — lands
# on both sides instead of on whichever ran second. Then prints the
# comparison. Results go to PERF_OUT/<label> as for perf.sh; labels default to
# `baseline` and `candidate` (PERF_LABEL_A / PERF_LABEL_B).
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1

[ $# -ge 2 ] || { sed -n '2,12p' "$0" >&2; exit 2; }
A=$1 B=$2; shift 2
ROUNDS=${PERF_ROUNDS:-3}
LABEL_A=${PERF_LABEL_A:-baseline}
LABEL_B=${PERF_LABEL_B:-candidate}
OUT=${PERF_OUT:-$PWD/benchmark-results/perf}
SUFFIX=""
[ -n "${CACHE_EVICTION:-}" ] && SUFFIX="-$CACHE_EVICTION"

for round in $(seq 1 "$ROUNDS"); do
  for side in A B; do
    if [ "$side" = A ]; then bin=$A label=$LABEL_A; else bin=$B label=$LABEL_B; fi
    append=1
    [ "$round" = 1 ] && append=0
    echo
    echo "#### round $round/$ROUNDS: $label"
    TANOD_BIN=$bin PERF_LABEL=$label PERF_TRIALS=1 PERF_APPEND=$append PERF_OUT=$OUT \
      ./bench/perf.sh "$@" || exit 1
  done
done

python3 bench/perf-compare.py "$OUT/$LABEL_A$SUFFIX" "$OUT/$LABEL_B$SUFFIX"
