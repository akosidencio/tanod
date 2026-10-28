#!/usr/bin/env bash
# Heap allocations per request, by request type, counted by valgrind's DHAT.
#
#   bench/alloc.sh [tanod-binary]
#
# For each request type, Tanod runs under DHAT while the type is requested
# exactly ALLOC_REQUESTS times over one keep-alive connection; a run with no
# requests at all measures startup and shutdown, and is subtracted. What is
# left, divided by the request count, is what one request of that type costs
# the allocator — blocks and bytes. Deterministic, so unlike throughput it can
# be compared between two builds on a busy machine.
#
# Under DHAT Tanod runs one thread at a time, tens of times slower: this is an
# allocation count, never a latency or throughput figure.
#
# Needs valgrind and ab (Apache Bench). Writes one JSON line per type to
# stdout, and to $ALLOC_OUT when set.
BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"

REQUESTS=${ALLOC_REQUESTS:-400}
TYPES=${ALLOC_TYPES:-"static_hit ssr_hit ssr_miss private rsc_hit server_action bypass shed"}

command -v valgrind >/dev/null || { echo "bench/alloc.sh needs valgrind" >&2; exit 1; }
command -v ab >/dev/null || { echo "bench/alloc.sh needs ab (Apache Bench)" >&2; exit 1; }

BENCH_PROFILE=release
export BENCH_PROFILE
bench_init alloc
bench_param requests "$REQUESTS"
if [ -n "${1:-}" ]; then
  TANOD_BIN=$1
else
  bench_build
  TANOD_BIN=$(bench_bin tanod)
fi
LOADGEN=$(bench_bin loadgen)
ORIGIN_BIN=$(bench_bin slow-origin)

LISTEN_PORT=$(bench_free_port)
ORIGIN_PORT=$(bench_free_port)
SLOW_PORT=$(bench_free_port)
CONFIG="$BENCH_DIR/alloc.yaml"
cat > "$CONFIG" <<EOF
version: 1
server:
  listen: "127.0.0.1:$LISTEN_PORT"
  graceful:
    drain_period: 0s
    shutdown_timeout: 1s
    pid_file: "$BENCH_DIR/tanod.pid"
    upgrade_socket: "$BENCH_DIR/upgrade.sock"
origin:
  upstreams: ["127.0.0.1:$ORIGIN_PORT"]
  concurrency:
    max: 64
cache:
  enabled: true
  max_memory: 256MiB
  max_body_size: 8MiB
routes:
  - id: next-static
    match: "/_next/static/**"
    class: static
    cache:
      override_origin: true
      ttl:
        max: 1h
  - id: products
    match: "/products/**"
    class: public_ssr
    cache:
      override_origin: true
      ttl:
        max: 1h
    coalesce:
      enabled: true
  - id: account
    match: "/account/**"
    class: private_dynamic
  - id: api
    match: "/api/**"
    class: public_dynamic
    cache:
      enabled: false
  - id: shed
    match: "/shed/**"
    class: public_dynamic
    cache:
      enabled: false
    concurrency:
      max: 1
      queue:
        max: 0
        timeout: 1s
  - id: catchall
    match: "/**"
EOF

bench_spawn origin "$ORIGIN_BIN" "$ORIGIN_PORT" 0
bench_wait_port 127.0.0.1 "$ORIGIN_PORT" "slow-origin"

BASE="http://127.0.0.1:$LISTEN_PORT"
POST_BODY="$BENCH_DIR/action.txt"
printf '["form-data"]' > "$POST_BODY"

# ab sends exactly -n requests; -k reuses the connection.
abn() { ab -q -k -n "$1" -c 1 "${@:2}" >/dev/null 2>&1 || bench_fail "ab failed: ${*:2}"; }

