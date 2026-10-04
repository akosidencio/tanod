#!/usr/bin/env bash
# The operator surface: liveness, readiness, status, and the drain state that
# makes a zero-downtime restart possible.
#
# The assertion that matters is not "the endpoint answered". It is that during
# a drain, readiness fails **while traffic is still being served correctly** —
# because that gap is the entire mechanism. A readiness endpoint that starts
# failing at the same moment the process stops serving is worth nothing: the
# load balancer learns the instance is gone by having requests fail on it.
BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"

bench_init admin
bench_param render_ms 50
bench_build

ORIGIN_PORT=$(bench_free_port)
LISTEN_PORT=$(bench_free_port)
ADMIN_PORT=$(bench_free_port)
METRICS_PORT=$(bench_free_port)
CONFIG="$BENCH_DIR/admin.yaml"

bench_render_config "$BENCH_ROOT/bench/admin.yaml.tpl" "$CONFIG" \
  "LISTEN=$LISTEN_PORT" "ORIGIN=$ORIGIN_PORT" "ADMIN=$ADMIN_PORT" \
  "METRICS=$METRICS_PORT" "PIDFILE=$BENCH_DIR/tanod.pid" \
  "UPGRADESOCK=$BENCH_DIR/upgrade.sock"

bench_spawn origin "$(bench_bin slow-origin)" "$ORIGIN_PORT" 50
bench_wait_port 127.0.0.1 "$ORIGIN_PORT" "slow-origin"
bench_start_tanod tanod "$CONFIG" "$LISTEN_PORT" "$ADMIN_PORT"
PID=$(bench_pid tanod)

admin_code() { curl -s -o /dev/null -w '%{http_code}' --max-time 5 "http://127.0.0.1:$ADMIN_PORT$1"; }
admin_body() { curl -s --max-time 5 "http://127.0.0.1:$ADMIN_PORT$1"; }
proxy_code() { curl -s -o /dev/null -w '%{http_code}' --max-time 20 "http://127.0.0.1:$LISTEN_PORT/$1"; }

# JSON field reader that does not need jq. The leading quote is what keeps
# `"limit":` from matching inside `"queue_max":` and friends.
json_field() { # body, field
  printf '%s' "$1" | sed -n "s/.*\"$2\":\([0-9a-zA-Z]*\).*/\1/p" | head -1
}

echo "endpoints"
LIVE=$(admin_code /health/live)
READY=$(admin_code /health/ready)
STATUS=$(admin_code /status)
echo "  /health/live           $LIVE"
echo "  /health/ready          $READY"
echo "  /status                $STATUS"
bench_assert_eq "$LIVE" 200 "/health/live"
bench_assert_eq "$READY" 200 "/health/ready on a healthy instance"
bench_assert_eq "$STATUS" 200 "/status"

# An operator surface must not be a way to make the process do unbounded work,
# and it must not accept anything that changes state.
echo
echo "surface"
NOTFOUND=$(admin_code /../../etc/passwd)
UNKNOWN=$(admin_code /admin/does-not-exist)
POST=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 -X POST "http://127.0.0.1:$ADMIN_PORT/status")
echo "  unknown path           $UNKNOWN"
echo "  traversal attempt      $NOTFOUND"
echo "  POST /status           $POST"
bench_assert_eq "$UNKNOWN" 404 "an unknown admin path"
bench_assert_eq "$POST" 405 "a POST to the admin surface"

# The document has to be *live* state, not a snapshot taken at startup.
echo
echo "status content"
BODY=$(admin_body /status)
GEN=$(json_field "$BODY" generation)
FINGERPRINT=$(json_field "$BODY" fingerprint)
SCHEMA=$(json_field "$BODY" schema_version)
echo "  config generation      $GEN"
echo "  config fingerprint     $FINGERPRINT"
echo "  config schema version  $SCHEMA"
bench_assert_eq "$GEN" 1 "config generation before any reload"
[ -n "$FINGERPRINT" ] || bench_fail "the status document has no config fingerprint: $BODY"
bench_assert_eq "$SCHEMA" 1 "config schema version"
case "$BODY" in
  *'"address":"127.0.0.1:'"$ORIGIN_PORT"'"'*) ;;
  *) bench_fail "the status document does not name the configured upstream: $BODY" ;;
