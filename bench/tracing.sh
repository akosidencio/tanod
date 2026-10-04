#!/usr/bin/env bash
# Request correlation and OpenTelemetry span export.
#
# Correlation and export are separate claims and this asserts them separately,
# because they fail separately: correlation is unconditional and free, export
# is configuration and can be broken by a collector nobody noticed was down.
#
# Every assertion reads a witness other than Tanod's own account of itself:
#
#   * what `traceparent` the *origin* received, from the fixture's
#     /echo-headers — not what the proxy logged that it sent;
#   * what the *collector* received, byte for byte, from a listener that
#     records the request rather than a real collector that would swallow it;
#   * whether the ids in the access log are the same ids that appear in the
#     exported spans, which is the property that makes a log line and a trace
#     joinable at all and the one that silently breaks first.
BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"

command -v python3 >/dev/null || { echo "SKIP: bench/tracing.sh needs python3 for the collector" >&2; exit 0; }

bench_init tracing
bench_param render_ms 20
bench_param sample_mode always
bench_build

ORIGIN_PORT=$(bench_free_port)
LISTEN_PORT=$(bench_free_port)
METRICS_PORT=$(bench_free_port)
COLLECTOR_PORT=$(bench_free_port)
CONFIG="$BENCH_DIR/tracing.yaml"
SPANS="$BENCH_DIR/spans.txt"
: > "$SPANS"

bench_render_config "$BENCH_ROOT/bench/tracing.yaml.tpl" "$CONFIG" \
  "LISTEN=$LISTEN_PORT" "ORIGIN=$ORIGIN_PORT" "METRICS=$METRICS_PORT" \
  "COLLECTOR=$COLLECTOR_PORT" "PIDFILE=$BENCH_DIR/tanod.pid" \
  "UPGRADESOCK=$BENCH_DIR/upgrade.sock"

bench_spawn collector python3 "$BENCH_ROOT/bench/collector.py" "$COLLECTOR_PORT" "$SPANS"
bench_wait_port 127.0.0.1 "$COLLECTOR_PORT" "otlp collector"
bench_spawn origin "$(bench_bin slow-origin)" "$ORIGIN_PORT" 20
bench_wait_port 127.0.0.1 "$ORIGIN_PORT" "slow-origin"
bench_start_tanod tanod "$CONFIG" "$LISTEN_PORT"

echoed() { # path, extra curl args...
  local path=$1; shift
  curl -s --max-time 10 "$@" "http://127.0.0.1:$LISTEN_PORT/echo-headers$path"
}
field() { printf '%s' "$1" | sed -n "s/.*\"$2\":\"\([^\"]*\)\".*/\1/p"; }

INBOUND='00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01'

# ------------------------------------------------- correlation, unconditional

echo "propagation to the origin"
FRESH=$(echoed /a)
FRESH_TP=$(field "$FRESH" traceparent)
echo "  no inbound context     $FRESH_TP"
case "$FRESH_TP" in
  00-????????????????????????????????-????????????????-0?) ;;
  *) bench_fail "the origin did not receive a well-formed traceparent: '$FRESH_TP'" ;;
esac

CONT=$(echoed /b -H "traceparent: $INBOUND" -H 'tracestate: vendor=abc')
CONT_TP=$(field "$CONT" traceparent)
CONT_TS=$(field "$CONT" tracestate)
echo "  trusted inbound        $CONT_TP"
echo "  tracestate             $CONT_TS"
# The trace id must survive the hop, or every trace has a hole where Tanod is.
case "$CONT_TP" in
  00-4bf92f3577b34da6a3ce929d0e0e4736-*) ;;
  *) bench_fail "a believed trace id was not continued: '$CONT_TP'" ;;
esac
# And the span id must NOT: forwarding the caller's own span id would make the
# origin's spans siblings of the caller's rather than children of the fetch.
case "$CONT_TP" in
  *-00f067aa0ba902b7-*) bench_fail "the caller's span id was forwarded verbatim: '$CONT_TP'" ;;
esac
[ "$CONT_TS" = "vendor=abc" ] || bench_fail "tracestate from a trusted peer was not forwarded: '$CONT_TS'"

