#!/usr/bin/env bash
# What a cache hit costs, and therefore what moving the cache out of process
# would cost. The input to docs/CACHE-STORAGE-EVALUATION.md.
#
# Three numbers, all medians over a keepalive connection so that per-request
# TCP setup is not counted as cache latency:
#
#   miss   client -> Tanod -> origin render -> client
#   hit    client -> Tanod -> in-process lookup -> client
#
# The hit figure is the one that decides the evaluation. It already contains
# one loopback round trip, so it bounds what a *second* round trip to an
# external store would add: an out-of-process cache cannot be cheaper than the
# loopback component of this number, and that component is most of it.
BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"

SAMPLES=${1:-60}
RENDER_MS=${2:-200}
BODY_MIB=${3:-1}

bench_init storage
bench_param samples "$SAMPLES"
bench_param render_ms "$RENDER_MS"
bench_param body_mib "$BODY_MIB"
bench_build

ORIGIN_PORT=$(bench_free_port)
LISTEN_PORT=$(bench_free_port)
METRICS_PORT=$(bench_free_port)
CONFIG="$BENCH_DIR/storage.yaml"
bench_render_config "$BENCH_ROOT/bench/storage.yaml" "$CONFIG" \
  "LISTEN=$LISTEN_PORT" "ORIGIN=$ORIGIN_PORT" "METRICS=$METRICS_PORT"

bench_spawn origin "$(bench_bin slow-origin)" "$ORIGIN_PORT" "$RENDER_MS"
bench_wait_port 127.0.0.1 "$ORIGIN_PORT" "slow-origin"
bench_start_tanod tanod "$CONFIG" "$LISTEN_PORT" "$METRICS_PORT"

# One curl invocation, many URLs: the connection is reused after the first, so
# only the first sample pays for the TCP handshake and it is dropped. `-o` has
# to be repeated per URL — a single one applies to the first transfer only, and
# the remaining bodies would otherwise land in the timings.
timed_urls() { # url...
  local args="" u
  for u in "$@"; do args="$args -o /dev/null $u"; done
  # shellcheck disable=SC2086
  curl -s -w '%{time_total}\n' $args 2>/dev/null | tail -n +2
}

# ---- miss: every URL distinct, so every one renders
MISS_URLS=""
for i in $(seq 0 12); do
  MISS_URLS="$MISS_URLS http://127.0.0.1:$LISTEN_PORT/big/$BODY_MIB?miss=$i-$RANDOM"
done
# shellcheck disable=SC2086
MED_MISS=$(timed_urls $MISS_URLS | sed '/^$/d' | bench_median)

# ---- hit: one URL, warmed, then measured
WARM_URL="http://127.0.0.1:$LISTEN_PORT/big/$BODY_MIB?hit=1"
curl -s -o /dev/null "$WARM_URL"
STATUS=$(curl -s -o /dev/null -D - "$WARM_URL" | tr -d '\r' | sed -n 's/^[Xx]-[Tt]anod: //p')
[ "$STATUS" = "HIT" ] || bench_fail "the measured URL was $STATUS, not HIT; there is no hit path to measure"
HIT_URLS=""
for i in $(seq 0 "$SAMPLES"); do HIT_URLS="$HIT_URLS $WARM_URL"; done
# shellcheck disable=SC2086
MED_HIT=$(timed_urls $HIT_URLS | sed '/^$/d' | bench_median)

# ---- the same, on a body small enough that transfer is not the cost.
#
# This is the number the storage evaluation actually turns on: with the body
# out of the way, what is left is one loopback round trip plus an in-process
# map lookup. An out-of-process cache would add a second round trip of the
# same order to every hit, before it serialised a single byte.
SMALL_URL="http://127.0.0.1:$LISTEN_PORT/small-page?hit=1"
curl -s -o /dev/null "$SMALL_URL"
SMALL_URLS=""
for i in $(seq 0 "$SAMPLES"); do SMALL_URLS="$SMALL_URLS $SMALL_URL"; done
# shellcheck disable=SC2086
MED_SMALL=$(timed_urls $SMALL_URLS | sed '/^$/d' | bench_median)

RATIO=$(awk -v m="$MED_MISS" -v h="$MED_HIT" 'BEGIN { printf "%.1f", (h > 0 ? m / h : 0) }')

echo
echo "  median miss  (origin render + ${BODY_MIB}MiB body)  ${MED_MISS}s"
echo "  median hit   (in-process lookup + same body)  ${MED_HIT}s"
echo "  median hit   (small body: round trip + lookup) ${MED_SMALL}s"
echo "  hit is ${RATIO}x faster than a miss"
echo
bench_print_params
echo

bench_result median_miss_seconds "$MED_MISS"
bench_result median_hit_seconds "$MED_HIT"
bench_result median_small_hit_seconds "$MED_SMALL"
bench_result hit_speedup "$RATIO"

# The cache has to be worth having at all before its storage is worth
# discussing. With a 200ms render in front, a hit that is not several times
# faster means something else dominates and these numbers say nothing.
bench_lt_float "$MED_HIT" "$(awk -v m="$MED_MISS" 'BEGIN{print m/3}')" \
  || bench_fail "median hit ${MED_HIT}s is not meaningfully faster than a ${MED_MISS}s miss"
# The claim the evaluation rests on: a hit is sub-millisecond-ish, so a second
# network round trip is not a rounding error on it.
bench_lt_float "$MED_HIT" 0.05 \
  || bench_fail "median hit ${MED_HIT}s is far slower than the evaluation assumes"
bench_assert_no_panics tanod
bench_lt_float "$MED_SMALL" 0.01 \
  || bench_fail "a small-body hit took ${MED_SMALL}s; the evaluation assumes a round trip plus a map lookup, not this"
bench_pass "median cache hit ${MED_HIT}s against a ${MED_MISS}s miss (${RATIO}x); a small-body hit is ${MED_SMALL}s, which is one loopback round trip plus a map lookup — the cost an external store would add again on every hit"
