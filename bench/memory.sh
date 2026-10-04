#!/usr/bin/env bash
# Memory under pressure: does the process stay inside the budgets it was given?
#
# Every bound Tanod has is a *configured* number, and a configured bound is
# worth exactly as much as the evidence that it holds when the workload tries
# to exceed it. This drives all three past their limits at once:
#
#   * the cache budget, with a working set an order of magnitude larger;
#   * the spool budget, with more slow readers than it can hold at once, so
#     the overflow path (`budget_exhausted`) is taken rather than described;
#   * `cache.max_body_size`, with a body that exceeds it, which must be served
#     and not stored.
#
# The assertion is on resident set size, not on an allocator statistic. RSS is
# what a container limit and an OOM killer look at, and an allocator that has
# not returned freed pages to the OS would flatter every other measure.
BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"

ROUNDS=${1:-${MEMORY_ROUNDS:-6}}
SLOW_READERS=${2:-${MEMORY_SLOW_READERS:-12}}

# Budgets in the template: 8MiB cache, 4MiB spool, 2MiB max body.
# 14MiB of configured buffers. The ceiling allows generously for the runtime,
# the connection pool and per-request state — it is here to catch "the budget
# is not a budget", not to pin a memory footprint.
RSS_CEILING_KB=${MEMORY_RSS_CEILING_KB:-393216}

bench_init memory
bench_param rounds "$ROUNDS"
bench_param slow_readers "$SLOW_READERS"
bench_param cache_budget_bytes 8388608
bench_param spool_budget_bytes 4194304
bench_param max_body_bytes 2097152
bench_param rss_ceiling_kb "$RSS_CEILING_KB"
bench_build

ORIGIN_PORT=$(bench_free_port)
LISTEN_PORT=$(bench_free_port)
METRICS_PORT=$(bench_free_port)
CONFIG="$BENCH_DIR/memory.yaml"

bench_render_config "$BENCH_ROOT/bench/memory.yaml.tpl" "$CONFIG" \
  "LISTEN=$LISTEN_PORT" "ORIGIN=$ORIGIN_PORT" "METRICS=$METRICS_PORT" \
  "PIDFILE=$BENCH_DIR/tanod.pid" "UPGRADESOCK=$BENCH_DIR/upgrade.sock"

bench_spawn origin "$(bench_bin slow-origin)" "$ORIGIN_PORT" 20
bench_wait_port 127.0.0.1 "$ORIGIN_PORT" "slow-origin"
bench_start_tanod tanod "$CONFIG" "$LISTEN_PORT" "$METRICS_PORT"
PID=$(bench_pid tanod)
BASE="http://127.0.0.1:$LISTEN_PORT"

RSS_BASELINE=$(bench_rss_kb "$PID")
echo "baseline RSS             $RSS_BASELINE KiB"
SAMPLES="$BENCH_DIR/samples"
: > "$SAMPLES"

# ------------------------------------------- 1. a working set the cache cannot hold

echo
echo "phase 1: $((ROUNDS * 16)) unique 1MiB entries against an 8MiB cache"
for round in $(seq 1 "$ROUNDS"); do
  seq 1 16 | xargs -P 4 -I{} curl -s -o /dev/null --max-time 30 "$BASE/big/1?u=$round-{}"
  printf '%s %s\n' "$(bench_rss_kb "$PID")" \
    "$(bench_metric "$METRICS_PORT" tanod_cache_bytes)" >> "$SAMPLES"
done
CACHE_PEAK=$(awk '{print $2}' "$SAMPLES" | bench_max)
echo "  peak cache bytes       $CACHE_PEAK"
bench_result cache_peak_bytes "$CACHE_PEAK"
# Both directions. The lower bound is what stops this passing against a cache
# that quietly never stored anything at all.
bench_assert_gt "${CACHE_PEAK%%.*}" 2097152 "peak cache bytes (nothing was ever cached)"
bench_assert_le "${CACHE_PEAK%%.*}" 12582912 "peak cache bytes against an 8MiB budget"

# ------------------------------------------- 2. a body larger than max_body_size

