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
BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"

RENDER_MS=${1:-100}

bench_init forwarded
bench_param render_ms "$RENDER_MS"
bench_build

ORIGIN_PORT=$(bench_start_origin origin "$RENDER_MS")

# Start a proxy with the given trust configuration and leave its port in PORT.
start() { # name, from-list, client_ip source, scheme source
  PORT=$(bench_free_port)
  bench_render_config "$BENCH_ROOT/bench/forwarded.yaml" "$BENCH_DIR/$1.yaml" \
    "LISTEN=$PORT" "ORIGIN=$ORIGIN_PORT" "TRUST=$2" \
    "CLIENT_IP_SOURCE=$3" "SCHEME_SOURCE=$4"
  bench_start_tanod "$1" "$BENCH_DIR/$1.yaml" "$PORT"
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
bench_pass "forwarded metadata is a claim, and Tanod treats it as one: an untrusted client can move neither the origin's view of it nor the cache key, a trusted proxy is believed, and the hop walk resolves to the address a trusted proxy observed"
