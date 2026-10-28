#!/usr/bin/env bash
# Can a client tell Tanod who it is?
#
# `X-Forwarded-For` and `X-Forwarded-Proto` are set by whoever spoke to the
# proxy last. On a public listener that is the client, and believing them
# hands out two things:
#
#   * **A cache partition the client controls.** The scheme is part of the
#     cache key, so a client that can set `X-Forwarded-Proto` mints a fresh key
#     — and therefore a fresh origin render — per request. That is the exact
#     origin-work amplification Tanod exists to prevent, delivered by
#     Tanod.
#   * **A forged identity.** The address Tanod passes upstream is what the
#     origin's own rate limits, audit logs and geo rules read.
#
# Every check below runs twice, once with the peer untrusted and once with it
# trusted, because a benchmark that only exercises the safe configuration
# cannot tell "ignored the header" from "never looked at it".
#
# The origin-facing half covers every header a framework builds a public URL
# or an identity from — host, port, prefix, original-URL overrides, CDN
# client-IP headers — and proves through `/reflect/` (a cacheable page whose
# body holds an absolute URL built from them) that none of them can poison a
# shared response.
BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"

RENDER_MS=${1:-100}

bench_init forwarded
bench_param render_ms "$RENDER_MS"
bench_build

ORIGIN_PORT=$(bench_start_origin origin "$RENDER_MS")

# Start a proxy with the given trust configuration and leave its port in PORT.
start() { # name, from-list, client_ip source, scheme source [, host source [, listen address]]
  PORT=$(bench_free_port)
  bench_render_config "$BENCH_ROOT/bench/forwarded.yaml" "$BENCH_DIR/$1.yaml" \
    "LISTEN=$PORT" "ORIGIN=$ORIGIN_PORT" "TRUST=$2" \
    "CLIENT_IP_SOURCE=$3" "SCHEME_SOURCE=$4" "HOST_SOURCE=${5:-none}" \
    "LISTEN_ADDR=${6:-127.0.0.1}"
  bench_start_tanod "$1" "$BENCH_DIR/$1.yaml" "$PORT"
}

check() { # label, observed
  printf '  %-48s %s\n' "$1" "$2"
}

# What the origin says it received, for one JSON field.
received() { # port, field, curl args...
  local port=$1 field=$2; shift 2
  curl -s --max-time 10 "$@" "http://127.0.0.1:$port/echo-headers/x" \
    | sed -n "s/.*\"$field\":\"\([^\"]*\)\".*/\1/p"
}

echo "forwarded headers, with the peer untrusted and then trusted"
echo

# ================================================================ untrusted
start untrusted "[]" x_forwarded x_forwarded
UNTRUSTED=$PORT

# The client claims to be somebody else, over TLS it never used.
GOT_FOR=$(received "$UNTRUSTED" x_forwarded_for \
  -H 'X-Forwarded-For: 9.9.9.9' -H 'X-Forwarded-Proto: https')
GOT_PROTO=$(received "$UNTRUSTED" x_forwarded_proto \
  -H 'X-Forwarded-For: 9.9.9.9' -H 'X-Forwarded-Proto: https')

[ "$GOT_FOR" = "127.0.0.1" ] || bench_fail \
  "an untrusted client claimed X-Forwarded-For: 9.9.9.9 and the origin received '$GOT_FOR'; the address the origin sees must be the one Tanod observed, not the one the client chose"
printf '  %-48s %s\n' "untrusted X-Forwarded-For is replaced" "origin saw $GOT_FOR"
bench_result untrusted_xff "$GOT_FOR"

[ "$GOT_PROTO" = "http" ] || bench_fail \
  "an untrusted client claimed X-Forwarded-Proto: https and the origin received '$GOT_PROTO'; a framework generating absolute URLs from this would emit https links for a plaintext session"
printf '  %-48s %s\n' "untrusted X-Forwarded-Proto is replaced" "origin saw $GOT_PROTO"
bench_result untrusted_xfp "$GOT_PROTO"

# An RFC 7239 `Forwarded` header is a claim under a different name. Tanod
# does not emit one, so anything arriving under that name is unvouched-for and
# must not reach the origin.
GOT_FWD=$(received "$UNTRUSTED" forwarded -H 'Forwarded: for=9.9.9.9;proto=https')
[ -z "$GOT_FWD" ] || bench_fail \
  "a client-supplied Forwarded header reached the origin as '$GOT_FWD'"
printf '  %-48s %s\n' "client-supplied Forwarded is stripped" "origin saw nothing"