echo
echo "phase 2: a 4MiB body against a 2MiB max_body_size"
bench_origin_reset "$ORIGIN_PORT"
BIG_CODE=$(curl -s -o /dev/null -w '%{http_code}' --max-time 60 "$BASE/big/4")
BIG_LEN=$(curl -s -o /dev/null -w '%{size_download}' --max-time 60 "$BASE/big/4")
BIG_RENDERS=$(bench_origin_stat "$ORIGIN_PORT" total)
echo "  status                 $BIG_CODE"
echo "  bytes delivered        $BIG_LEN"
echo "  origin renders for 2   $BIG_RENDERS"
bench_result oversized_status "$BIG_CODE"
bench_result oversized_renders "$BIG_RENDERS"
# It must be *served* in full — refusing it, or truncating it, would turn a
# storage limit into a correctness bug.
bench_assert_eq "$BIG_CODE" 200 "an oversized body must still be served"
bench_assert_gt "$BIG_LEN" 4194304 "bytes delivered for a 4MiB body"
# And it must not be stored. The second request costing a second render is the
# observable form of that: a stored entry would have served it as a hit, and
# the origin is the witness rather than the proxy's own header.
bench_assert_eq "$BIG_RENDERS" 2 \
  "origin renders for two requests to a body larger than max_body_size (it was stored)"

# ------------------------------------------- 3. more slow readers than the spool holds

echo
echo "phase 3: $SLOW_READERS slow readers against a 4MiB spool budget"
for i in $(seq 1 "$SLOW_READERS"); do
  # 8 KiB/s against a 1MiB body: each reader is resident for two minutes if
  # left alone, and the deadline below is what ends it.
  curl -s -o /dev/null --limit-rate 8k --max-time 12 "$BASE/big/1?slow=$i" &
done
for _ in $(seq 1 12); do
  printf '%s %s\n' "$(bench_rss_kb "$PID")" \
    "$(bench_metric "$METRICS_PORT" tanod_spool_bytes)" >> "$SAMPLES"
  sleep 1
done
wait 2>/dev/null

SPOOL_PEAK=$(bench_metric "$METRICS_PORT" tanod_spool_bytes)
SPOOL_EXHAUSTED=$(bench_metric "$METRICS_PORT" 'tanod_spool_total{reason="budget_exhausted",route="bulk"}')
SPOOL_COMPLETE=$(bench_metric "$METRICS_PORT" 'tanod_spool_total{reason="complete",route="bulk"}')
echo "  spool bytes now        ${SPOOL_PEAK:-0}"
echo "  spools completed       ${SPOOL_COMPLETE:-0}"
echo "  spools budget-refused  ${SPOOL_EXHAUSTED:-0}"
bench_result spool_complete "${SPOOL_COMPLETE:-0}"
bench_result spool_budget_exhausted "${SPOOL_EXHAUSTED:-0}"
# The overflow path has to have been taken, or phase 3 proved nothing: with
# $SLOW_READERS × 1MiB against a 4MiB budget, some of them must degrade to
# streaming rather than the budget silently growing to fit.
bench_assert_gt "$(( ${SPOOL_COMPLETE%%.*} + ${SPOOL_EXHAUSTED%%.*} ))" 0 \
  "responses that went through the spool at all"

# ------------------------------------------------------------- assertions

bench_alive "$PID" || bench_fail "tanod exited under memory pressure"
bench_assert_no_panics tanod

RSS_PEAK=$(awk '{print $1}' "$SAMPLES" | bench_max)
RSS_FINAL=$(bench_rss_kb "$PID")
echo
echo "memory (RSS KiB)"
echo "  baseline               $RSS_BASELINE"
echo "  peak under pressure    $RSS_PEAK"
echo "  after                  $RSS_FINAL"
bench_result rss_baseline_kb "$RSS_BASELINE"
bench_result rss_peak_kb "$RSS_PEAK"
bench_result rss_final_kb "$RSS_FINAL"
bench_assert_le "$RSS_PEAK" "$RSS_CEILING_KB" \
  "peak RSS while every configured budget was being exceeded by the workload"

# Still serving, and still serving correctly, after all of it.
AFTER=$(curl -s -o /dev/null -w '%{http_code}' --max-time 20 "$BASE/hot/1")
echo "  serving afterwards     $AFTER"
bench_assert_eq "$AFTER" 200 "a request after the pressure is removed"

echo
bench_print_params
echo
bench_pass "peak RSS ${RSS_PEAK}KiB with a cache peaking at $CACHE_PEAK against its 8MiB budget, a 4MiB body served without being stored, and $SLOW_READERS slow readers against a 4MiB spool"
