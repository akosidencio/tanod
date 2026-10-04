#!/usr/bin/env bash
# Does request collapsing destroy streaming?
#
# A leader that streams a shell immediately and finishes 1.6s later must not
# make every waiter wait 1.6s for its first byte. If waiters only get the
# response once the leader completes, coalescing turns a 1ms TTFB into a 1.6s
# one for everybody but the leader — the component that exists to make things
# faster making them dramatically slower.
BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"

CONCURRENCY=${1:-20}
CHUNKS=${2:-5}
RENDER_MS=${3:-400}

bench_init streaming_coalescing
bench_param concurrency "$CONCURRENCY"
bench_param chunks "$CHUNKS"
bench_param render_ms "$RENDER_MS"
bench_build

ORIGIN_PORT=$(bench_free_port)
LISTEN_PORT=$(bench_free_port)
METRICS_PORT=$(bench_free_port)
CONFIG="$BENCH_DIR/coalesce.yaml"
bench_render_config "$BENCH_ROOT/bench/coalesce.yaml" "$CONFIG" \
  "LISTEN=$LISTEN_PORT" "ORIGIN=$ORIGIN_PORT" "METRICS=$METRICS_PORT"

bench_spawn origin "$(bench_bin slow-origin)" "$ORIGIN_PORT" "$RENDER_MS"
bench_wait_port 127.0.0.1 "$ORIGIN_PORT" "slow-origin"
bench_start_tanod tanod "$CONFIG" "$LISTEN_PORT" "$METRICS_PORT"
bench_origin_reset "$ORIGIN_PORT"

echo "$CONCURRENCY concurrent requests for one streaming url"
echo "($CHUNKS chunks, ${RENDER_MS}ms apart — the origin takes ~$(( (CHUNKS-1) * RENDER_MS ))ms to finish)"
echo

OUT=$(seq 1 "$CONCURRENCY" | xargs -P "$CONCURRENCY" -I{} \
  curl -s -o /dev/null --max-time 30 \
  -w '%{time_starttransfer} %{time_total} %{http_code}\n' \
  "http://127.0.0.1:$LISTEN_PORT/stream/$CHUNKS" 2>/dev/null)

# The render count comes from the origin's own counter. It used to be derived
# by grepping the *proxy's* access log for upstream lines, which measured the
# component under test with an expression that silently returned 0 whenever the
# log format changed.
RENDERS=$(bench_origin_stat "$ORIGIN_PORT" total)
SERVED=$(echo "$OUT" | grep -c ' 200$')

echo "$OUT" | sort -n | awk '
  { ttfb[NR]=$1; total[NR]=$2 }
  END {
    printf "  requests served       %d\n", NR
    printf "  median TTFB           %.3fs\n", ttfb[int(NR/2)+1]
    printf "  max TTFB              %.3fs\n", ttfb[NR]
    printf "  median total          %.3fs\n", total[int(NR/2)+1]
  }'
echo "  origin renders        $RENDERS"
echo

MAXTTFB=$(echo "$OUT" | awk '{print $1}' | bench_max)
MEDTOTAL=$(echo "$OUT" | awk '{print $2}' | bench_median)

bench_print_params
echo

bench_result served "$SERVED"
bench_result origin_renders "$RENDERS"
bench_result max_ttfb_s "$MAXTTFB"
bench_result median_total_s "$MEDTOTAL"

# Both halves must hold. Checking only TTFB would pass trivially when no
# collapsing happened at all, because every request then has its own origin
# connection and of course gets the shell immediately.
bench_assert_eq "$SERVED" "$CONCURRENCY" "requests served"
bench_assert_le "$RENDERS" 2 "origin renders (requests were not collapsed)"
bench_assert_gt "$RENDERS" 0 "origin renders (nothing reached the origin at all)"
bench_lt_float "$MAXTTFB" "$(awk -v t="$MEDTOTAL" 'BEGIN{print t/2}')" || bench_fail \
  "max TTFB ${MAXTTFB}s against a median total of ${MEDTOTAL}s — waiters were served only after the leader finished; coalescing buffered the response"
bench_pass "$SERVED requests, $RENDERS origin render — waiters received the shell while the leader was still rendering (max TTFB ${MAXTTFB}s of ${MEDTOTAL}s)"
