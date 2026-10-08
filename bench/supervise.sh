#!/usr/bin/env bash
# Shutdown, and Tanod supervising its origin (`origin.command`).
#
# What has to hold:
#
#   * **A request in flight at SIGTERM finishes.** The drain ends, listeners
#     close, and admitted requests still complete. (Before 0.3.0 they were
#     cancelled the moment the drain ended.) Checked with and without a
#     supervised origin.
#   * **Tanod serves only once the origin does,** so a health check on Tanod's
#     port never sees a Tanod with nothing behind it.
#   * **The origin is stopped last,** after Tanod's own shutdown, and is never
#     left running: not after SIGTERM, not after a SIGTERM that arrives while
#     Tanod is still waiting for it to start, not after Ctrl+C to the group.
#   * **An origin that dies takes Tanod with it,** with its exit code, so the
#     platform restarts the pair.
#   * **What would leave two Tanods or none in charge is refused:** --upgrade
#     exits, and SIGQUIT drains and stops instead of handing over.
#
# Uses bench/slow-origin as the supervised app, so the origin is also the
# witness for what reached it.

BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"

RENDER_MS=${1:-2500}

bench_init supervise
bench_param render_ms "$RENDER_MS"
bench_param drain_period 1s
bench_param shutdown_timeout 5s
bench_build

TANOD=$(bench_bin tanod)
ORIGIN=$(bench_bin slow-origin)

# Tanod in the background with its exit code kept: bench_spawn disowns, and
# several checks here are about how Tanod exits.
run_tanod() { # name config -> sets RUN_PID
  "$TANOD" run --config "$2" >"$BENCH_DIR/logs/$1.log" 2>&1 &
  RUN_PID=$!
  BENCH_PIDS="$BENCH_PIDS $RUN_PID"
}

# Wait for a pid to exit within N tenths of a second and set EXIT_CODE. Not
# `$(...)`: a subshell cannot `wait` for this shell's children.
exit_code_within() { # pid tenths
  local _
  for _ in $(seq 1 "$2"); do
    bench_alive "$1" || break
    sleep 0.1
  done
  if bench_alive "$1"; then
    EXIT_CODE="still-running"
  else
    wait "$1" 2>/dev/null
    EXIT_CODE=$?
  fi
}

origin_pid_from() { # tanod log name
  sed -n 's/.*origin is up (pid \([0-9]*\)).*/\1/p' "$(bench_log "$1")" | head -1
}

gone() { ! kill -0 "$1" 2>/dev/null; }

config() { # out listen upstream [command yaml lines]
  local out=$1 listen=$2 upstream=$3
  shift 3
  {
    cat <<EOF
version: 1
server:
  listen: "127.0.0.1:$listen"
  graceful:
    drain_period: 1s
    shutdown_timeout: 5s
    pid_file: "$BENCH_DIR/tanod-$listen.pid"
    upgrade_socket: "$BENCH_DIR/tanod-$listen.sock"
origin:
  upstreams: ["127.0.0.1:$upstream"]
EOF
    for line in "$@"; do echo "$line"; done
    cat <<EOF
routes:
  - id: all
    match: "/**"
    class: private_dynamic
EOF
  } >"$out"
}

slow_request() { # port -> CURL_PID; the status code lands in $BENCH_DIR/slow-$port
  curl -s -o /dev/null -w '%{http_code}' --max-time 30 "http://127.0.0.1:$1/slow" \
    >"$BENCH_DIR/slow-$1" 2>/dev/null &
  CURL_PID=$!
}