drive() { # type count
  local n=$2
  [ "$n" -eq 0 ] && return 0
  case "$1" in
    static_hit) curl -s -o /dev/null "$BASE/_next/static/a/size/8192"
                abn "$n" "$BASE/_next/static/a/size/8192" ;;
    ssr_hit) curl -s -o /dev/null "$BASE/products/1/size/32768"
             abn "$n" "$BASE/products/1/size/32768" ;;
    rsc_hit) curl -s -o /dev/null -H 'RSC: 1' "$BASE/products/1/size/8192"
             abn "$n" -H 'RSC: 1' "$BASE/products/1/size/8192" ;;
    private) abn "$n" -H 'Cookie: session=alloc' "$BASE/account/1/size/16384" ;;
    server_action) abn "$n" -p "$POST_BODY" -T 'text/plain;charset=UTF-8' \
                     -H 'Next-Action: 7f3a' "$BASE/products/1" ;;
    bypass) abn "$n" "$BASE/api/search/size/4096?q=alloc" ;;
    ssr_miss)
      # Every request a new key: one keep-alive connection, `n` requests.
      local done=0 got
      while [ "$done" -lt "$n" ]; do
        got=$("$LOADGEN" run --addr "127.0.0.1:$LISTEN_PORT" --path "/products/miss-{u}/size/32768" \
          --connections 1 --threads 1 --warmup 0 --duration 0.5 | jq '.requests + .errors')
        done=$((done + got))
      done
      echo "$done" > "$BENCH_DIR/miss-count" ;;
    shed)
      # One request holds the route's only permit on a slow render; every
      # request sent meanwhile is shed.
      curl -s -o /dev/null --max-time 60 "$BASE/shed/hold?slow=1" &
      local holder=$!
      sleep 2
      abn "$n" "$BASE/shed/x"
      kill "$holder" 2>/dev/null
      wait "$holder" 2>/dev/null ;;
  esac
}

# Allocation totals from a DHAT output file: "blocks bytes".
dhat_totals() {
  python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
print(sum(p["tbk"] for p in d["pps"]), sum(p["tb"] for p in d["pps"]))
' "$1"
}

# Run one type under DHAT and leave "blocks bytes" in $BENCH_DIR/totals.
# Not a command substitution: bench_fail inside one would end only the
# subshell, and the run would report zeros instead of failing.
run_type() { # type count
  local type=$1 n=$2 out="$BENCH_DIR/dhat-$1-$2.json" pid
  rm -f "$BENCH_DIR/totals"
  bench_spawn tanod valgrind --tool=dhat --dhat-out-file="$out" "$TANOD_BIN" run --config "$CONFIG"
  pid=$(bench_pid tanod)
  # Startup under DHAT is slow, and slower on a busy machine.
  for _ in $(seq 1 1200); do
    bench_port_open 127.0.0.1 "$LISTEN_PORT" && break
    bench_alive "$pid" || break
    sleep 0.1
  done
  bench_port_open 127.0.0.1 "$LISTEN_PORT" || { bench_dump_logs; bench_fail "tanod under dhat never listened ($type)"; }
  drive "$type" "$n"
  # Not bench_stop: under DHAT the graceful shutdown takes far longer than
  # its five-second allowance, and a killed process writes no profile.
  kill -TERM "$pid" 2>/dev/null
  for _ in $(seq 1 1200); do bench_alive "$pid" || break; sleep 0.1; done
  bench_stop tanod
  for _ in $(seq 1 100); do [ -s "$out" ] && break; sleep 0.2; done
  [ -s "$out" ] || bench_fail "no DHAT output for $type"
  dhat_totals "$out" > "$BENCH_DIR/totals" || bench_fail "unreadable DHAT output for $type"
  read -r TOTAL_BLOCKS TOTAL_BYTES < "$BENCH_DIR/totals"
  bench_assert_int "$TOTAL_BLOCKS" "DHAT blocks for $type"
}

# The shed type renders slowly so that its permit stays held.
start_slow_origin() {
  bench_stop origin
  bench_spawn origin "$ORIGIN_BIN" "$ORIGIN_PORT" "$1"
  bench_wait_port 127.0.0.1 "$ORIGIN_PORT" "slow-origin"
}

run_type idle 0
IDLE_BLOCKS=$TOTAL_BLOCKS
IDLE_BYTES=$TOTAL_BYTES
echo "startup and shutdown alone: $IDLE_BLOCKS blocks, $IDLE_BYTES bytes" >&2
for type in $TYPES; do
  if [ "$type" = shed ]; then start_slow_origin 30000; else start_slow_origin 0; fi
  run_type "$type" "$REQUESTS"
  blocks=$TOTAL_BLOCKS bytes=$TOTAL_BYTES
  count=$REQUESTS
  [ "$type" = ssr_miss ] && count=$(cat "$BENCH_DIR/miss-count")
  line=$(awk -v t="$type" -v b="$blocks" -v y="$bytes" -v ib="$IDLE_BLOCKS" -v iy="$IDLE_BYTES" -v n="$count" \
    -v bin="$TANOD_BIN" 'BEGIN { printf "{\"type\":\"%s\",\"requests\":%d,\"allocs_per_request\":%.1f,\"bytes_per_request\":%.0f,\"binary\":\"%s\"}\n", t, n, (b - ib) / n, (y - iy) / n, bin }')
  echo "$line"
  [ -n "${ALLOC_OUT:-}" ] && echo "$line" >> "$ALLOC_OUT"
done
bench_pass "allocation profile complete"
