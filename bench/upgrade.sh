#!/usr/bin/env bash
# Restarting without dropping requests, both ways Tanod supports it.
#
# **The socket handover is Linux-only.** Pingora passes listening file
# descriptors between processes with `SCM_RIGHTS`; its non-Linux `get_fds_from`
# is a stub that logs "Upgrade is not currently supported" and returns
# `ECONNREFUSED` — which reads exactly like "the old process is not running"
# and sends an operator hunting a problem that does not exist. Tanod refuses
# `--upgrade` up front off Linux instead, and this script asserts the handover
# where it works and the drain-based restart everywhere.
#
# The two are not the same claim and this script does not pretend they are:
#
#   handover  no request fails, because the new process owns the listening
#             socket before the old one lets go of it.
#   drain     no request fails *while the instance is draining*, and the
#             instance advertises itself not-ready throughout — but between
#             the old process exiting and the new one binding, nothing owns
#             the port. A load balancer covers that gap; the drain window is
#             what gives it time to. Asserting zero loss there would be
#             asserting something the platform cannot deliver, which is the
#             kind of evidence this suite exists to remove.
#
# Also asserted on every platform: `tanod run --test` exits zero on a config
# that can start and non-zero on one that cannot. That is the pre-flight the
# documented procedure runs before it signals a live process, and a check that
# cannot fail is not a check.
BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"

DURATION=${1:-6}

bench_init upgrade
bench_param traffic_seconds "$DURATION"
bench_param render_ms 20
bench_param drain_period_s 1
bench_param shutdown_timeout_s 3
bench_build

ORIGIN_PORT=$(bench_free_port)
LISTEN_PORT=$(bench_free_port)
ADMIN_PORT=$(bench_free_port)
CONFIG="$BENCH_DIR/upgrade.yaml"

bench_render_config "$BENCH_ROOT/bench/upgrade.yaml.tpl" "$CONFIG" \
  "LISTEN=$LISTEN_PORT" "ORIGIN=$ORIGIN_PORT" "ADMIN=$ADMIN_PORT" \
  "PIDFILE=$BENCH_DIR/tanod.pid" "UPGRADESOCK=$BENCH_DIR/upgrade.sock"

bench_spawn origin "$(bench_bin slow-origin)" "$ORIGIN_PORT" 20
bench_wait_port 127.0.0.1 "$ORIGIN_PORT" "slow-origin"

MODE=drain
[ "$(uname -s)" = "Linux" ] && MODE=handover
bench_param mode "$MODE"

# Traffic from a client that keeps no connection open between requests. A
# keep-alive client would ride the old process's already-accepted socket and
# never exercise the transition at all.
traffic_for() { # seconds, out-file
  local end file=$2
  : > "$file"
  end=$(( $(date +%s) + $1 ))
  while [ "$(date +%s)" -lt "$end" ]; do
    curl -s -o /dev/null -w '%{http_code}\n' --max-time 10 \
      -H 'Connection: close' "http://127.0.0.1:$LISTEN_PORT/render" >> "$file" 2>&1 \
      || echo "000" >> "$file"
  done
}

count_ok()  { grep -c '^200$' "$1" 2>/dev/null || echo 0; }
count_all() { wc -l < "$1" | tr -d ' '; }

admin_code() { curl -s -o /dev/null -w '%{http_code}' --max-time 5 "http://127.0.0.1:$ADMIN_PORT$1"; }
wait_for_ready() { # expected code
  local code
  for _ in $(seq 1 150); do
    code=$(admin_code /health/ready)
    [ "$code" = "$1" ] && { printf '%s' "$code"; return 0; }
    sleep 0.1
  done
  printf '%s' "$code"
}

# ------------------------------------------------------------- pre-flight

echo "pre-flight: tanod run --test"
"$(bench_bin tanod)" run --config "$CONFIG" --test >"$BENCH_DIR/logs/test-ok.log" 2>&1
TEST_OK=$?
echo "  valid config           exit $TEST_OK"
bench_assert_eq "$TEST_OK" 0 "tanod run --test on a valid config"

printf 'version: 1\norigin:\n  upstreams: []\n' > "$BENCH_DIR/broken.yaml"
"$(bench_bin tanod)" run --config "$BENCH_DIR/broken.yaml" --test >"$BENCH_DIR/logs/test-bad.log" 2>&1
TEST_BAD=$?
echo "  invalid config         exit $TEST_BAD"
[ "$TEST_BAD" -ne 0 ] || bench_fail "tanod run --test exited 0 on a config that cannot start"

