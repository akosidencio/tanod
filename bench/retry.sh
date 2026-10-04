#!/usr/bin/env bash
# One of two backends is killed outright, so every request routed to it is a
# connect failure. Three proxies see the same dead backend with three retry
# settings, which is what separates "retries work" from "retries are bounded".
#
#   off       retries disabled            — about half the requests fail
#   generous  budget 100% of traffic      — almost none fail
#   tight     budget floor of one retry   — the budget refuses the rest
#
# The third is the one that matters. A retry policy that always retries is not
# a feature, it is an amplifier pointed at an origin that is already failing.
BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"

REQUESTS=${1:-40}
RENDER_MS=${2:-20}

bench_init retry
bench_param requests "$REQUESTS"
bench_param render_ms "$RENDER_MS"
bench_build

ORIGIN_A=$(bench_free_port)
ORIGIN_B=$(bench_free_port)
bench_spawn origin-a "$(bench_bin slow-origin)" "$ORIGIN_A" "$RENDER_MS"
bench_spawn origin-b "$(bench_bin slow-origin)" "$ORIGIN_B" "$RENDER_MS"
bench_wait_port 127.0.0.1 "$ORIGIN_A" "slow-origin a"
bench_wait_port 127.0.0.1 "$ORIGIN_B" "slow-origin b"

# name, retry_enabled, budget_percent, budget_min -> sets LISTEN_<name>/METRICS_<name>
start_proxy() {
  local name=$1 enabled=$2 percent=$3 minimum=$4 listen metrics config
  listen=$(bench_free_port)
  metrics=$(bench_free_port)
  config="$BENCH_DIR/retry-$name.yaml"
  bench_render_config "$BENCH_ROOT/bench/retry.yaml" "$config" \
    "LISTEN=$listen" "ORIGIN_A=$ORIGIN_A" "ORIGIN_B=$ORIGIN_B" \
    "METRICS=$metrics" "RETRY=$enabled" "PERCENT=$percent" "MIN=$minimum"
  bench_start_tanod "tanod-$name" "$config" "$listen" "$metrics"
  eval "LISTEN_$name=$listen"
  eval "METRICS_$name=$metrics"
}

start_proxy off false 10 3
start_proxy generous true 100 100
start_proxy tight true 0 1

# The backend dies *after* every proxy has started and resolved it, which is
# the case a retry covers: a process that was there and is not any more.
bench_stop origin-b

# count 200s out of REQUESTS through one proxy
drive_ok() { # listen_port
  seq 1 "$REQUESTS" | xargs -P 8 -I{} \
    curl -s -o /dev/null -w '%{http_code}\n' --max-time 20 \
    "http://127.0.0.1:$1/p/{}" 2>/dev/null | grep -c '^200$'
}

retries_allowed() { # metrics_port
  curl -s --max-time 5 "http://127.0.0.1:$1/metrics" \
    | sed -n 's/^tanod_origin_retries_total{[^}]*outcome="allowed"[^}]*} \([0-9]*\)$/\1/p' \
    | awk '{ n += $1 } END { print n + 0 }'
}

retries_refused() { # metrics_port
  curl -s --max-time 5 "http://127.0.0.1:$1/metrics" \
    | sed -n 's/^tanod_origin_retries_total{[^}]*outcome="budget_exhausted"[^}]*} \([0-9]*\)$/\1/p' \
    | awk '{ n += $1 } END { print n + 0 }'
}

OK_OFF=$(drive_ok "$(bench_get LISTEN_off)")
OK_GEN=$(drive_ok "$(bench_get LISTEN_generous)")
OK_TIGHT=$(drive_ok "$(bench_get LISTEN_tight)")

ALLOWED_GEN=$(retries_allowed "$(bench_get METRICS_generous)")
ALLOWED_TIGHT=$(retries_allowed "$(bench_get METRICS_tight)")
REFUSED_TIGHT=$(retries_refused "$(bench_get METRICS_tight)")

echo "one of two backends is dead; $REQUESTS requests through each proxy"
echo
echo "  retries off        $OK_OFF / $REQUESTS served"
echo "  budget 100%        $OK_GEN / $REQUESTS served   ($ALLOWED_GEN retries spent)"
echo "  budget floor of 1  $OK_TIGHT / $REQUESTS served   ($ALLOWED_TIGHT spent, $REFUSED_TIGHT refused)"
echo
bench_print_params
echo

bench_result served_without_retries "$OK_OFF"
bench_result served_with_budget "$OK_GEN"
bench_result served_with_tight_budget "$OK_TIGHT"
bench_result retries_spent "$ALLOWED_GEN"
bench_result retries_refused "$REFUSED_TIGHT"

# Without retries, everything round-robined onto the dead backend fails.
bench_assert_gt "$OK_OFF" 0 "requests served with retries off"
bench_assert_le "$OK_OFF" $(( REQUESTS * 4 / 5 )) "requests served with retries off"

# With budget to spare, a retry re-enters peer selection and usually lands on
# the backend that is still alive. Not always: selection is round-robin and a
# retry can be handed the dead backend a second time, which then exhausts
# `max_attempts: 2`. That is the honest result rather than a flaw in the
# harness — a retry is a second chance, not a guarantee — and it is why the
# assertion is a large improvement rather than a perfect score.
bench_assert_gt "$OK_GEN" $(( REQUESTS * 3 / 4 )) "requests served with a generous budget"
bench_assert_gt "$OK_GEN" "$OK_OFF" "a budget must serve more than no retries at all"
bench_assert_gt "$ALLOWED_GEN" 0 "retries actually spent"

# The property the budget exists for. A floor of one retry per window must not
# rescue half the traffic, however much of it is failing.
bench_assert_gt "$REFUSED_TIGHT" 0 "retries refused for lack of budget"
bench_assert_le "$OK_TIGHT" $(( OK_OFF + 4 )) "requests rescued by a one-retry budget"

bench_assert_no_panics tanod-off
bench_assert_no_panics tanod-generous
bench_assert_no_panics tanod-tight
bench_pass "a dead backend cost $(( REQUESTS - OK_OFF )) requests with retries off and $(( REQUESTS - OK_GEN )) with a budget, while a one-retry budget refused $REFUSED_TIGHT retries rather than amplifying the failure"
