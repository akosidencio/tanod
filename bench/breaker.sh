#!/usr/bin/env bash
# Two backends. One starts failing every render while its health endpoint keeps
# answering 200 — the failure an active probe cannot express. The origins count
# what they were actually asked to render, so "traffic stopped arriving" is
# measured at the backend rather than inferred from the proxy's own metrics.
BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"

REQUESTS=${1:-40}
RENDER_MS=${2:-20}

bench_init breaker
bench_param requests "$REQUESTS"
bench_param render_ms "$RENDER_MS"
bench_build

ORIGIN_A=$(bench_free_port)
ORIGIN_B=$(bench_free_port)
LISTEN_PORT=$(bench_free_port)
METRICS_PORT=$(bench_free_port)
ADMIN_PORT=$(bench_free_port)
CONFIG="$BENCH_DIR/breaker.yaml"
bench_render_config "$BENCH_ROOT/bench/breaker.yaml" "$CONFIG" \
  "LISTEN=$LISTEN_PORT" "ORIGIN_A=$ORIGIN_A" "ORIGIN_B=$ORIGIN_B" \
  "METRICS=$METRICS_PORT" "ADMIN=$ADMIN_PORT"

bench_spawn origin-a "$(bench_bin slow-origin)" "$ORIGIN_A" "$RENDER_MS"
bench_spawn origin-b "$(bench_bin slow-origin)" "$ORIGIN_B" "$RENDER_MS"
bench_wait_port 127.0.0.1 "$ORIGIN_A" "slow-origin a"
bench_wait_port 127.0.0.1 "$ORIGIN_B" "slow-origin b"
bench_start_tanod tanod "$CONFIG" "$LISTEN_PORT" "$METRICS_PORT"

# Both backends must be passing before anything is asserted, or the first
# phase measures the health checker's startup instead of the breaker.
bench_wait_http "http://127.0.0.1:$ADMIN_PORT/health/ready" "tanod admin"
for _ in $(seq 1 100); do
  HEALTHY=$(curl -s --max-time 5 "http://127.0.0.1:$METRICS_PORT/metrics" \
    | grep -c '^tanod_upstream_healthy{[^}]*} 1$')
  [ "${HEALTHY:-0}" -ge 2 ] && break
  sleep 0.1
done
bench_assert_eq "${HEALTHY:-0}" 2 "backends passing their health check at start"

drive() { # count
  seq 1 "$1" | xargs -P 8 -I{} \
    curl -s -o /dev/null --max-time 20 "http://127.0.0.1:$LISTEN_PORT/p/{}" 2>/dev/null
}

# ---- phase 1: both healthy, both take work
bench_origin_reset "$ORIGIN_A"
bench_origin_reset "$ORIGIN_B"
drive "$REQUESTS"
BASE_A=$(bench_origin_stat "$ORIGIN_A" total)
BASE_B=$(bench_origin_stat "$ORIGIN_B" total)
echo "healthy:  origin-a rendered ${BASE_A:-?}, origin-b rendered ${BASE_B:-?} of $REQUESTS"
bench_assert_gt "${BASE_A:-0}" 0 "origin-a share while healthy"
bench_assert_gt "${BASE_B:-0}" 0 "origin-b share while healthy"

# ---- phase 2: origin-b fails every render, /healthz keeps saying ok
curl -fsS -o /dev/null --max-time 5 "http://127.0.0.1:$ORIGIN_B/__fail" \
  || bench_fail "origin-b did not accept /__fail"
drive "$REQUESTS"

# Prove the premise before the conclusion: if the health check had noticed,
# this would be a test of health checking and not of the breaker.
STILL_HEALTHY=$(curl -s --max-time 5 "http://127.0.0.1:$METRICS_PORT/metrics" \
  | grep -c '^tanod_upstream_healthy{[^}]*} 1$')
bench_assert_eq "${STILL_HEALTHY:-0}" 2 "backends still passing their health check"

EJECTED=$(curl -s --max-time 5 "http://127.0.0.1:$METRICS_PORT/metrics" \
  | grep -c "^tanod_upstream_ejected{upstream=\"127.0.0.1:$ORIGIN_B\"} 1$")
bench_assert_eq "${EJECTED:-0}" 1 "origin-b ejected while its health check passes"

# ---- phase 3: with it ejected, the good backend takes the traffic
bench_origin_reset "$ORIGIN_A"
bench_origin_reset "$ORIGIN_B"
drive "$REQUESTS"
AFTER_A=$(bench_origin_stat "$ORIGIN_A" total)
AFTER_B=$(bench_origin_stat "$ORIGIN_B" total)
echo "ejected:  origin-a rendered ${AFTER_A:-?}, origin-b rendered ${AFTER_B:-?} of $REQUESTS"

bench_assert_gt "${AFTER_A:-0}" $((REQUESTS - 5)) "origin-a renders while origin-b is ejected"
# Not necessarily zero: the ejected backend is deliberately given one recovery
# probe per `open_for`, and a long enough phase spans one. What must not happen
# is it taking its old share.
bench_assert_le "${AFTER_B:-999}" 4 "origin-b renders while ejected"

# ---- phase 4: it recovers, and the probe puts it back in rotation
#
# This is the half-open path, and it is the part a breaker most often gets
# wrong: a backend that is never picked never produces the observation that
# would close its breaker, so one blip ejects it forever.
curl -fsS -o /dev/null --max-time 5 "http://127.0.0.1:$ORIGIN_B/__heal" \
  || bench_fail "origin-b did not accept /__heal"

RECOVERED=0
for _ in $(seq 1 60); do
  drive 4
  if curl -s --max-time 5 "http://127.0.0.1:$METRICS_PORT/metrics" \
    | grep -q "^tanod_upstream_ejected{upstream=\"127.0.0.1:$ORIGIN_B\"} 0$"; then
    RECOVERED=1
    break
  fi
  sleep 0.5
done
bench_assert_eq "$RECOVERED" 1 "origin-b came back into rotation after it healed"

bench_origin_reset "$ORIGIN_A"
bench_origin_reset "$ORIGIN_B"
drive "$REQUESTS"
BACK_A=$(bench_origin_stat "$ORIGIN_A" total)
BACK_B=$(bench_origin_stat "$ORIGIN_B" total)
echo "recovered: origin-a rendered ${BACK_A:-?}, origin-b rendered ${BACK_B:-?} of $REQUESTS"
bench_assert_gt "${BACK_B:-0}" 0 "origin-b share after recovery"

bench_print_params
echo

bench_result healthy_split "${BASE_A:-?}/${BASE_B:-?}"
bench_result ejected_split "${AFTER_A:-?}/${AFTER_B:-?}"
bench_result recovered_split "${BACK_A:-?}/${BACK_B:-?}"

bench_assert_no_panics tanod
bench_pass "a backend passing its health check and failing every render was ejected (${AFTER_B:-?} of $REQUESTS afterwards), then returned to rotation on a recovery probe once it healed (${BACK_B:-?} of $REQUESTS)"
