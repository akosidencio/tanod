#!/usr/bin/env bash
# Does a slow reader still hold a render permit after the origin has finished?
#
# This is the benchmark for the gap that has been open since the project
# started, recorded in the README as "slow readers and render capacity". A
# permit is meant to model render capacity, but Pingora paces upstream reads
# against downstream writes, so without a spool the permit is released when the
# *client* finishes reading rather than when the origin finishes rendering.
#
# The shape of the test:
#
#   * ceiling of 2 concurrent origin requests
#   * two clients rate-limited to 32 KB/s each ask for a 8 MiB page
#   * once the origin reports it has finished rendering both of them, a third,
#     ordinary request asks for capacity
#
# With the spool off, the third request waits behind readers that are no longer
# consuming anything. With it on, capacity is back the moment the origin is
# done. Both directions are measured in one run, because a benchmark that only
# runs the fixed configuration cannot tell a fix from a coincidence — the whole
# point of the first phase of this roadmap.
#
# `timeouts.downstream_write` is set to 60s so that it cannot be what returns
# the capacity. If this passes, it passed because the origin finished.
BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"

SIZE_MIB=${1:-8}
RATE=${2:-32k}
RENDER_MS=${3:-200}

bench_init spool
bench_param body_mib "$SIZE_MIB"
bench_param reader_rate "$RATE"
bench_param render_ms "$RENDER_MS"
bench_param ceiling 2
bench_param readers 2
bench_param downstream_write 60s
bench_build

ORIGIN_PORT=$(bench_start_origin origin "$RENDER_MS")

# Run the whole scenario once, with the spool on or off, and leave the probe's
# status and latency in PROBE_CODE / PROBE_MS.
scenario() { # spool(true|false), max_body, label
  local spool=$1 max_body=$2 label=$3
  local listen
  listen=$(bench_free_port)

  bench_render_config "$BENCH_ROOT/bench/spool.yaml" "$BENCH_DIR/$label.yaml" \
    "LISTEN=$listen" "ORIGIN=$ORIGIN_PORT" "SPOOL=$spool" "SPOOL_MAX_BODY=$max_body"

  bench_stop tanod
  bench_start_tanod tanod "$BENCH_DIR/$label.yaml" "$listen"
  bench_origin_reset "$ORIGIN_PORT"

  # Two slow readers take both permits. Distinct URLs so nothing is collapsed
  # or reused — this must measure admission, not the cache.
  bench_spawn "reader1_$label" curl -s -o /dev/null --limit-rate "$RATE" \
    "http://127.0.0.1:$listen/rendered/a-$label/$SIZE_MIB"
  bench_spawn "reader2_$label" curl -s -o /dev/null --limit-rate "$RATE" \
    "http://127.0.0.1:$listen/rendered/b-$label/$SIZE_MIB"

  # Wait until the *origin* says it has finished both renders. This is the
  # witness the whole benchmark turns on, and it is the origin's own counter
  # rather than anything the proxy reports about itself.
  local in_flight total
  for _ in $(seq 1 200); do
    total=$(bench_origin_stat "$ORIGIN_PORT" total)
    in_flight=$(bench_origin_stat "$ORIGIN_PORT" in_flight)
    if [ "${total:-0}" -ge 2 ] && [ "${in_flight:-9}" -eq 0 ]; then break; fi
    sleep 0.1
  done
  bench_assert_eq "${in_flight:-99}" 0 "origin still rendering when the probe was sent ($label)"
  bench_assert_gt "${total:-0}" 1 "origin never received both slow reads ($label)"

  # The readers are still reading; the origin is not still rendering. Anything
  # holding a permit now is holding it for the client's benefit, not the
  # origin's.
  local start
  start=$(date +%s%N)
  PROBE_CODE=$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 \
    "http://127.0.0.1:$listen/quick-$label")
  PROBE_MS=$(( ($(date +%s%N) - start) / 1000000 ))

  bench_stop "reader1_$label"
  bench_stop "reader2_$label"
  printf '  %-24s %-8s %-10s\n' "$label" "$PROBE_CODE" "${PROBE_MS}ms"
}