# And the cache-key half. Two requests for one path, one of them claiming
# https: if the claim reached the key they would be two entries and two
# renders, which is a render per header value an attacker cares to invent.
bench_origin_reset "$ORIGIN_PORT"
curl -s -o /dev/null --max-time 10 "http://127.0.0.1:$UNTRUSTED/p/key"
curl -s -o /dev/null --max-time 10 -H 'X-Forwarded-Proto: https' "http://127.0.0.1:$UNTRUSTED/p/key"
UNTRUSTED_RENDERS=$(bench_origin_stat "$ORIGIN_PORT" total)
bench_assert_eq "$UNTRUSTED_RENDERS" 1 \
  "a spoofed X-Forwarded-Proto produced $UNTRUSTED_RENDERS origin renders for one URL — the client controls a cache-key dimension and can force a render per request"
printf '  %-48s %s\n' "a spoofed scheme cannot split the cache key" "$UNTRUSTED_RENDERS render"
bench_result untrusted_key_renders "$UNTRUSTED_RENDERS"

# The pathological version: a fresh invented scheme each time. The key must be
# unmoved by all of them.
bench_origin_reset "$ORIGIN_PORT"
for claim in https ftp gopher HTTPS wss "http " httpss; do
  curl -s -o /dev/null --max-time 10 -H "X-Forwarded-Proto: $claim" \
    "http://127.0.0.1:$UNTRUSTED/p/flood"
done
FLOOD_RENDERS=$(bench_origin_stat "$ORIGIN_PORT" total)
bench_assert_eq "$FLOOD_RENDERS" 1 \
  "seven invented scheme values produced $FLOOD_RENDERS origin renders for one URL"
printf '  %-48s %s\n' "7 invented schemes, one URL" "$FLOOD_RENDERS render"
bench_result scheme_flood_renders "$FLOOD_RENDERS"

# ---------------------------------------------- origin-facing identity/URL
#
# Every header a framework might read a public URL or an identity from, all
# forged at once and the identity ones repeated. The origin must see Tanod's
# own statement of each, once, or nothing.
ECHO=$(curl -s --max-time 10 "http://127.0.0.1:$UNTRUSTED/echo-headers/x" \
  -H 'X-Forwarded-For: 9.9.9.9' -H 'X-Forwarded-For: 8.8.8.8' \
  -H 'X-Real-IP: 9.9.9.9' -H 'X-Forwarded-Host: evil.example' \
  -H 'X-Forwarded-Host: evil2.example' -H 'X-Forwarded-Port: 8443' \
  -H 'X-Forwarded-Prefix: /evil' -H 'X-Original-Host: evil.example' \
  -H 'X-Original-URL: /admin' -H 'CF-Connecting-IP: 9.9.9.9')
field() { printf '%s' "$ECHO" | sed -n "s/.*\"$1\":\"\([^\"]*\)\".*/\1/p"; }
expect_field() { # field, expected, why
  local got; got=$(field "$1")
  [ "$got" = "$2" ] || bench_fail "origin received $1='$got', expected '$2': $3"
}
expect_field x_forwarded_for 127.0.0.1 "repeated forged X-Forwarded-For must collapse to the observed peer"
expect_field x_real_ip 127.0.0.1 "X-Real-IP is an identity claim and must carry Tanod's conclusion"
expect_field x_forwarded_host "127.0.0.1:$UNTRUSTED" "an untrusted X-Forwarded-Host must be replaced by the requested authority"
expect_field x_forwarded_port "$UNTRUSTED" "X-Forwarded-Port must describe the requested authority"
expect_field x_forwarded_prefix "" "X-Forwarded-Prefix is never interpreted, so never forwarded"
expect_field x_original_host "" "X-Original-Host overrides the URL the origin builds"
expect_field x_original_url "" "X-Original-URL overrides the path some frameworks route on"
expect_field cf_connecting_ip "" "a CDN identity header from an untrusted peer is forged"
check "forged host/port/prefix/identity are restated" "host=$(field x_forwarded_host) port=$(field x_forwarded_port)"
bench_result untrusted_xfh "$(field x_forwarded_host)"

# The poisoning test. One request with a forged host and scheme fills the
# cache; the next ordinary request is a hit. If either forged header reached
# the origin unkeyed, the stored canonical URL names evil.example or https.
bench_origin_reset "$ORIGIN_PORT"
POISON=$(curl -s --max-time 10 -H 'X-Forwarded-Host: evil.example' \
  -H 'X-Forwarded-Proto: https' -H 'X-Original-Host: evil.example' \
  "http://127.0.0.1:$UNTRUSTED/reflect/1")
