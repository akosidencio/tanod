#!/usr/bin/env bash
# Sustained mixed traffic, watched for the failures that only appear with time.
#
# A short benchmark answers "does the mechanism work". This answers "does it
# still work after an hour", which is a different question and the one that
# decides whether something is safe to leave running. Three failures are only
# visible here:
#
#   * a slow leak — memory that grows with cumulative requests rather than with
#     concurrent ones, which every short test passes;
#   * a permit leak — capacity that is never returned on some rare path, which
#     shows up as in-flight ratcheting upward and eventually as a proxy that
#     sheds everything while the origin sits idle;
#   * an eviction bug — a cache that exceeds its byte budget once the working
#     set outgrows it, which a short run never reaches.
#
# The traffic is deliberately mixed rather than uniform. A single repeated URL
# would sit in the cache and exercise almost nothing; the mix here keeps the
# cache filling and evicting, keeps admission genuinely contended, and includes
# the paths that historically leaked: client disconnects, oversized bodies, and
# responses the policy refuses to share.
#
# Default is short enough for CI. `SOAK_SECONDS=3600 ./bench/soak.sh` is the
# release gate — see docs/RELEASE-GATES.md.
BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"

SECONDS_TO_RUN=${1:-${SOAK_SECONDS:-60}}
WORKERS=${2:-${SOAK_WORKERS:-12}}
# RSS ceiling. Generous on purpose: this is a leak detector, not a memory
# budget assertion — the cache budget below is that. A leak crosses it; normal
# operation is not close.
RSS_CEILING_KB=${SOAK_RSS_CEILING_KB:-786432}

bench_init soak
bench_param seconds "$SECONDS_TO_RUN"
bench_param workers "$WORKERS"
bench_param render_ms 25
bench_param cache_budget_bytes 16777216
bench_param rss_ceiling_kb "$RSS_CEILING_KB"
bench_build

ORIGIN_PORT=$(bench_free_port)
LISTEN_PORT=$(bench_free_port)
ADMIN_PORT=$(bench_free_port)
METRICS_PORT=$(bench_free_port)
CONFIG="$BENCH_DIR/soak.yaml"

bench_render_config "$BENCH_ROOT/bench/soak.yaml.tpl" "$CONFIG" \
  "LISTEN=$LISTEN_PORT" "ORIGIN=$ORIGIN_PORT" "ADMIN=$ADMIN_PORT" \
  "METRICS=$METRICS_PORT" "PIDFILE=$BENCH_DIR/tanod.pid" \
  "UPGRADESOCK=$BENCH_DIR/upgrade.sock"

bench_spawn origin "$(bench_bin slow-origin)" "$ORIGIN_PORT" 25
bench_wait_port 127.0.0.1 "$ORIGIN_PORT" "slow-origin"
bench_start_tanod tanod "$CONFIG" "$LISTEN_PORT" "$ADMIN_PORT"
PID=$(bench_pid tanod)

BASE="http://127.0.0.1:$LISTEN_PORT"

# One worker's request mix. Each arm is here because it is a path that has
# leaked something in this codebase or in the dependency before.
worker() { # id, end-epoch, results-file
  local id=$1 end=$2 out=$3 n=0
  while [ "$(date +%s)" -lt "$end" ]; do
    n=$((n + 1))
    case $((n % 6)) in
      # A small hot set: cache hits and coalesced followers, which must never
      # consume a permit.
      0|1) curl -s -o /dev/null -w '%{http_code}\n' --max-time 20 "$BASE/hot/$((n % 4))" ;;
      # Unique keys, small bodies: pure admission pressure, nothing reusable.
      2)   curl -s -o /dev/null -w '%{http_code}\n' --max-time 20 "$BASE/cold/$id-$n" ;;
      # Unique keys, 1MiB bodies. `/big/1` is the size; the query string is
      # what varies the cache key. Sixteen of these fill the 16MiB budget, so
      # from then on the run is exercising eviction rather than filling.
      3)   curl -s -o /dev/null -w '%{http_code}\n' --max-time 20 "$BASE/big/1?u=$id-$n" ;;
      # A response the policy must refuse to share, every time.
      4)   curl -s -o /dev/null -w '%{http_code}\n' --max-time 20 "$BASE/private/$id-$n" ;;
      # Hang up part-way through a 1MiB body. Historically the path that leaked
      # a permit, because upstream end-of-stream never arrives on it.
      #
      # One marker line, not curl's status code: a timed-out curl prints `000`
      # *and* exits non-zero, so writing both would record every deliberate
      # disconnect twice — once as a marker and once as an unexplained failure.
      5)   if curl -s -o /dev/null --max-time 0.05 "$BASE/big/1?abort=$id-$n" >/dev/null 2>&1; then
             echo "abort-raced"
           else
             echo "aborted"
           fi ;;
    esac >> "$out" 2>/dev/null
  done
}

