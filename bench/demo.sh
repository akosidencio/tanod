#!/usr/bin/env bash
# The headline demonstration: unique URLs, nothing cacheable, nothing to
# coalesce. Only admission control is doing any work.
#
# Fires the same load twice — straight at the origin, then through tanod —
# and reads the peak concurrency the origin itself observed. The origin is the
# witness, so the result does not depend on trusting the proxy's own metrics.
BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"

CONCURRENCY=${1:-100}
RENDER_MS=${2:-1000}

bench_init admission
bench_param concurrency "$CONCURRENCY"
bench_param render_ms "$RENDER_MS"
bench_build

ORIGIN_PORT=$(bench_free_port)
LISTEN_PORT=$(bench_free_port)
CONFIG="$BENCH_DIR/demo.yaml"
bench_render_config "$BENCH_ROOT/bench/demo.yaml" "$CONFIG" \
  "LISTEN=$LISTEN_PORT" "ORIGIN=$ORIGIN_PORT"

CEILING=$(awk '/^  concurrency:/{f=1} f&&/max:/{print $2; exit}' "$CONFIG")
bench_assert_int "$CEILING" "configured ceiling"
bench_param ceiling "$CEILING"

# The load generator. Peak concurrency is read back from the origin's own
# counter rather than from a response header, which a cache could replay.
fire() { # base url
  seq 1 "$CONCURRENCY" | xargs -P "$CONCURRENCY" -I{} \
    curl -s -o /dev/null --max-time 25 "$1/product/{}" >/dev/null 2>&1
}

run_case() { # label, base url, result key, through_proxy
  bench_spawn origin "$(bench_bin slow-origin)" "$ORIGIN_PORT" "$RENDER_MS"
  bench_wait_port 127.0.0.1 "$ORIGIN_PORT" "slow-origin"
  if [ "$4" = yes ]; then
    bench_start_tanod tanod "$CONFIG" "$LISTEN_PORT"
  fi

  local start peak elapsed
  start=$(date +%s)
  fire "$2"
  elapsed=$(( $(date +%s) - start ))
  peak=$(bench_origin_stat "$ORIGIN_PORT" peak)
  bench_assert_int "$peak" "$1: origin peak"

  [ "$4" = yes ] && bench_stop tanod
  bench_stop origin
  printf '  %-22s peak=%-6s wall=%ss\n' "$1" "$peak" "$elapsed"
  bench_result "$3" "$peak"
  bench_result "${3}_wall_s" "$elapsed"
}

echo "$CONCURRENCY concurrent requests, $CONCURRENCY unique URLs, ${RENDER_MS}ms render"
echo "nothing cacheable, nothing coalescible — admission control only"
echo
run_case "direct to origin" "http://127.0.0.1:$ORIGIN_PORT" direct   no
run_case "through tanod"  "http://127.0.0.1:$LISTEN_PORT" governed yes
echo
DIRECT=$(bench_get BENCH_RESULT_direct)
GOVERNED=$(bench_get BENCH_RESULT_governed)
echo "  configured ceiling     $CEILING"
echo "  origin peak, direct    $DIRECT"
echo "  origin peak, tanod   $GOVERNED"
echo
bench_print_params
echo

# Both halves are asserted. A governed peak under the ceiling proves nothing on
# its own if the load never pushed past the ceiling in the first place, so the
# unprotected run has to demonstrate that it could.
bench_assert_gt "$DIRECT" "$CEILING" "unprotected origin peak (load never exceeded the ceiling, so the test proved nothing)"
bench_assert_le "$GOVERNED" "$CEILING" "governed origin peak"
bench_pass "unprotected, the origin was driven to $DIRECT concurrent renders; through tanod, $GOVERNED"