esac
case "$BODY" in
  *'"name":"pages"'*) ;;
  *) bench_fail "the status document does not report the route limiter: $BODY" ;;
esac

# A reload has to be visible here, or "did my config apply" stays unanswerable
# from outside the process — which is the question the field exists for.
echo
echo "generation tracks a reload"
sed -i.bak "s/max: 4/max: 6/" "$CONFIG" && rm -f "$CONFIG.bak"
kill -HUP "$PID"
for _ in $(seq 1 100); do
  GEN=$(json_field "$(admin_body /status)" generation)
  [ "$GEN" = "2" ] && break
  sleep 0.1
done
echo "  config generation      $GEN"
FINGERPRINT_AFTER=$(json_field "$(admin_body /status)" fingerprint)
echo "  config fingerprint     $FINGERPRINT_AFTER"
bench_result generation_after_reload "$GEN"
bench_assert_eq "$GEN" 2 "config generation after a SIGHUP"
[ "$FINGERPRINT_AFTER" != "$FINGERPRINT" ] \
  || bench_fail "the config fingerprint did not change when the effective config changed"

# --------------------------------------------------------------- draining

echo
echo "drain (SIGUSR1)"
kill -USR1 "$PID"
for _ in $(seq 1 100); do
  READY=$(admin_code /health/ready)
  [ "$READY" = "503" ] && break
  sleep 0.1
done
LIVE=$(admin_code /health/live)
echo "  /health/ready          $READY"
echo "  /health/live           $LIVE"
bench_result ready_while_draining "$READY"
bench_assert_eq "$READY" 503 "/health/ready while draining"
# Liveness must NOT follow readiness: an orchestrator that killed the process
# here would be undoing the drain it just asked for.
bench_assert_eq "$LIVE" 200 "/health/live while draining"

# The whole point of the window. Requests keep being served correctly while
# the balancer is being told to stop sending them.
echo
echo "traffic during the drain window"
SERVED_OK=0
for i in $(seq 1 20); do
  [ "$(proxy_code "drain/$i")" = "200" ] && SERVED_OK=$((SERVED_OK + 1))
done
echo "  requests served 200    $SERVED_OK/20"
bench_result served_while_draining "$SERVED_OK"
bench_assert_eq "$SERVED_OK" 20 "requests served while draining (the drain window is not a window at all)"
bench_alive "$PID" || bench_fail "SIGUSR1 terminated the process; it must drain without exiting"

BODY=$(admin_body /status)
case "$BODY" in
  *'"draining":true'*) ;;
  *) bench_fail "the status document does not report the drain state: $BODY" ;;
esac
case "$BODY" in
  *'"reason":"sigusr1"'*) ;;
  *) bench_fail "the status document does not report why it is draining: $BODY" ;;
esac

echo
echo "metrics agree with the endpoints"
DRAINING=$(curl -s --max-time 5 "http://127.0.0.1:$METRICS_PORT/metrics" \
  | sed -n 's/^tanod_draining \([0-9]*\)$/\1/p')
GEN_METRIC=$(curl -s --max-time 5 "http://127.0.0.1:$METRICS_PORT/metrics" \
  | sed -n 's/^tanod_config_generation \([0-9]*\)$/\1/p')
FINGERPRINT_METRIC=$(curl -s --max-time 5 "http://127.0.0.1:$METRICS_PORT/metrics" \
  | sed -n 's/^tanod_config_fingerprint \([0-9]*\)$/\1/p')
echo "  tanod_draining          $DRAINING"
echo "  tanod_config_generation $GEN_METRIC"
echo "  config fingerprint        $FINGERPRINT_METRIC"
# A dashboard and an endpoint that disagree send an operator down the wrong
# path at the worst possible moment.
bench_assert_eq "${DRAINING:-x}" 1 "tanod_draining while draining"
bench_assert_eq "${GEN_METRIC:-x}" 2 "tanod_config_generation after a reload"
bench_assert_eq "${FINGERPRINT_METRIC:-x}" "$FINGERPRINT_AFTER" \
  "tanod_config_fingerprint after a reload"

echo
bench_print_params
echo
bench_pass "readiness failed on drain while all 20 requests still succeeded, and generation, fingerprint, drain state and metrics all agreed"