END=$(( $(date +%s) + SECONDS_TO_RUN ))
echo "soaking for ${SECONDS_TO_RUN}s with $WORKERS workers"
for w in $(seq 1 "$WORKERS"); do
  worker "$w" "$END" "$BENCH_DIR/results-$w" &
done

# Sample while it runs. The samples are the evidence: a single reading at the
# end cannot tell a leak from a large working set.
SAMPLES="$BENCH_DIR/samples"
: > "$SAMPLES"
while [ "$(date +%s)" -lt "$END" ]; do
  printf '%s %s %s %s\n' \
    "$(date +%s)" \
    "$(bench_rss_kb "$PID")" \
    "$(bench_metric "$METRICS_PORT" tanod_cache_bytes)" \
    "$(bench_metric "$METRICS_PORT" 'tanod_origin_in_flight{limiter="global"}')" \
    >> "$SAMPLES"
  sleep 2
done
wait

# ------------------------------------------------------------- assertions

bench_alive "$PID" || bench_fail "tanod exited during the soak"
bench_assert_no_panics tanod

TOTAL=$(cat "$BENCH_DIR"/results-* 2>/dev/null | wc -l | tr -d ' ')
OK=$(cat "$BENCH_DIR"/results-* 2>/dev/null | grep -c '^200$' || true)
SHED=$(cat "$BENCH_DIR"/results-* 2>/dev/null | grep -c '^503$' || true)
ABORTED=$(cat "$BENCH_DIR"/results-* 2>/dev/null | grep -c '^aborted$' || true)
# The same arm, on the occasions the whole 1MiB body arrived inside the 50ms
# deadline. Deliberate, and not evidence of the disconnect path being taken.
RACED=$(cat "$BENCH_DIR"/results-* 2>/dev/null | grep -c '^abort-raced$' || true)
OTHER=$((TOTAL - OK - SHED - ABORTED - RACED))

echo
echo "traffic"
echo "  requests               $TOTAL"
echo "  200                    $OK"
echo "  503 (shed, by design)  $SHED"
echo "  aborted (by design)    $ABORTED"
echo "  abort arm raced        $RACED"
echo "  anything else          $OTHER"
bench_result requests "$TOTAL"
bench_result ok "$OK"
bench_result shed "$SHED"
bench_result unexpected "$OTHER"
bench_assert_gt "$TOTAL" 100 "requests issued (too few to soak anything)"
# The disconnect arm has to actually disconnect, or the path this soak exists
# to watch is never taken.
bench_assert_gt "$ABORTED" 0 "requests aborted mid-body (the disconnect path was never exercised)"
# 503 is a correct answer under a ceiling; a 502 or a 000 is not.
bench_assert_eq "$OTHER" 0 "responses that were neither served, shed, nor deliberately aborted"

# ---- memory: compare the first quarter of the run against the last

