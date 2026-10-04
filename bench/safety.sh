#!/usr/bin/env bash
# The check that matters more than any throughput number.
#
# Same route, same permissive config that collapsed 100 requests into one
# render. But this path answers with Set-Cookie, so every request must get its
# own render — sharing one would hand one visitor's session to ninety-nine
# strangers.
BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"

CONCURRENCY=${1:-50}

bench_init set_cookie_safety
bench_param concurrency "$CONCURRENCY"
bench_param render_ms 200
bench_build

ORIGIN_PORT=$(bench_free_port)
LISTEN_PORT=$(bench_free_port)
METRICS_PORT=$(bench_free_port)
CONFIG="$BENCH_DIR/coalesce.yaml"
bench_render_config "$BENCH_ROOT/bench/coalesce.yaml" "$CONFIG" \
  "LISTEN=$LISTEN_PORT" "ORIGIN=$ORIGIN_PORT" "METRICS=$METRICS_PORT"

bench_spawn origin "$(bench_bin slow-origin)" "$ORIGIN_PORT" 200
bench_wait_port 127.0.0.1 "$ORIGIN_PORT" "slow-origin"
bench_start_tanod tanod "$CONFIG" "$LISTEN_PORT" "$METRICS_PORT"
bench_origin_reset "$ORIGIN_PORT"

echo "$CONCURRENCY concurrent requests to a Set-Cookie route,"
echo "on the same config that collapses 100 requests into 1 render"
echo
OUT=$(seq 1 "$CONCURRENCY" | xargs -P "$CONCURRENCY" -I{} \
  curl -s -o /dev/null -D - --max-time 30 \
  "http://127.0.0.1:$LISTEN_PORT/private/account" 2>/dev/null)

COOKIES=$(echo "$OUT" | grep -i '^set-cookie:' | sed 's/.*session=//' | tr -d '\r; ' | sort -u | wc -l | tr -d ' ')
SERVED=$(echo "$OUT" | grep -ci '^HTTP/1.1 200')
RENDERS=$(bench_origin_stat "$ORIGIN_PORT" total)
echo "  requests served        $SERVED / $CONCURRENCY"
echo "  distinct session ids   $COOKIES"
echo "  origin renders         $RENDERS"
echo
echo "  X-Tanod breakdown:"
echo "$OUT" | grep -i '^x-tanod:' | tr -d '\r' | awk '{print "    " $2}' | sort | uniq -c
echo
bench_print_params
echo

bench_result served "$SERVED"
bench_result distinct_sessions "$COOKIES"
bench_result origin_renders "$RENDERS"

bench_assert_eq "$SERVED" "$CONCURRENCY" "requests served"
bench_assert_eq "$COOKIES" "$CONCURRENCY" "distinct session ids (a response was shared between clients)"
# The origin must have been asked to render every one of them. A lower count
# would mean a Set-Cookie response was served from the cache.
bench_assert_eq "$RENDERS" "$CONCURRENCY" "origin renders"
bench_pass "every request got its own session; nothing was shared"