# A malformed context must cost a *continued* trace, never the request.
GARBAGE=$(echoed /c -H 'traceparent: this-is-not-a-traceparent' -H 'tracestate: leak=yes')
GARBAGE_TP=$(field "$GARBAGE" traceparent)
GARBAGE_TS=$(field "$GARBAGE" tracestate)
echo "  malformed inbound      $GARBAGE_TP"
case "$GARBAGE_TP" in
  00-????????????????????????????????-????????????????-0?) ;;
  *) bench_fail "a malformed inbound context broke propagation: '$GARBAGE_TP'" ;;
esac
case "$GARBAGE_TP" in
  00-4bf92f3577b34da6a3ce929d0e0e4736-*) bench_fail "a malformed context was somehow continued" ;;
esac
# Vendor state attached to a context that was thrown away must not survive
# into the origin's view of the request.
[ -z "$GARBAGE_TS" ] || bench_fail "tracestate from an ignored context reached the origin: '$GARBAGE_TS'"

# An inbound context that is *not* believed must be ignored — and the setting
# that decides it must be reloadable, because turning trace ingestion off is
# something you do during an incident rather than at a restart.
echo
echo "trust_incoming: never, applied by SIGHUP"
PID=$(bench_pid tanod)
sed -i.bak 's/    trust_incoming: .*/    trust_incoming: never/' "$CONFIG" 2>/dev/null || true
grep -q 'trust_incoming' "$CONFIG" \
  || sed -i.bak 's/^  tracing:$/  tracing:\n    trust_incoming: never/' "$CONFIG"
rm -f "$CONFIG.bak"
before=$(wc -l < "$(bench_log tanod)")
kill -HUP "$PID"
for _ in $(seq 1 100); do
  tail -n "+$((before + 1))" "$(bench_log tanod)" | grep -q "config reloaded" && break
  sleep 0.1
done
tail -n "+$((before + 1))" "$(bench_log tanod)" | grep -q "config reloaded" \
  || bench_fail "SIGHUP did not apply telemetry.tracing.trust_incoming: $(tail -n 3 "$(bench_log tanod)")"

IGNORED=$(echoed /d -H "traceparent: $INBOUND" -H 'tracestate: vendor=abc')
IGNORED_TP=$(field "$IGNORED" traceparent)
IGNORED_TS=$(field "$IGNORED" tracestate)
echo "  inbound now ignored    $IGNORED_TP"
case "$IGNORED_TP" in
  00-4bf92f3577b34da6a3ce929d0e0e4736-*)
    bench_fail "an inbound trace was believed with trust_incoming: never" ;;
esac
case "$IGNORED_TP" in
  00-????????????????????????????????-????????????????-0?) ;;
  *) bench_fail "refusing an inbound context broke propagation: '$IGNORED_TP'" ;;
esac
[ -z "$IGNORED_TS" ] || bench_fail "tracestate survived an ignored context: '$IGNORED_TS'"

echo
echo "access log"
LOG=$(bench_log tanod)
LOG_TRACE=$(grep -o '"trace_id":"[0-9a-f]\{32\}"' "$LOG" | tail -1 | sed 's/.*:"//;s/"//')
LOG_GEN=$(grep -o '"generation":[0-9]*' "$LOG" | tail -1 | sed 's/.*://')
echo "  trace_id               $LOG_TRACE"
echo "  generation             $LOG_GEN"
[ -n "$LOG_TRACE" ] || bench_fail "no trace_id in the access log; correlation is not unconditional"
# The reload above bumped it, so this also proves the generation on a log line
# tracks the config that actually served the request.
bench_assert_eq "${LOG_GEN:-x}" 2 "config generation in the access log after a reload"
grep -q '"trace_continued":true' "$LOG" \
  || bench_fail "the access log never recorded a continued trace"

# --------------------------------------------------------------- span export

echo
echo "span export"
for _ in $(seq 1 60); do
  [ -s "$SPANS" ] && break
  sleep 0.2
done
[ -s "$SPANS" ] || bench_fail "the collector received nothing within 12s"

BATCHES=$(wc -l < "$SPANS" | tr -d ' ')
echo "  batches received       $BATCHES"
bench_result batches "$BATCHES"

# The OTLP/HTTP contract, not just "some bytes arrived". A collector rejects a
# batch outright on either of these.
grep -q '^application/json' "$SPANS" \
  || bench_fail "the exporter did not send Content-Type: application/json"
