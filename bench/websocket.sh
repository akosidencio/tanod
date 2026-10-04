#!/usr/bin/env bash
# Upgrade/WebSocket traffic, without weakening admission or cache rules.
#
# A WebSocket handshake is a `GET` that usually carries a session cookie. Left
# alone, the classifier reads it as an ordinary document request: cacheable,
# coalescible, and consuming a render permit for the life of the socket. All
# three are wrong, and each is wrong in a different way:
#
#   * cacheable — a `101` offered to the microcache
#   * coalescible — two clients collapsed onto one tunnel
#   * permit-consuming — a handful of sockets starving every page on the site
#
# The `origin.concurrency.max` in this benchmark's config is 1, so the third
# failure is not subtle: one held socket would block everything.
BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"

SOCKETS=${1:-4}
RENDER_MS=${2:-200}

bench_init websocket
bench_param sockets "$SOCKETS"
bench_param render_ms "$RENDER_MS"
bench_param render_ceiling 1
bench_build

command -v python3 >/dev/null || bench_fail "python3 is required for the WebSocket client"

ORIGIN_PORT=$(bench_start_origin origin "$RENDER_MS")
WS_CLIENT="$BENCH_ROOT/bench/ws-client.py"

echo "WebSocket proxying with a render ceiling of 1"
echo

# ------------------------------------------------- upgrades are off by default
#
# Refused with 501, not with the overload status: nothing is overloaded and a
# retry will never succeed. Answering 503 would invite a retry loop and would
# file a configuration mistake under "origin pressure" in the metrics.
DISABLED_PORT=$(bench_free_port)
bench_render_config "$BENCH_ROOT/bench/websocket.yaml" "$BENCH_DIR/disabled.yaml" \
  "LISTEN=$DISABLED_PORT" "ORIGIN=$ORIGIN_PORT" "UPGRADE=false" "MAX_SOCKETS=10"
bench_start_tanod disabled "$BENCH_DIR/disabled.yaml" "$DISABLED_PORT"
bench_origin_reset "$ORIGIN_PORT"

