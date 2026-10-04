#!/usr/bin/env bash
# Things going wrong while traffic is flowing.
#
# The other benchmarks each hold one thing constant and vary another. This one
# breaks the environment underneath a running proxy and asserts what must
# remain true regardless:
#
#   * **Tanod survives.** Not "mostly succeeds" — survives. A governor that
#     dies when its origin does has made the outage worse than no governor.
#   * **Nothing is answered wrongly.** A failing origin may produce a 502 or a
#     stale hit; it must never produce another user's response, and a
#     `Set-Cookie` route must stay unshared through all of it.
#   * **Capacity comes back.** Every permit held by a request that died with
#     its backend must be released. A leak here is invisible during the chaos
#     and fatal afterwards, when the origin is healthy and the proxy still
#     sheds everything.
#   * **The operator surface keeps answering.** `/health/live` and `/status`
#     have to work during an incident specifically, because that is when
#     somebody reads them. An admin endpoint that hangs when the origin is
#     down is an admin endpoint that is never available when it matters.
#
# One backend of two is killed, so the load balancer has somewhere to go and
# the health checker has a state change to make; then both are killed, so the
# fully-unhealthy path runs; then they come back.
BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"

ROUNDS=${1:-${CHAOS_ROUNDS:-3}}

bench_init chaos
bench_param rounds "$ROUNDS"
bench_param backends 2
bench_param render_ms 30
bench_build

ORIGIN_A=$(bench_free_port)
ORIGIN_B=$(bench_free_port)
LISTEN_PORT=$(bench_free_port)
ADMIN_PORT=$(bench_free_port)
METRICS_PORT=$(bench_free_port)
CONFIG="$BENCH_DIR/chaos.yaml"

bench_render_config "$BENCH_ROOT/bench/chaos.yaml.tpl" "$CONFIG" \
  "LISTEN=$LISTEN_PORT" "ORIGINA=$ORIGIN_A" "ORIGINB=$ORIGIN_B" \
  "ADMIN=$ADMIN_PORT" "METRICS=$METRICS_PORT" \
  "PIDFILE=$BENCH_DIR/tanod.pid" "UPGRADESOCK=$BENCH_DIR/upgrade.sock"

start_backend() { # name, port
  bench_spawn "$1" "$(bench_bin slow-origin)" "$2" 30
  bench_wait_port 127.0.0.1 "$2" "slow-origin $1"
}
start_backend origin-a "$ORIGIN_A"
start_backend origin-b "$ORIGIN_B"
bench_start_tanod tanod "$CONFIG" "$LISTEN_PORT" "$ADMIN_PORT"
PID=$(bench_pid tanod)
BASE="http://127.0.0.1:$LISTEN_PORT"

# Background traffic for the whole run. Recorded, never asserted as a whole:
# during a total origin outage a 502 is the *correct* answer, so a blanket
# "everything must be 200" would be asserting that Tanod invents responses.
RESULTS="$BENCH_DIR/results"
: > "$RESULTS"
CHAOS_DONE="$BENCH_DIR/done"
rm -f "$CHAOS_DONE"
(
  n=0
  while [ ! -f "$CHAOS_DONE" ]; do
    n=$((n + 1))
    curl -s -o /dev/null -w '%{http_code}\n' --max-time 15 "$BASE/hot/$((n % 3))" >> "$RESULTS" 2>&1
    curl -s -o /dev/null -w '%{http_code}\n' --max-time 15 "$BASE/cold/$n" >> "$RESULTS" 2>&1
  done
) &
TRAFFIC=$!

admin_code() { curl -s -o /dev/null -w '%{http_code}' --max-time 5 "http://127.0.0.1:$ADMIN_PORT$1"; }

# The admin surface, sampled throughout. Sampling during the chaos rather than
# after it is the whole point: an endpoint that recovers once the origin does
# is not an endpoint you can debug an outage with.
ADMIN_SAMPLES="$BENCH_DIR/admin-samples"
: > "$ADMIN_SAMPLES"
(
  while [ ! -f "$CHAOS_DONE" ]; do
    printf '%s %s\n' "$(admin_code /health/live)" "$(admin_code /status)" >> "$ADMIN_SAMPLES"
    sleep 0.5
  done
) &
ADMIN_WATCH=$!

# Privacy under chaos. A response carrying `Set-Cookie` is never shared, and
# "the origin is falling over" is not an exception any configuration reaches.
SESSIONS="$BENCH_DIR/sessions"
: > "$SESSIONS"
collect_sessions() { # count
  for _ in $(seq 1 "$1"); do
    curl -s -D - -o /dev/null --max-time 15 "$BASE/private/x" 2>/dev/null \
      | tr -d '\r' | sed -n 's/^[Ss]et-[Cc]ookie: session=\([^;]*\).*/\1/p' >> "$SESSIONS"
  done
}

