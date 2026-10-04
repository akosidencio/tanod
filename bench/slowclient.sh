#!/usr/bin/env bash
# Can a slow reader occupy origin capacity it is not using?
#
# Yes, partly — and this script measures how much, then asserts the one thing
# Tanod actually promises about it.
#
# A permit models render capacity and is returned when pingora observes
# upstream end-of-stream. Because pingora paces upstream reads against
# downstream writes, a slow reader delays that observation. Whether a given
# body size is delayed at all depends on the socket buffers between the origin
# and the client, which differ per kernel and per machine — so the size table
# below is reported as a diagnostic and never asserted.
#
# What *is* asserted is the documented guarantee: `timeouts.downstream_write`
# bounds the stall. A slow reader must not be able to hold a render slot
# indefinitely.
BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"

DW=${1:-30s}
BOUNDED_DW=${2:-2s}
BOUNDED_SECONDS=${BOUNDED_DW%s}

bench_init slow_reader
bench_param downstream_write "$DW"
bench_param bounded_downstream_write "$BOUNDED_DW"
bench_param ceiling 2
bench_param reader_rate 32k
bench_build

ORIGIN_PORT=$(bench_free_port)
LISTEN_PORT=$(bench_free_port)

write_config() { # downstream_write
  cat > "$BENCH_DIR/slowclient.yaml" <<EOF
version: 1
server:
  listen: "127.0.0.1:$LISTEN_PORT"
origin:
  upstreams: ["127.0.0.1:$ORIGIN_PORT"]
  concurrency:
    max: 2
    queue:
      max: 10
      timeout: 3s
timeouts:
  downstream_write: $1
cache:
  enabled: false
routes:
  - id: pages
    match: "/**"
    class: public_ssr
EOF
}

# Two rate-limited readers occupy both permits, then one ordinary request asks
# for the capacity they are no longer using. RESULT is set for the caller.
probe() { # url for the slow readers, label, settle seconds
  bench_stop tanod; bench_stop origin
  bench_spawn origin "$(bench_bin slow-origin)" "$ORIGIN_PORT" 50
  bench_wait_port 127.0.0.1 "$ORIGIN_PORT" "slow-origin"
  bench_start_tanod tanod "$BENCH_DIR/slowclient.yaml" "$LISTEN_PORT"

  bench_spawn reader1 curl -s -o /dev/null --limit-rate 32k "$1"
  bench_spawn reader2 curl -s -o /dev/null --limit-rate 32k "$1"
  sleep "$3"

  local start code ms
  start=$(date +%s%N)
  code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 "http://127.0.0.1:$LISTEN_PORT/quick")
  ms=$(( ($(date +%s%N) - start) / 1000000 ))
  bench_stop reader1; bench_stop reader2
  if [ "$code" = "200" ] && [ "$ms" -lt 1000 ]; then RESULT=yes; else RESULT=no; fi
  PROBE_CODE=$code
  PROBE_MS=$ms
  printf '  %-28s %-8s %-9s %s\n' "$2" "$code" "${ms}ms" "$RESULT"
}

echo "ceiling = 2, two readers at 32KB/s, then one normal request"
echo "timeouts.downstream_write = $DW"
echo
printf '  %-28s %-8s %-9s %s\n' "response" "status" "latency" "capacity returned?"

write_config "$DW"
for MIB in 1 4 16; do
  probe "http://127.0.0.1:$LISTEN_PORT/big/x/$MIB" "buffered ${MIB}MiB" 3
  bench_result "buffered_${MIB}mib" "$RESULT"
done
probe "http://127.0.0.1:$LISTEN_PORT/bigstream/40" "streaming (chunked)" 3
bench_result streaming "$RESULT"

echo
echo "Rows above are diagnostic: whether a given size stalls depends on the"
echo "socket buffers between origin and client, which are not portable."
echo
echo "Asserted: with timeouts.downstream_write = $BOUNDED_DW, the stall is bounded"
echo
printf '  %-28s %-8s %-9s %s\n' "response" "status" "latency" "capacity returned?"

# The same two slow readers, but the write timeout now cuts them off. Waiting
# past the timeout must find the capacity back, whatever the buffers did.
write_config "$BOUNDED_DW"
probe "http://127.0.0.1:$LISTEN_PORT/bigstream/200" "streaming, bounded write" \
  "$(awk -v t="$BOUNDED_SECONDS" 'BEGIN{print t + 2}')"
bench_result bounded_probe_status "$PROBE_CODE"
bench_result bounded_probe_ms "$PROBE_MS"

echo
bench_print_params
echo

[ "$PROBE_CODE" = "200" ] || bench_fail \
  "after timeouts.downstream_write ($BOUNDED_DW) elapsed the probe still got HTTP $PROBE_CODE — a slow reader held a render slot past the bound that is supposed to cap it"
bench_assert_le "$PROBE_MS" 1000 "probe latency once the write timeout had elapsed"
bench_pass "slow readers can delay observed origin end-of-stream, but timeouts.downstream_write bounds it: capacity was back within ${PROBE_MS}ms of the deadline"