VICTIM=$(curl -s --max-time 10 "http://127.0.0.1:$UNTRUSTED/reflect/1")
case "$POISON$VICTIM" in *evil*|*https:*) bench_fail \
  "a forged host reached a shared response: attacker got '$POISON', next visitor got '$VICTIM'" ;; esac
case "$VICTIM" in *"href=\"http://127.0.0.1:$UNTRUSTED/reflect/1\""*) ;; *) bench_fail \
  "the cached canonical URL is '$VICTIM', not the requested authority" ;; esac
REFLECT_RENDERS=$(bench_origin_stat "$ORIGIN_PORT" total)
bench_assert_eq "$REFLECT_RENDERS" 1 \
  "a forged host split the cache: $REFLECT_RENDERS renders for one URL"
check "a forged host cannot poison a shared page" "$REFLECT_RENDERS render, canonical unchanged"
bench_result untrusted_reflect_renders "$REFLECT_RENDERS"

# Malformed, oversized and non-ASCII claims: refused or ignored, never a 5xx.
for claim in "evil.example/path" "a@b.example" "$(printf 'x%.0s' $(seq 1 300)).example" "shop example"; do
  CODE=$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 \
    -H "X-Forwarded-Host: $claim" -H "X-Forwarded-For: $claim" \
    "http://127.0.0.1:$UNTRUSTED/echo-headers/x")
  [ "$CODE" = 200 ] || bench_fail "a malformed forwarding value produced HTTP $CODE"
done
CODE=$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 \
  -H "$(printf 'X-Forwarded-Host: sh\xc3\xb6p.example')" "http://127.0.0.1:$UNTRUSTED/echo-headers/x")
case "$CODE" in 200|400) ;; *) bench_fail "a non-ASCII X-Forwarded-Host produced HTTP $CODE" ;; esac
check "malformed and non-ASCII values are survivable" "ok"

# HTTP/1.0 without Host: no authority to state, so none is invented.
HTTP10=$(printf 'GET /echo-headers/x HTTP/1.0\r\nX-Forwarded-Host: evil.example\r\n\r\n' \
  | curl -s --max-time 10 "telnet://127.0.0.1:$UNTRUSTED" 2>/dev/null || true)
case "$HTTP10" in *evil.example*) bench_fail "a Host-less HTTP/1.0 request forwarded a forged host" ;; esac
check "Host-less HTTP/1.0 forwards no invented host" "$(printf '%s' "$HTTP10" | head -1 | tr -d '\r')"
bench_stop untrusted

# ================================================================== trusted
start trusted '["127.0.0.1/32"]' x_forwarded x_forwarded
TRUSTED=$PORT

GOT_FOR=$(received "$TRUSTED" x_forwarded_for -H 'X-Forwarded-For: 203.0.113.7')
[ "$GOT_FOR" = "203.0.113.7" ] || bench_fail \
  "a trusted peer's X-Forwarded-For was ignored: the origin received '$GOT_FOR'"
printf '  %-48s %s\n' "trusted X-Forwarded-For is believed" "origin saw $GOT_FOR"
bench_result trusted_xff "$GOT_FOR"

GOT_PROTO=$(received "$TRUSTED" x_forwarded_proto -H 'X-Forwarded-Proto: https')
[ "$GOT_PROTO" = "https" ] || bench_fail \
  "a trusted peer's X-Forwarded-Proto was ignored: the origin received '$GOT_PROTO'"
printf '  %-48s %s\n' "trusted X-Forwarded-Proto is believed" "origin saw $GOT_PROTO"
bench_result trusted_xfp "$GOT_PROTO"

# The hop walk. Everything left of the rightmost untrusted address was written
# by somebody nobody vouched for; reading the leftmost entry — the obvious
# implementation — returns whatever the client put there.
GOT_FOR=$(received "$TRUSTED" x_forwarded_for \
  -H 'X-Forwarded-For: 9.9.9.9, 203.0.113.7, 127.0.0.1')
[ "$GOT_FOR" = "203.0.113.7" ] || bench_fail \
  "the hop walk returned '$GOT_FOR' from '9.9.9.9, 203.0.113.7, 127.0.0.1'; reading the leftmost entry returns 9.9.9.9, an address the client chose"
printf '  %-48s %s\n' "the walk stops at the rightmost untrusted hop" "origin saw $GOT_FOR"
bench_result hop_walk "$GOT_FOR"

