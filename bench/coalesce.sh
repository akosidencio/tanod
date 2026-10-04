#!/usr/bin/env bash
# N concurrent requests for the SAME url. The origin counts how many renders it
# was actually asked to perform.
BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"

CONCURRENCY=${1:-100}
RENDER_MS=${2:-1000}

bench_init coalescing
bench_param concurrency "$CONCURRENCY"
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

echo "$CONCURRENCY concurrent requests for ONE url, ${RENDER_MS}ms render"
echo
OUT=$(seq 1 "$CONCURRENCY" | xargs -P "$CONCURRENCY" -I{} \
  curl -s -o /dev/null -D - --max-time 30 \
  "http://127.0.0.1:$LISTEN_PORT/product/iphone" 2>/dev/null)

# Counted by the origin, not scraped from a response header: a coalesced
# response carries the *leader's* headers, so header-derived counts measure
# what was replayed rather than what was rendered.
RENDERS=$(bench_origin_stat "$ORIGIN_PORT" total)
STATUSES=$(echo "$OUT" | grep -ci '^HTTP/1.1 200')

echo "  requests served        $STATUSES / $CONCURRENCY"
echo "  origin renders         ${RENDERS:-?}"
echo
echo "  X-Tanod breakdown:"
echo "$OUT" | grep -i '^x-tanod:' | tr -d '\r' | awk '{print "    " $2}' | sort | uniq -c
echo
bench_print_params
echo

bench_result served "$STATUSES"
bench_result origin_renders "$RENDERS"

bench_assert_eq "$STATUSES" "$CONCURRENCY" "requests served"
# Two renders, not one: the leader can finish and its entry expire while the
# tail of the burst is still arriving. Three would mean collapsing failed.
bench_assert_le "$RENDERS" 2 "origin renders"
bench_assert_gt "$RENDERS" 0 "origin renders (nothing reached the origin at all)"
bench_pass "$CONCURRENCY requests collapsed onto $RENDERS origin render(s)"