if [ "$MODE" = "drain" ]; then
  # The message is the deliverable here, not the exit code: a bare
  # ECONNREFUSED would be read as a missing peer.
  "$(bench_bin tanod)" run --config "$CONFIG" --upgrade \
    >"$BENCH_DIR/logs/test-upgrade.log" 2>&1
  echo "  --upgrade off Linux    exit $?"
  grep -q "not supported on this platform" "$BENCH_DIR/logs/test-upgrade.log" \
    || bench_fail "--upgrade off Linux did not explain why: $(cat "$BENCH_DIR/logs/test-upgrade.log")"
fi

# --------------------------------------------------------- the old process

bench_start_tanod old "$CONFIG" "$LISTEN_PORT" "$ADMIN_PORT"
OLD_PID=$(bench_pid old)
echo
echo "old process pid $OLD_PID on $LISTEN_PORT, admin on $ADMIN_PORT"
READY=$(wait_for_ready 200)
bench_assert_eq "$READY" 200 "readiness before anything happens"

# A bare SIGTERM must create an observable drain window before Pingora closes
# its listeners. This is separate from the explicit SIGUSR1 pre-stop path:
# without this assertion a normal `docker stop`, systemd stop, or Kubernetes
# termination can regress to readiness disappearing at the exact instant that
# traffic disappears.
echo
echo "automatic drain on SIGTERM"
kill -TERM "$OLD_PID"
READY=$(wait_for_ready 503)
echo "  readiness while alive $READY"
bench_assert_eq "$READY" 503 "plain SIGTERM must advertise not-ready before listeners close"
bench_alive "$OLD_PID" || bench_fail "plain SIGTERM skipped the observable drain window"