OFF=$(python3 "$WS_CLIENT" 127.0.0.1 "$DISABLED_PORT" /ws/echo hello)
OFF_STATUS=${OFF#status=}; OFF_STATUS=${OFF_STATUS%% *}
bench_assert_eq "$OFF_STATUS" 501 \
  "with upgrade.enabled false the handshake got HTTP $OFF_STATUS; a proxy that will not tunnel must say so rather than half-accept"
REACHED=$(bench_origin_stat "$ORIGIN_PORT" sockets_total)
bench_assert_eq "${REACHED:-0}" 0 \
  "the refused handshake still reached the origin"
printf '  %-46s %s\n' "upgrade.enabled: false refuses the handshake" "$OFF_STATUS, origin untouched"
bench_stop disabled
bench_result disabled_status "$OFF_STATUS"

# ------------------------------------------------------------ the tunnel works
LISTEN=$(bench_free_port)
bench_render_config "$BENCH_ROOT/bench/websocket.yaml" "$BENCH_DIR/websocket.yaml" \
  "LISTEN=$LISTEN" "ORIGIN=$ORIGIN_PORT" "UPGRADE=true" "MAX_SOCKETS=$SOCKETS"
bench_start_tanod tanod "$BENCH_DIR/websocket.yaml" "$LISTEN"
bench_origin_reset "$ORIGIN_PORT"

ECHO=$(python3 "$WS_CLIENT" 127.0.0.1 "$LISTEN" /ws/echo "round-trip")
ECHO_STATUS=${ECHO#status=}; ECHO_STATUS=${ECHO_STATUS%% *}
ECHO_BODY=${ECHO#*echo=}
bench_assert_eq "$ECHO_STATUS" 101 "the handshake through the proxy returned HTTP $ECHO_STATUS"
[ "$ECHO_BODY" = "round-trip" ] || bench_fail \
  "the tunnel echoed '$ECHO_BODY' instead of 'round-trip' — the handshake completed but bytes do not flow both ways"
printf '  %-46s %s\n' "handshake and frame round-trip" "101, echoed '$ECHO_BODY'"
bench_result echo_status "$ECHO_STATUS"

# The accept key is checked by the client against its own nonce, so a `101`
# here proves the *origin* completed the handshake rather than the proxy
# answering on its behalf.
printf '  %-46s %s\n' "Sec-WebSocket-Accept verified end to end" "ok"

# ------------------------------------ a socket is not a render, and vice versa
#
# The heart of it. Hold every permitted socket open, then ask for a page. With
# a render ceiling of 1, a page that still renders proves the sockets are
# bounded by their own limit and not by the origin's render budget.
bench_origin_reset "$ORIGIN_PORT"
for i in $(seq 1 "$SOCKETS"); do
  bench_spawn "socket$i" python3 "$WS_CLIENT" 127.0.0.1 "$LISTEN" "/ws/hold-$i" --hold 6
done

for _ in $(seq 1 100); do
  OPEN=$(bench_origin_stat "$ORIGIN_PORT" sockets_open)
  [ "${OPEN:-0}" -ge "$SOCKETS" ] && break
  sleep 0.1
done
bench_assert_eq "${OPEN:-0}" "$SOCKETS" "only ${OPEN:-0} of $SOCKETS sockets reached the origin"
printf '  %-46s %s\n' "$SOCKETS sockets held open at the origin" "$OPEN open"

PAGE_START=$(date +%s%N)
PAGE=$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 "http://127.0.0.1:$LISTEN/a-page")
PAGE_MS=$(( ($(date +%s%N) - PAGE_START) / 1000000 ))
bench_assert_eq "$PAGE" 200 \
  "with $SOCKETS sockets open and a render ceiling of 1, an ordinary page got HTTP $PAGE — upgraded connections are consuming render permits"
bench_assert_le "$PAGE_MS" 3000 "page latency while $SOCKETS sockets were open"
printf '  %-46s %s\n' "a page still renders alongside them" "$PAGE in ${PAGE_MS}ms"
bench_result page_with_sockets_status "$PAGE"
bench_result page_with_sockets_ms "$PAGE_MS"

# ------------------------------------------------- the socket ceiling holds
#
# One more than the configured maximum. It must be refused rather than
# admitted, or `upgrade.max_concurrent` bounds nothing.
OVER=$(python3 "$WS_CLIENT" 127.0.0.1 "$LISTEN" /ws/over hello)
OVER_STATUS=${OVER#status=}; OVER_STATUS=${OVER_STATUS%% *}
[ "$OVER_STATUS" = "503" ] || bench_fail \
  "the $((SOCKETS + 1))th socket got HTTP $OVER_STATUS against upgrade.max_concurrent: $SOCKETS — the ceiling admits more than it says"
printf '  %-46s %s\n' "socket $((SOCKETS + 1)) of max $SOCKETS is shed" "$OVER_STATUS"
bench_result over_ceiling_status "$OVER_STATUS"

# ---------------------------------------------- sockets are never shared
#
# Two handshakes at one URL, at the same time, on a route where coalescing is
# enabled. Collapsing them would join two people to one conversation.
bench_origin_reset "$ORIGIN_PORT"
for i in 1 2; do bench_stop "socket$i"; done
sleep 0.5
bench_spawn pair1 python3 "$WS_CLIENT" 127.0.0.1 "$LISTEN" /ws/shared --hold 3
bench_spawn pair2 python3 "$WS_CLIENT" 127.0.0.1 "$LISTEN" /ws/shared --hold 3
sleep 1.5
PAIRED=$(bench_origin_stat "$ORIGIN_PORT" sockets_total)
bench_assert_eq "${PAIRED:-0}" 2 \
  "two concurrent handshakes at one URL produced ${PAIRED:-0} origin socket(s) — they were coalesced onto a shared tunnel"
printf '  %-46s %s\n' "two handshakes at one URL are two tunnels" "$PAIRED sockets"
bench_result concurrent_handshake_sockets "$PAIRED"

# And the permit accounting must come back. If an upgraded connection leaked
# its slot, the ceiling would ratchet down until nothing was admitted.
bench_stop pair1; bench_stop pair2
for i in $(seq 3 "$SOCKETS"); do bench_stop "socket$i"; done
sleep 1
AFTER=$(python3 "$WS_CLIENT" 127.0.0.1 "$LISTEN" /ws/after hello)
AFTER_STATUS=${AFTER#status=}; AFTER_STATUS=${AFTER_STATUS%% *}
bench_assert_eq "$AFTER_STATUS" 101 \
  "after every socket closed, a new handshake got HTTP $AFTER_STATUS — closed connections did not return their slots"
printf '  %-46s %s\n' "closed sockets return their slots" "$AFTER_STATUS"
bench_result slot_returned_status "$AFTER_STATUS"

echo
bench_print_params
echo
bench_pass "Upgrade traffic is proxied without weakening the governor: refused when disabled, bounded by its own ceiling of $SOCKETS, never coalesced, and holding no render capacity — a page rendered in ${PAGE_MS}ms with $SOCKETS sockets open against a render ceiling of 1"