# An invented scheme from a *trusted* peer is still not a scheme. The
# normalisation is not a trust check; it is a range check, and it applies to
# everyone.
GOT_PROTO=$(received "$TRUSTED" x_forwarded_proto -H 'X-Forwarded-Proto: gopher')
[ "$GOT_PROTO" = "http" ] || bench_fail \
  "a trusted peer claimed the scheme 'gopher' and the origin received '$GOT_PROTO'; only http and https may ever reach the origin or the cache key"
printf '  %-48s %s\n' "an invented scheme is refused even when trusted" "origin saw $GOT_PROTO"
bench_result trusted_invented_scheme "$GOT_PROTO"

# With the peer trusted, http and https genuinely are different requests, and
# must be different cache entries: they can legitimately produce different
# bodies through absolute URLs, redirects and HSTS.
bench_origin_reset "$ORIGIN_PORT"
curl -s -o /dev/null --max-time 10 -H 'X-Forwarded-Proto: http' "http://127.0.0.1:$TRUSTED/p/split"
curl -s -o /dev/null --max-time 10 -H 'X-Forwarded-Proto: https' "http://127.0.0.1:$TRUSTED/p/split"
TRUSTED_RENDERS=$(bench_origin_stat "$ORIGIN_PORT" total)
bench_assert_eq "$TRUSTED_RENDERS" 2 \
  "http and https from a trusted proxy shared one cache entry ($TRUSTED_RENDERS render) — a plaintext response can then be served to a client that asked for TLS"
printf '  %-48s %s\n' "trusted http and https are separate entries" "$TRUSTED_RENDERS renders"
bench_result trusted_key_renders "$TRUSTED_RENDERS"
bench_stop trusted

# ============================================== trusted, public host opt-in
#
# A CDN/load balancer that terminates TLS and talks plain HTTP to Tanod, with
# `trusted_proxies.host` enabled. The public URL comes from the proxy; the
# chain walk still decides which hop's values count.
start trusted_host '["127.0.0.1/32"]' x_forwarded x_forwarded x_forwarded
HOSTED=$PORT
ECHO=$(curl -s --max-time 10 "http://127.0.0.1:$HOSTED/echo-headers/x" \
  -H 'X-Forwarded-For: 203.0.113.7' -H 'X-Forwarded-Proto: https' \
  -H 'X-Forwarded-Host: Shop.Example' -H 'CF-Connecting-IP: 203.0.113.7')
expect_field x_forwarded_host shop.example "a trusted, opted-in X-Forwarded-Host must be preserved (normalised)"
expect_field x_forwarded_proto https "TLS terminated before Tanod must still read as https"
expect_field x_forwarded_port 443 "an https public URL with no explicit port is on 443"
expect_field cf_connecting_ip 203.0.113.7 "a trusted CDN's own identity header passes through"
check "trusted public host and TLS scheme are preserved" "$(field x_forwarded_proto)://$(field x_forwarded_host):$(field x_forwarded_port)"

# Explicit public port.
ECHO=$(curl -s --max-time 10 "http://127.0.0.1:$HOSTED/echo-headers/x" \
  -H 'X-Forwarded-Proto: https' -H 'X-Forwarded-Host: shop.example:8443')
expect_field x_forwarded_port 8443 "an explicit public port must be preserved"

# Spoofed values prefixed to a chain the trusted proxy appended to.
ECHO=$(curl -s --max-time 10 "http://127.0.0.1:$HOSTED/echo-headers/x" \
  -H 'X-Forwarded-For: 9.9.9.9, 203.0.113.7' \
  -H 'X-Forwarded-Proto: http, https' \
  -H 'X-Forwarded-Host: evil.example, shop.example')
expect_field x_forwarded_for 203.0.113.7 "the walk must stop at the rightmost untrusted hop"
expect_field x_forwarded_proto https "the scheme must come from the chosen hop, not the client's prefix"
expect_field x_forwarded_host shop.example "the host must come from the chosen hop, not the client's prefix"
check "a spoofed prefix cannot choose host or scheme" "$(field x_forwarded_host)"

# Contradictory but well-formed: http listener, trusted https claim, host with
# a port. Coherent output: scheme, host and port all from the same statement.
ECHO=$(curl -s --max-time 10 "http://127.0.0.1:$HOSTED/echo-headers/x" \
  -H 'X-Forwarded-Proto: https' -H 'X-Forwarded-Host: shop.example:80')