echo "shutdown without supervision"
UP=$(bench_start_origin plain-origin "$RENDER_MS")
LISTEN=$(bench_free_port)
config "$BENCH_DIR/plain.yaml" "$LISTEN" "$UP"
run_tanod plain "$BENCH_DIR/plain.yaml"
bench_wait_port 127.0.0.1 "$LISTEN" "tanod"
slow_request "$LISTEN"
sleep 0.4
kill -TERM "$RUN_PID"
wait "$CURL_PID"
PLAIN_STATUS=$(cat "$BENCH_DIR/slow-$LISTEN")
exit_code_within "$RUN_PID" 150; PLAIN_EXIT=$EXIT_CODE
echo "    in-flight request    $PLAIN_STATUS"
echo "    tanod exit           $PLAIN_EXIT"
bench_assert_eq "$PLAIN_STATUS" 200 "a request in flight at SIGTERM (it was cancelled when the drain ended)"
bench_assert_eq "$PLAIN_EXIT" 0 "tanod's exit after SIGTERM"
bench_result plain_in_flight "$PLAIN_STATUS"

echo "supervised: ready, serve, SIGTERM with a request in flight"
UP=$(bench_free_port)
LISTEN=$(bench_free_port)
# One second of startup before the origin listens, so serving early would show.
config "$BENCH_DIR/sup.yaml" "$LISTEN" "$UP" \
  "  command:" \
  "    args: [\"sh\", \"-c\", \"sleep 1; exec $ORIGIN $UP $RENDER_MS\"]"
run_tanod sup "$BENCH_DIR/sup.yaml"
TANOD_PID=$RUN_PID
sleep 0.5
EARLY="closed"
bench_port_open 127.0.0.1 "$LISTEN" && EARLY="open"
echo "    tanod port before the origin is up   $EARLY"
[ "$EARLY" = closed ] || bench_fail "tanod accepted connections before its origin was up"
bench_wait_port 127.0.0.1 "$LISTEN" "supervised tanod"
APP=$(origin_pid_from sup)
[ -n "$APP" ] || bench_fail "tanod did not report the origin's pid"
STATUS=$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 "http://127.0.0.1:$LISTEN/first")
bench_assert_eq "$STATUS" 200 "a request through the supervised origin"
bench_assert_eq "$(bench_origin_stat "$UP" total)" 1 "renders the origin saw"
slow_request "$LISTEN"
sleep 0.4
kill -TERM "$TANOD_PID"
wait "$CURL_PID"
SUP_STATUS=$(cat "$BENCH_DIR/slow-$LISTEN")
exit_code_within "$TANOD_PID" 200; SUP_EXIT=$EXIT_CODE
echo "    in-flight request    $SUP_STATUS"
echo "    tanod exit           $SUP_EXIT"
bench_assert_eq "$SUP_STATUS" 200 "a request in flight when the supervised tanod got SIGTERM"
bench_assert_eq "$SUP_EXIT" 0 "the supervised tanod's exit"
gone "$APP" || bench_fail "the origin (pid $APP) outlived tanod"
bench_log_has_since "$(bench_log sup)" 0 "stopping the origin" \
  || bench_fail "tanod never logged stopping the origin"
bench_result supervised_in_flight "$SUP_STATUS"

echo "supervised: the origin dies"
UP=$(bench_free_port)
LISTEN=$(bench_free_port)
config "$BENCH_DIR/crash.yaml" "$LISTEN" "$UP" \
  "  command:" \
  "    args: [\"sh\", \"-c\", \"$ORIGIN $UP 50 & o=\$!; sleep 2; kill \$o; exit 42\"]"
run_tanod crash "$BENCH_DIR/crash.yaml"
bench_wait_port 127.0.0.1 "$LISTEN" "crashing tanod"
exit_code_within "$RUN_PID" 100; CRASH_EXIT=$EXIT_CODE
echo "    tanod exit           $CRASH_EXIT (the origin exited 42)"
bench_assert_eq "$CRASH_EXIT" 42 "tanod's exit code when its origin died"

echo "supervised: SIGTERM while waiting for the origin to start"
UP=$(bench_free_port)
LISTEN=$(bench_free_port)
config "$BENCH_DIR/slowstart.yaml" "$LISTEN" "$UP" \
  "  command:" \
  "    args: [\"sh\", \"-c\", \"echo \$\$ > $BENCH_DIR/slowstart.pid; exec sleep 300\"]" \
  "    ready_timeout: 120s"