grep -q '"resourceSpans"' "$SPANS" || bench_fail "the payload is not an OTLP trace document"
grep -q '"service.name"' "$SPANS" || bench_fail "no service.name resource attribute"
grep -q 'tanod-bench' "$SPANS" || bench_fail "the configured service name is not in the payload"
# Protobuf JSON renders 64-bit fields as strings. A number here is rejected for
# the whole batch, and it is exactly the mistake a hand-written encoder makes.
grep -q '"startTimeUnixNano":"[0-9]' "$SPANS" \
  || bench_fail "timestamps are not string-encoded; a collector would reject the batch"

SERVER_SPANS=$(grep -o '"kind":2' "$SPANS" | wc -l | tr -d ' ')
CLIENT_SPANS=$(grep -o '"kind":3' "$SPANS" | wc -l | tr -d ' ')
echo "  server spans           $SERVER_SPANS"
echo "  origin-fetch spans     $CLIENT_SPANS"
bench_result server_spans "$SERVER_SPANS"
bench_result origin_spans "$CLIENT_SPANS"
bench_assert_gt "$SERVER_SPANS" 2 "server spans exported"
# Without the child span an origin-latency number has nothing to hang off, and
# "the origin was slow" cannot be told apart from "we queued for two seconds".
bench_assert_gt "$CLIENT_SPANS" 2 "origin-fetch spans exported"
grep -q '"parentSpanId"' "$SPANS" || bench_fail "no span was nested under another"

# The join that makes any of this useful: an id in the log must be findable in
# the exported trace. This is the property that breaks silently.
echo "  log id present in spans"
grep -q "$LOG_TRACE" "$SPANS" \
  || bench_fail "trace_id $LOG_TRACE from the access log appears in no exported span"

# And the believed trace id must be the one exported, not a fresh one.
grep -q '4bf92f3577b34da6a3ce929d0e0e4736' "$SPANS" \
  || bench_fail "the continued trace was exported under a different trace id"

echo
echo "metrics"
EXPORTED=$(curl -s --max-time 5 "http://127.0.0.1:$METRICS_PORT/metrics" \
  | sed -n 's/^tanod_spans_total{outcome="exported"} \([0-9]*\)$/\1/p')
FAILED=$(curl -s --max-time 5 "http://127.0.0.1:$METRICS_PORT/metrics" \
  | sed -n 's/^tanod_spans_total{outcome="export_failed"} \([0-9]*\)$/\1/p')
DROPPED=$(curl -s --max-time 5 "http://127.0.0.1:$METRICS_PORT/metrics" \
  | sed -n 's/^tanod_spans_total{outcome="dropped"} \([0-9]*\)$/\1/p')
echo "  exported               ${EXPORTED:-0}"
echo "  export_failed          ${FAILED:-0}"
echo "  dropped                ${DROPPED:-0}"
bench_result spans_exported "${EXPORTED:-0}"
bench_assert_gt "${EXPORTED:-0}" 2 "tanod_spans_total{exported}"
bench_assert_eq "${FAILED:-0}" 0 "tanod_spans_total{export_failed} against a healthy collector"
bench_assert_eq "${DROPPED:-0}" 0 "tanod_spans_total{dropped} at this traffic level"

# ------------------------------------------ telemetry is never load-bearing

echo
echo "a dead collector does not affect traffic"
bench_stop collector
BEFORE=$(date +%s%N)
CODES=""
for _ in $(seq 1 15); do
  CODES="$CODES$(curl -s -o /dev/null -w '%{http_code} ' --max-time 10 \
    "http://127.0.0.1:$LISTEN_PORT/render")"
done
ELAPSED_MS=$(( ($(date +%s%N) - BEFORE) / 1000000 ))
OK=$(printf '%s' "$CODES" | tr ' ' '\n' | grep -c '^200$' || true)
echo "  requests 200           $OK/15"
echo "  wall time              ${ELAPSED_MS}ms"
bench_result requests_with_dead_collector "$OK"
bench_result ms_with_dead_collector "$ELAPSED_MS"
# The claim in the module docs: export failure is counted and logged, never
# propagated. If a request path ever awaited the exporter this would stall on
# the connect timeout instead.
bench_assert_eq "$OK" 15 "requests served while the collector is unreachable"
bench_assert_le "$ELAPSED_MS" 8000 "wall time for 15 requests with a dead collector"
bench_alive "$(bench_pid tanod)" || bench_fail "tanod died when its collector went away"

echo
bench_print_params
echo
bench_pass "traces propagated and continued correctly, $SERVER_SPANS server and $CLIENT_SPANS origin spans exported and joinable to the access log, and a dead collector cost nothing"