echo "ceiling = 2, two readers at $RATE pulling ${SIZE_MIB}MiB, probe after the origin finished"
echo
printf '  %-24s %-8s %-10s\n' "configuration" "status" "probe latency"

# 1. The old behaviour, kept in the run so the fix has something to be
#    measured against. This is what the README described as the open gap.
scenario false 2MiB "spool-off"
OFF_CODE=$PROBE_CODE
OFF_MS=$PROBE_MS
bench_result spool_off_status "$OFF_CODE"
bench_result spool_off_ms "$OFF_MS"

# 2. The spool, sized to hold the body.
scenario true "$((SIZE_MIB * 2))MiB" "spool-on"
ON_CODE=$PROBE_CODE
ON_MS=$PROBE_MS
bench_result spool_on_status "$ON_CODE"
bench_result spool_on_ms "$ON_MS"

# 3. A body far larger than `spool.max_body`, which must degrade to the old
#    behaviour rather than truncate, error, or buffer without limit.
scenario true 64KiB "spool-too-small"
SMALL_CODE=$PROBE_CODE
bench_result spool_overflow_status "$SMALL_CODE"

echo
bench_print_params
echo

# --------------------------------------------------------------- assertions

# The fix itself. `-lt 1000` rather than `= 200`: a slow probe that eventually
# succeeds is still a probe that queued behind capacity nobody was using.
[ "$ON_CODE" = "200" ] || bench_fail \
  "with the spool enabled the probe got HTTP $ON_CODE — capacity did not come back after the origin finished"
bench_assert_le "$ON_MS" 1000 \
  "probe latency with the spool enabled (the origin had already finished)"

# The control. If this also passes, the benchmark proves nothing: either the
# body was small enough to fit in the socket buffers, or the machine is fast
# enough that the readers finished too. Failing loudly is the point — three of
# the tests written in the previous phase were worthless until they were run
# against the unfixed code.
if [ "$OFF_CODE" = "200" ] && [ "$OFF_MS" -lt 1000 ]; then
  bench_fail "the control case also returned capacity in ${OFF_MS}ms, so this run
  did not reproduce the problem the spool exists to fix and cannot be evidence
  that it was fixed. Raise the body size (arg 1, currently ${SIZE_MIB}MiB) or
  lower the reader rate (arg 2, currently $RATE)."
fi

# Degrading, not failing: a response too large for the spool must still be
# served correctly.
[ "$SMALL_CODE" = "200" ] || [ "$SMALL_CODE" = "503" ] || bench_fail \
  "a body larger than spool.max_body produced HTTP $SMALL_CODE; overflowing the spool must degrade to streaming, not break the response"

# And the bytes must be intact. A buffer that reorders or drops on overflow
# would pass every latency assertion above while corrupting the page.
LISTEN_CHECK=$(bench_free_port)
bench_render_config "$BENCH_ROOT/bench/spool.yaml" "$BENCH_DIR/integrity.yaml" \
  "LISTEN=$LISTEN_CHECK" "ORIGIN=$ORIGIN_PORT" "SPOOL=true" "SPOOL_MAX_BODY=64KiB"
bench_stop tanod
bench_start_tanod tanod "$BENCH_DIR/integrity.yaml" "$LISTEN_CHECK"
DIRECT=$(curl -s "http://127.0.0.1:$ORIGIN_PORT/big/integrity/1" | cksum)
THROUGH=$(curl -s "http://127.0.0.1:$LISTEN_CHECK/big/integrity/1" | cksum)
[ "$DIRECT" = "$THROUGH" ] || bench_fail \
  "a body that overflowed the spool came out different from the origin's: origin=$DIRECT proxy=$THROUGH"
bench_result integrity_checksum_match yes

bench_pass "the spool returns render capacity when the origin finishes: ${OFF_MS}ms without it, ${ON_MS}ms with it, body integrity preserved on overflow"