AUTO_OK=0
for _ in $(seq 1 10); do
  [ "$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 \
    -H 'Connection: close' "http://127.0.0.1:$LISTEN_PORT/render")" = "200" ] \
    && AUTO_OK=$((AUTO_OK + 1))
done
echo "  requests still served $AUTO_OK/10"
bench_result sigterm_drain_succeeded "$AUTO_OK"
bench_assert_eq "$AUTO_OK" 10 "requests served during the automatic SIGTERM drain window"

AUTO_EXITED=0
for _ in $(seq 1 150); do
  bench_alive "$OLD_PID" || { AUTO_EXITED=1; break; }
  sleep 0.1
done
bench_assert_eq "$AUTO_EXITED" 1 "the automatic-drain process exited after SIGTERM"

# Start a fresh old process for the platform-specific restart scenario below.
bench_start_tanod old "$CONFIG" "$LISTEN_PORT" "$ADMIN_PORT"
OLD_PID=$(bench_pid old)
READY=$(wait_for_ready 200)
bench_assert_eq "$READY" 200 "readiness after the automatic-drain regression check"

if [ "$MODE" = "handover" ]; then
  # --------------------------------------------------------- socket handover
  #
  # Traffic runs *across* the transition, because that is the claim. The new
  # process starts with --upgrade and receives the listening descriptors over
  # the upgrade socket; the old one is then signalled SIGQUIT and drains what
  # it already accepted. Both halves are needed: --upgrade alone waits on a
  # socket nobody offers, and SIGQUIT alone hands the listeners to nobody.
  traffic_for "$DURATION" "$BENCH_DIR/across" &
  TRAFFIC=$!
  sleep 2

  echo
  echo "starting the new process with --upgrade"
  bench_spawn new "$(bench_bin tanod)" run --config "$CONFIG" --upgrade
  NEW_PID=$(bench_pid new)
  sleep 1
  echo "  new process pid        $NEW_PID"
  echo "  SIGQUIT to             $OLD_PID"
  kill -QUIT "$OLD_PID"

  DRAINED=0
  for _ in $(seq 1 400); do
    bench_alive "$OLD_PID" || { DRAINED=1; break; }
    sleep 0.1
  done
  echo "  old process exited     $([ $DRAINED = 1 ] && echo yes || echo 'NO — still running')"
  wait "$TRAFFIC" 2>/dev/null

  TOTAL=$(count_all "$BENCH_DIR/across")
  OK=$(count_ok "$BENCH_DIR/across")
  BAD=$((TOTAL - OK))
  echo
  echo "traffic across the handover"
  echo "  requests               $TOTAL"
  echo "  200                    $OK"
  echo "  anything else          $BAD"
  bench_result requests "$TOTAL"
  bench_result succeeded "$OK"
  bench_result failed "$BAD"
  bench_assert_gt "$TOTAL" 20 "requests issued across the handover (too few to prove anything)"
  # The claim, and the only platform on which it can be made.
  bench_assert_eq "$BAD" 0 "requests that did not return 200 across the socket handover"
  bench_assert_eq "$DRAINED" 1 "the old process exited after handing over"
else
  # ------------------------------------------------------- drain and replace
  #
  # Measured in phases, because a single number across the whole restart would
  # only be measuring how long this script chose to keep hammering a port
  # nobody was listening on.
  echo
  echo "phase 1: drain window (SIGUSR1, still serving)"
  kill -USR1 "$OLD_PID"
  READY=$(wait_for_ready 503)
  echo "  readiness              $READY"
  bench_assert_eq "$READY" 503 "readiness must fail as soon as a drain begins"
  bench_alive "$OLD_PID" || bench_fail "SIGUSR1 terminated the process; a drain must not exit"

  traffic_for "$DURATION" "$BENCH_DIR/draining"
  DRAIN_TOTAL=$(count_all "$BENCH_DIR/draining")
  DRAIN_OK=$(count_ok "$BENCH_DIR/draining")
  echo "  requests               $DRAIN_TOTAL"
  echo "  200                    $DRAIN_OK"
  bench_result draining_requests "$DRAIN_TOTAL"
  bench_result draining_succeeded "$DRAIN_OK"
  bench_assert_gt "$DRAIN_TOTAL" 10 "requests issued during the drain window"
  # The whole point of the window: not-ready to the balancer, business as
  # usual to anything still arriving.
  bench_assert_eq "$DRAIN_OK" "$DRAIN_TOTAL" \
    "requests served during the drain window (the window serves nothing, so it buys nothing)"

  echo
  echo "phase 2: stop and replace"
  STOPPED_AT=$(date +%s)
  kill -TERM "$OLD_PID"
  DRAINED=0
  for _ in $(seq 1 400); do
    bench_alive "$OLD_PID" || { DRAINED=1; break; }
    sleep 0.1
  done
  GAP=$(( $(date +%s) - STOPPED_AT ))
  echo "  old process exited     $([ $DRAINED = 1 ] && echo "yes after ${GAP}s" || echo 'NO')"
  bench_result shutdown_seconds "$GAP"
  bench_assert_eq "$DRAINED" 1 "the old process exited on SIGTERM"
  # SIGUSR1 began more than one drain period ago, so SIGTERM does not pay that
  # window twice. Pingora's shutdown timeout still runs to completion on an
  # idle process; this config therefore takes about three seconds here.
  echo "  (expected ~3s: the SIGUSR1 drain window already elapsed, leaving only"
  echo "   Pingora's 3s shutdown timeout — see docs/OPERATIONS.md)"
  bench_assert_le "$GAP" 12 "seconds from SIGTERM to exit"
  bench_assert_gt "$GAP" 2 "seconds from SIGTERM to exit (in-flight requests got no window)"

  bench_start_tanod new "$CONFIG" "$LISTEN_PORT" "$ADMIN_PORT"
  NEW_PID=$(bench_pid new)
  echo "  new process pid        $NEW_PID"
  READY=$(wait_for_ready 200)
  bench_assert_eq "$READY" 200 "readiness on the new process"

  echo
  echo "phase 3: traffic after the replacement"
  traffic_for 3 "$BENCH_DIR/after"
  AFTER_TOTAL=$(count_all "$BENCH_DIR/after")
  AFTER_OK=$(count_ok "$BENCH_DIR/after")
  echo "  requests               $AFTER_TOTAL"
  echo "  200                    $AFTER_OK"
  bench_result after_requests "$AFTER_TOTAL"
  bench_result after_succeeded "$AFTER_OK"
  bench_assert_gt "$AFTER_TOTAL" 5 "requests issued after the replacement"
  bench_assert_eq "$AFTER_OK" "$AFTER_TOTAL" "requests served by the replacement process"
fi

bench_alive "$NEW_PID" || bench_fail "the new process is not running after the restart"
[ "$NEW_PID" != "$OLD_PID" ] || bench_fail "the pid did not change; no restart took place"

echo
bench_print_params
echo
if [ "$MODE" = "handover" ]; then
  bench_pass "$OK/$TOTAL requests served across a live socket handover; pid $OLD_PID drained and exited, pid $NEW_PID serving"
else
  bench_pass "drain-based restart: readiness failed first, $DRAIN_OK/$DRAIN_TOTAL served while draining, old pid exited in ${GAP}s, $AFTER_OK/$AFTER_TOTAL served by pid $NEW_PID (the socket handover itself needs Linux)"
fi