expect_field x_forwarded_port 80 "the stated port wins over the scheme default"

# Believed hosts are keyed: two public hosts, two entries, two canonicals.
bench_origin_reset "$ORIGIN_PORT"
A=$(curl -s --max-time 10 -H 'X-Forwarded-Proto: https' -H 'X-Forwarded-Host: a.example' "http://127.0.0.1:$HOSTED/reflect/2")
B=$(curl -s --max-time 10 -H 'X-Forwarded-Proto: https' -H 'X-Forwarded-Host: b.example' "http://127.0.0.1:$HOSTED/reflect/2")
A2=$(curl -s --max-time 10 -H 'X-Forwarded-Proto: https' -H 'X-Forwarded-Host: A.example:443' "http://127.0.0.1:$HOSTED/reflect/2")
case "$A" in *'https://a.example/reflect/2'*) ;; *) bench_fail "public host a.example rendered '$A'" ;; esac
case "$B" in *'https://b.example/reflect/2'*) ;; *) bench_fail "public host b.example was served '$B'" ;; esac
[ "$A" = "$A2" ] || bench_fail "a.example and A.example:443 are one public URL but were two entries"
HOSTED_RENDERS=$(bench_origin_stat "$ORIGIN_PORT" total)
bench_assert_eq "$HOSTED_RENDERS" 2 \
  "two public hosts produced $HOSTED_RENDERS renders; each needs exactly its own entry"
check "believed public hosts are separate cache entries" "$HOSTED_RENDERS renders"
bench_result trusted_host_renders "$HOSTED_RENDERS"
bench_stop trusted_host

# ============================================================== IPv6 peers
#
# Dual-stack listener; the v6 loopback peer is trusted in one run and not in
# the other. Skipped, loudly, where the host has no IPv6 loopback.
if grep -qs '^00000000000000000000000000000001 ' /proc/net/if_inet6; then
  start v6_untrusted '[]' x_forwarded x_forwarded none '[::]'
  ECHO=$(curl -s --max-time 10 -g "http://[::1]:$PORT/echo-headers/x" \
    -H 'X-Forwarded-For: 9.9.9.9' -H 'X-Forwarded-Host: evil.example')
  if [ -n "$ECHO" ]; then
    expect_field x_forwarded_for ::1 "an untrusted IPv6 peer is the client"
    expect_field x_forwarded_host "[::1]:$PORT" "an IPv6 literal authority is restated with brackets"
    check "untrusted IPv6 peer" "$(field x_forwarded_for) / $(field x_forwarded_host)"
    bench_stop v6_untrusted
    start v6_trusted '["::1/128"]' x_forwarded x_forwarded none '[::]'
    ECHO=$(curl -s --max-time 10 -g "http://[::1]:$PORT/echo-headers/x" \
      -H 'X-Forwarded-For: 2001:db8::7, ::1')
    expect_field x_forwarded_for 2001:db8::7 "a trusted IPv6 proxy's chain is walked"
    check "trusted IPv6 proxy chain" "$(field x_forwarded_for)"
    bench_stop v6_trusted
  else
    bench_stop v6_untrusted
    check "IPv6 peers" "skipped: no IPv6 loopback"
  fi
else
  check "IPv6 peers" "skipped: no IPv6 loopback"
fi

# ======================================================= sources are separate
#
# `client_ip: none` must not silently keep reading the header. A source set to
# `none` that still parsed would make the config lie about what it does.
start ignored '["127.0.0.1/32"]' none none
GOT_FOR=$(received "$PORT" x_forwarded_for -H 'X-Forwarded-For: 203.0.113.7')
GOT_PROTO=$(received "$PORT" x_forwarded_proto -H 'X-Forwarded-Proto: https')
[ "$GOT_FOR" = "127.0.0.1" ] || bench_fail \
  "client_ip: none still read X-Forwarded-For: the origin received '$GOT_FOR'"
[ "$GOT_PROTO" = "http" ] || bench_fail \
  "scheme: none still read X-Forwarded-Proto: the origin received '$GOT_PROTO'"
printf '  %-48s %s\n' "source: none reads nothing, even from a trusted peer" "$GOT_FOR / $GOT_PROTO"

echo
bench_print_params
echo
bench_pass "forwarded metadata is a claim, and Tanod treats it as one: an untrusted client can move neither the origin's view of it nor the cache key, a trusted proxy is believed, the hop walk resolves to the address a trusted proxy observed, and no unkeyed header can poison a shared response"
