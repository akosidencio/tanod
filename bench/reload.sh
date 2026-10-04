#!/usr/bin/env bash
# Config reload on SIGHUP, including the case that matters: a bad config is
# refused and the running one keeps serving.
#
# Every phase is asserted against the *origin's* observed peak concurrency, not
# against the proxy's log line. A log line saying "config reloaded" only proves
# the file was parsed; it is not evidence that the new ceiling took effect, and
# an earlier version of this script printed that line and checked nothing.
BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"

BURST=${1:-6}
RAISED=${2:-6}

bench_init reload
bench_param burst "$BURST"
bench_param initial_ceiling 1
bench_param raised_ceiling "$RAISED"
bench_param render_ms 400
bench_build

ORIGIN_PORT=$(bench_free_port)
LISTEN_PORT=$(bench_free_port)
CONFIG="$BENCH_DIR/reload.yaml"

write_config() { # route ceiling
  cat > "$CONFIG" <<EOF
version: 1
server:
  listen: "127.0.0.1:$LISTEN_PORT"
origin:
  upstreams: ["127.0.0.1:$ORIGIN_PORT"]
  concurrency:
    max: 100
cache:
  enabled: false
routes:
  - id: pages
    match: "/**"
    class: public_ssr
    concurrency:
      max: $1
EOF
}

write_config 1
bench_spawn origin "$(bench_bin slow-origin)" "$ORIGIN_PORT" 400
bench_wait_port 127.0.0.1 "$ORIGIN_PORT" "slow-origin"
bench_start_tanod tanod "$CONFIG" "$LISTEN_PORT"
PID=$(bench_pid tanod)

# Fire a burst and report the peak concurrency the origin actually saw. The
# counters are reset first so each phase is measured on its own.
burst_peak() { # path prefix
  bench_origin_reset "$ORIGIN_PORT"
  seq 1 "$BURST" | xargs -P "$BURST" -I{} \
    curl -s -o /dev/null --max-time 20 "http://127.0.0.1:$LISTEN_PORT/$1/{}" >/dev/null 2>&1
  bench_origin_stat "$ORIGIN_PORT" peak
}

# SIGHUP, then wait for the process to say what it decided. Polling the log
# beats `sleep 1`: on a loaded machine a fixed sleep reports a slow reload as a
# refused one.
signal_and_wait() { # pattern
  local before
  before=$(wc -l < "$(bench_log tanod)")
  kill -HUP "$PID"
  for _ in $(seq 1 100); do
    if bench_log_has_since "$(bench_log tanod)" "$before" "$1"; then return 0; fi
    sleep 0.1
  done
  bench_fail "tanod never logged '$1' after SIGHUP"
}

echo "route ceiling 1, $BURST concurrent requests"
BASE_PEAK=$(burst_peak a)
echo "  origin peak            $BASE_PEAK"
bench_result initial_peak "$BASE_PEAK"
bench_assert_eq "$BASE_PEAK" 1 "origin peak under a ceiling of 1"

echo
echo "SIGHUP with an invalid config (duplicate route id)"
printf 'version: 1\norigin:\n  upstreams: ["127.0.0.1:%s"]\nroutes:\n  - id: dup\n    match: "/a"\n  - id: dup\n    match: "/b"\n' \
  "$ORIGIN_PORT" > "$CONFIG"
signal_and_wait "reload refused"
echo "  $(grep -o 'reload refused.*' "$(bench_log tanod)" | tail -1)"
REFUSED_PEAK=$(burst_peak b)
echo "  origin peak            $REFUSED_PEAK"
bench_result refused_peak "$REFUSED_PEAK"
bench_alive "$PID" || bench_fail "tanod exited on an invalid config instead of refusing it"
[ "$(bench_pid tanod)" = "$PID" ] || bench_fail "tanod restarted rather than keeping the running config"
bench_assert_eq "$REFUSED_PEAK" 1 "origin peak after a refused reload (the bad config took effect)"

echo
echo "SIGHUP raising the ceiling to $RAISED"
write_config "$RAISED"
signal_and_wait "config reloaded"
echo "  $(grep -o 'config reloaded.*' "$(bench_log tanod)" | tail -1)"
RAISED_PEAK=$(burst_peak c)
echo "  origin peak            $RAISED_PEAK"
bench_result raised_peak "$RAISED_PEAK"
bench_alive "$PID" || bench_fail "tanod exited during a valid reload"
[ "$(bench_pid tanod)" = "$PID" ] || bench_fail "tanod restarted rather than reloading in place"
# The point of the phase: the limiter was resized in place, so the burst now
# reaches the origin at the new width. Asserting only "the log said reloaded"
# would pass even if the new ceiling were ignored entirely.
bench_assert_gt "$RAISED_PEAK" 1 "origin peak after raising the ceiling (the new limit never took effect)"
bench_assert_le "$RAISED_PEAK" "$RAISED" "origin peak after raising the ceiling"

echo
bench_print_params
echo
bench_pass "ceiling 1 held, an invalid config was refused without dropping it, and a valid reload widened it to $RAISED_PEAK in the same process"