echo "chaos, $ROUNDS round(s)"
for round in $(seq 1 "$ROUNDS"); do
  echo
  echo "round $round"
  collect_sessions 5
  sleep 1

  echo "  killing one backend of two"
  bench_stop origin-a
  sleep 2
  LIVE=$(admin_code /health/live)
  echo "    /health/live         $LIVE"
  bench_assert_eq "$LIVE" 200 "liveness with one backend down"
  collect_sessions 5

  echo "  killing the second"
  bench_stop origin-b
  sleep 2
  # A fully unhealthy pool must still be *served* — Tanod does not refuse to
  # pick, because refusing turns a degraded origin into a guaranteed outage
  # and stale-if-error exists for exactly this window.
  DOWN_CODE=$(curl -s -o /dev/null -w '%{http_code}' --max-time 15 "$BASE/cold/down-$round")
  echo "    request while down   $DOWN_CODE"
  bench_alive "$PID" || bench_fail "tanod died when every backend went away"
  case "$DOWN_CODE" in
    502|503|504) ;;
    200) ;;   # a stale hit, which is the point of stale-if-error
    *) bench_fail "an unexpected status while every backend was down: $DOWN_CODE" ;;
  esac

  echo "  reloading config while the origin is down"
  # A reload during an incident is the reload that actually happens, and it is
  # the worst moment for one to be half-applied.
  before=$(wc -l < "$(bench_log tanod)")
  kill -HUP "$PID"
  for attempt in $(seq 1 100); do
    tail -n "+$((before + 1))" "$(bench_log tanod)" | grep -q "config reloaded" && break
    sleep 0.1
  done
  tail -n "+$((before + 1))" "$(bench_log tanod)" | grep -q "config reloaded" \
    || bench_fail "SIGHUP during an origin outage did not reload"

  echo "  bringing both backends back"
  start_backend origin-a "$ORIGIN_A"
  start_backend origin-b "$ORIGIN_B"
  # Recovery has to be automatic. `healthy_after: 1` with a 1s interval means
  # one good probe, so a couple of seconds is generous.
  RECOVERED=0
  for attempt in $(seq 1 200); do
    if [ "$(curl -s -o /dev/null -w '%{http_code}' --max-time 15 "$BASE/cold/up-$round-$attempt")" = "200" ]; then
      RECOVERED=1
      break
    fi
    sleep 0.2
  done
  echo "    serving again        $([ $RECOVERED = 1 ] && echo yes || echo NO)"
  bench_assert_eq "$RECOVERED" 1 "recovery after both backends returned"
  collect_sessions 5
done

touch "$CHAOS_DONE"
wait "$TRAFFIC" "$ADMIN_WATCH" 2>/dev/null

# ------------------------------------------------------------- assertions

echo
echo "after the chaos"
bench_alive "$PID" || bench_fail "tanod did not survive"
bench_assert_no_panics tanod

TOTAL=$(wc -l < "$RESULTS" | tr -d ' ')
OK=$(grep -c '^200$' "$RESULTS" || true)
echo "  requests               $TOTAL"
echo "  200                    $OK"
bench_result requests "$TOTAL"
bench_result succeeded "$OK"
bench_assert_gt "$TOTAL" 50 "requests issued during the chaos"

# The admin surface answered throughout, not only afterwards.
ADMIN_TOTAL=$(wc -l < "$ADMIN_SAMPLES" | tr -d ' ')
ADMIN_OK=$(grep -c '^200 200$' "$ADMIN_SAMPLES" || true)
echo "  admin samples          $ADMIN_OK/$ADMIN_TOTAL answered 200"
bench_result admin_samples "$ADMIN_TOTAL"
bench_result admin_ok "$ADMIN_OK"
bench_assert_gt "$ADMIN_TOTAL" 10 "admin samples taken during the chaos"
bench_assert_eq "$ADMIN_OK" "$ADMIN_TOTAL" \
  "admin samples that answered 200 (the operator surface failed during the incident it exists for)"

# Nothing private was ever shared. Every session cookie must be distinct: one
# repeat is one user handed another user's session.
SESSION_COUNT=$(grep -c . "$SESSIONS" || true)
SESSION_UNIQUE=$(sort -u "$SESSIONS" | grep -c . || true)
echo "  session cookies        $SESSION_UNIQUE distinct of $SESSION_COUNT"
bench_result sessions "$SESSION_COUNT"
bench_result sessions_distinct "$SESSION_UNIQUE"
bench_assert_gt "$SESSION_COUNT" 5 "session responses collected"
bench_assert_eq "$SESSION_UNIQUE" "$SESSION_COUNT" \
  "distinct session cookies (a Set-Cookie response was shared between clients)"

# Capacity came back. This is the assertion that fails when a permit leaked on
# a path that only runs when a backend dies mid-render — invisible during the
# chaos, fatal afterwards.
sleep 1
IN_FLIGHT=$(bench_metric "$METRICS_PORT" 'tanod_origin_in_flight{limiter="global"}')
READY=$(admin_code /health/ready)
FINAL=$(curl -s -o /dev/null -w '%{http_code}' --max-time 15 "$BASE/cold/final")
echo "  in-flight permits      ${IN_FLIGHT:-?}"
echo "  readiness              $READY"
echo "  a fresh request        $FINAL"
bench_result in_flight_at_end "${IN_FLIGHT:-?}"
bench_assert_eq "${IN_FLIGHT%%.*}" 0 "origin permits still held after the chaos (a permit leaked)"
bench_assert_eq "$READY" 200 "readiness after recovery"
bench_assert_eq "$FINAL" 200 "a request after recovery"

echo
bench_print_params
echo
bench_pass "survived $ROUNDS round(s) of backend loss and reloads: $SESSION_UNIQUE/$SESSION_COUNT sessions stayed distinct, the admin surface answered $ADMIN_OK/$ADMIN_TOTAL samples throughout, and no permit leaked"