run_tanod slowstart "$BENCH_DIR/slowstart.yaml"
for _ in $(seq 1 50); do [ -s "$BENCH_DIR/slowstart.pid" ] && break; sleep 0.1; done
STARTING_APP=$(cat "$BENCH_DIR/slowstart.pid" 2>/dev/null)
[ -n "$STARTING_APP" ] || bench_fail "the never-ready origin did not start"
sleep 0.3
kill -TERM "$RUN_PID"
exit_code_within "$RUN_PID" 100; EARLY_EXIT=$EXIT_CODE
sleep 0.3
echo "    tanod exit           $EARLY_EXIT"
bench_assert_eq "$EARLY_EXIT" 0 "tanod's exit when stopped before serving"
gone "$STARTING_APP" || bench_fail "a SIGTERM during startup left the origin (pid $STARTING_APP) running"

if command -v setsid >/dev/null 2>&1; then
  echo "supervised: Ctrl+C to tanod's whole process group"
  UP=$(bench_free_port)
  LISTEN=$(bench_free_port)
  config "$BENCH_DIR/group.yaml" "$LISTEN" "$UP" \
    "  command:" \
    "    args: [\"$ORIGIN\", \"$UP\", \"50\"]"
  setsid "$TANOD" run --config "$BENCH_DIR/group.yaml" >"$BENCH_DIR/logs/group.log" 2>&1 &
  GROUP_PID=$!
  BENCH_PIDS="$BENCH_PIDS $GROUP_PID"
  bench_wait_port 127.0.0.1 "$LISTEN" "tanod in its own session"
  APP=$(origin_pid_from group)
  kill -INT -- "-$GROUP_PID"
  exit_code_within "$GROUP_PID" 100; GROUP_EXIT=$EXIT_CODE
  sleep 0.3
  echo "    tanod exit           $GROUP_EXIT"
  bench_assert_eq "$GROUP_EXIT" 0 "tanod's exit after a group SIGINT"
  gone "$APP" || bench_fail "the origin (pid $APP) survived a group SIGINT"
  bench_log_has_since "$(bench_log group)" 0 "stopping the origin" \
    || bench_fail "the origin was not stopped by tanod after a group SIGINT"
fi

echo "supervised: SIGQUIT and --upgrade"
UP=$(bench_free_port)
LISTEN=$(bench_free_port)
config "$BENCH_DIR/quit.yaml" "$LISTEN" "$UP" \
  "  command:" \
  "    args: [\"$ORIGIN\", \"$UP\", \"50\"]"
UPGRADE_OUT=$("$TANOD" run --config "$BENCH_DIR/quit.yaml" --upgrade 2>&1)
UPGRADE_EXIT=$?
echo "    --upgrade exit       $UPGRADE_EXIT"
bench_assert_eq "$UPGRADE_EXIT" 1 "tanod run --upgrade with a supervised origin"
case "$UPGRADE_OUT" in
  *"cannot be used with origin.command"*) ;;
  *) bench_fail "--upgrade was refused without saying why: $UPGRADE_OUT" ;;
esac
run_tanod quit "$BENCH_DIR/quit.yaml"
bench_wait_port 127.0.0.1 "$LISTEN" "tanod"
APP=$(origin_pid_from quit)
kill -QUIT "$RUN_PID"
exit_code_within "$RUN_PID" 150; QUIT_EXIT=$EXIT_CODE
echo "    SIGQUIT exit         $QUIT_EXIT"
bench_assert_eq "$QUIT_EXIT" 0 "tanod's exit after SIGQUIT"
gone "$APP" || bench_fail "the origin (pid $APP) survived SIGQUIT"
bench_log_has_since "$(bench_log quit)" 0 "cannot hand over a supervised origin" \
  || bench_fail "SIGQUIT was not turned into a drain and stop"

for name in plain sup crash slowstart quit; do bench_assert_no_panics "$name"; done

echo
bench_print_params
echo
bench_pass "in-flight requests finished on SIGTERM with and without a supervised origin; tanod served only once its origin did, stopped it last, exited with its code when it died, stopped it when interrupted during startup, and refused --upgrade"