RSS_FIRST=$(awk 'NR<=NR_END {print $2}' NR_END=99999 "$SAMPLES" | head -n "$(( $(wc -l < "$SAMPLES") / 4 + 1 ))" | bench_median)
RSS_LAST=$(awk '{print $2}' "$SAMPLES" | tail -n "$(( $(wc -l < "$SAMPLES") / 4 + 1 ))" | bench_median)
RSS_MAX=$(awk '{print $2}' "$SAMPLES" | bench_max)
echo
echo "memory (RSS KiB)"
echo "  first quarter, median  $RSS_FIRST"
echo "  last quarter, median   $RSS_LAST"
echo "  peak                   $RSS_MAX"
bench_result rss_first_kb "$RSS_FIRST"
bench_result rss_last_kb "$RSS_LAST"
bench_result rss_peak_kb "$RSS_MAX"
bench_assert_le "$RSS_MAX" "$RSS_CEILING_KB" "peak RSS"

# Growth between the two halves is the leak signal. A generous multiplier,
# because a cache legitimately fills during the first quarter and the working
# set here deliberately outgrows the budget — but a leak keeps going, and over
# a long soak it goes far past this.
GROWTH_LIMIT=$(( RSS_FIRST * 2 + 65536 ))
echo "  growth allowance       $GROWTH_LIMIT"
bench_assert_le "$RSS_LAST" "$GROWTH_LIMIT" \
  "RSS at the end of the soak (memory grew with cumulative requests, which is a leak)"

# ---- the cache stayed inside its byte budget the whole time

CACHE_MAX=$(awk '{print $3}' "$SAMPLES" | bench_max)
echo
echo "cache"
echo "  peak bytes             $CACHE_MAX"
echo "  budget                 16777216"
bench_result cache_peak_bytes "$CACHE_MAX"
# Two-sided, and the lower bound is the important half. An upper bound alone
# passes trivially against a cache that never filled — which is exactly what
# an earlier version of this script did, proving nothing about eviction. The
# working set here is sized to outgrow the budget, so a peak below a quarter
# of it means the traffic mix stopped exercising the path.
bench_assert_gt "${CACHE_MAX%%.*}" 4194304 \
  "peak cache bytes (the cache never filled, so eviction was never exercised)"
# And eviction kept it inside the budget. The allowance covers fills in
# progress, which are counted against the budget before they are admitted.
bench_assert_le "${CACHE_MAX%%.*}" 25165824 "peak cache bytes against a 16MiB budget"

# The invariant the whole product rests on, asserted against the origin's own
# count rather than the proxy's.
ORIGIN_PEAK=$(bench_origin_stat "$ORIGIN_PORT" peak)
echo "  origin peak concurrency $ORIGIN_PEAK (ceiling 8)"
bench_result origin_peak "$ORIGIN_PEAK"
bench_assert_le "$ORIGIN_PEAK" 8 "origin peak concurrency over the whole soak"
bench_assert_gt "$ORIGIN_PEAK" 1 "origin peak concurrency (nothing ever overlapped, so nothing was contended)"

# ---- no permit leaked

IN_FLIGHT=$(bench_metric "$METRICS_PORT" 'tanod_origin_in_flight{limiter="global"}')
READY=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 "http://127.0.0.1:$ADMIN_PORT/health/ready")
echo
echo "after the traffic stops"
echo "  in-flight permits      ${IN_FLIGHT:-?}"
echo "  readiness              $READY"
bench_result in_flight_at_end "${IN_FLIGHT:-?}"
# The clearest leak signal there is: with nothing in flight, a non-zero here
# means capacity that will never come back.
bench_assert_eq "${IN_FLIGHT%%.*}" 0 "origin permits still held after all traffic stopped"
bench_assert_eq "$READY" 200 "readiness after the soak"

echo
bench_print_params
echo
bench_pass "$TOTAL requests over ${SECONDS_TO_RUN}s: RSS ${RSS_FIRST}→${RSS_LAST} KiB (peak $RSS_MAX), cache peaked at $CACHE_MAX within its 16MiB budget, no permit left held, no panic"
