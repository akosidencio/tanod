#!/usr/bin/env bash
# Does Tanod behave the same over HTTP/2 as it does over HTTP/1.1?
#
# The question is not whether bytes move — Pingora handles the transport. It is
# whether the governor's own rules survive the protocol change, because HTTP/2
# alters three things that classification and cache keying both read:
#
#   * there is no `Host` header; the authority is the `:authority`
#     pseudo-header, which Pingora surfaces on the URI
#   * `Cookie` arrives as several headers rather than one
#   * one connection carries many concurrent requests
#
# The first of those is a cache-key input. A proxy that reads `Host` and
# nothing else sees "" for every h2 request and merges every virtual host on
# the listener into one entry — a cross-tenant leak that appears the day
# `server.h2c` is switched on and is invisible before then. This benchmark
# asserts against it directly.
#
# Upstream h2 is exercised by chaining two Tanod processes: the inner one
# serves h2c, the outer one is configured to speak HTTP/2 to it. That keeps a
# second server implementation out of the repository and still puts a real h2
# client and a real h2 server on either end of the connector.
BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"

CONCURRENCY=${1:-20}
RENDER_MS=${2:-800}

bench_init http2
bench_param concurrency "$CONCURRENCY"
bench_param render_ms "$RENDER_MS"
bench_param ceiling 5
bench_build

command -v curl >/dev/null || bench_fail "curl is required"
curl --version | grep -q HTTP2 || bench_fail \
  "this curl was built without HTTP/2 support, so the benchmark cannot speak h2 and must not report success"

ORIGIN_PORT=$(bench_start_origin origin "$RENDER_MS")
LISTEN=$(bench_free_port)
bench_render_config "$BENCH_ROOT/bench/http2.yaml" "$BENCH_DIR/http2.yaml" \
  "LISTEN=$LISTEN" "ORIGIN=$ORIGIN_PORT" "CEILING=5" "UPSTREAM_VERSION=http1"
bench_start_tanod tanod "$BENCH_DIR/http2.yaml" "$LISTEN"
BASE="http://127.0.0.1:$LISTEN"

h2() { curl -s --http2-prior-knowledge --max-time 20 "$@"; }
h2_version() {
  curl -s --http2-prior-knowledge -o /dev/null -w '%{http_version}' --max-time 20 "$@"
}

echo "downstream h2c and upstream h2"
echo

# ------------------------------------------------------- downstream h2c
VERSION=$(h2_version "$BASE/h2/plain")
[ "$VERSION" = "2" ] || bench_fail \
  "the h2c listener answered HTTP/$VERSION; server.h2c did not take effect, and every assertion below would silently be an HTTP/1.1 test"
printf '  %-44s %s\n' "h2c listener negotiated" "HTTP/$VERSION"
bench_result downstream_version "$VERSION"

# The same listener must still serve HTTP/1.1. Pingora peeks for the
# connection preface, so enabling h2c is additive — but "additive" is a claim
# worth checking rather than assuming, because getting it wrong locks out
# every ordinary client at once.
H1_VERSION=$(curl -s --http1.1 -o /dev/null -w '%{http_version}' --max-time 20 "$BASE/h1/plain")
[ "$H1_VERSION" = "1.1" ] || bench_fail \
  "the h2c listener answered HTTP/$H1_VERSION to an HTTP/1.1 client"
printf '  %-44s %s\n' "same listener still serves HTTP/1.1" "HTTP/$H1_VERSION"
bench_result h1_still_served "$H1_VERSION"

# ------------------------------------------- the authority reaches the key
#
# Two `Host` values, one path. Over HTTP/2 the authority is not a header, so a
# proxy reading `Host` alone would give both requests the same cache key and
# serve the first tenant's page to the second. `--resolve` is what makes curl
# send a different `:authority` while still connecting to the test listener.
bench_origin_reset "$ORIGIN_PORT"
h2 --resolve "a.example.com:$LISTEN:127.0.0.1" "http://a.example.com:$LISTEN/tenant" > /dev/null
h2 --resolve "b.example.com:$LISTEN:127.0.0.1" "http://b.example.com:$LISTEN/tenant" > /dev/null
TENANT_RENDERS=$(bench_origin_stat "$ORIGIN_PORT" total)
bench_assert_eq "$TENANT_RENDERS" 2 \
  "two authorities at one path cost $TENANT_RENDERS origin render(s) — over HTTP/2 the authority is the :authority pseudo-header, not a Host header, and dropping it merges every virtual host into one cache entry"
printf '  %-44s %s\n' "two h2 authorities are two cache entries" "$TENANT_RENDERS renders"
bench_result authority_renders "$TENANT_RENDERS"

# And the positive direction: the same authority twice is one entry, so the
# fix above did not simply disable caching over h2.
bench_origin_reset "$ORIGIN_PORT"
h2 --resolve "a.example.com:$LISTEN:127.0.0.1" "http://a.example.com:$LISTEN/reused" > /dev/null
h2 --resolve "a.example.com:$LISTEN:127.0.0.1" "http://a.example.com:$LISTEN/reused" > /dev/null
REUSED=$(bench_origin_stat "$ORIGIN_PORT" total)
bench_assert_eq "$REUSED" 1 \
  "the same authority twice cost $REUSED renders; caching is not working over HTTP/2 at all"
printf '  %-44s %s\n' "the same h2 authority is one cache entry" "$REUSED render"
bench_result reuse_renders "$REUSED"

# ------------------------------------------------- coalescing over h2
#
# Separate connections rather than one multiplexed one: multiplexing would
# prove that h2 works, not that Tanod collapses concurrent duplicates.
bench_origin_reset "$ORIGIN_PORT"
seq 1 "$CONCURRENCY" | xargs -P "$CONCURRENCY" -I{} \
  curl -s --http2-prior-knowledge -o /dev/null --max-time 30 "$BASE/coalesced" 2>/dev/null
COALESCE_RENDERS=$(bench_origin_stat "$ORIGIN_PORT" total)
bench_assert_le "$COALESCE_RENDERS" 2 \
  "$CONCURRENCY concurrent h2 requests for one URL cost $COALESCE_RENDERS origin renders"
printf '  %-44s %s\n' "$CONCURRENCY concurrent h2 requests" "$COALESCE_RENDERS render(s)"
bench_result coalesce_renders "$COALESCE_RENDERS"

# -------------------------------------------------- admission over h2
#
# Unique URLs, so nothing is cached and nothing is collapsed and the only
# thing standing between the clients and the origin is the ceiling.
bench_origin_reset "$ORIGIN_PORT"
seq 1 "$CONCURRENCY" | xargs -P "$CONCURRENCY" -I{} \
  curl -s --http2-prior-knowledge -o /dev/null --max-time 30 "$BASE/unique/{}" 2>/dev/null
PEAK=$(bench_origin_stat "$ORIGIN_PORT" peak)
bench_assert_le "$PEAK" 5 \
  "origin concurrency peaked at $PEAK against a ceiling of 5 — admission is not applied to HTTP/2 requests"
printf '  %-44s %s\n' "origin peak under $CONCURRENCY h2 arrivals" "$PEAK (ceiling 5)"
bench_result admission_peak "$PEAK"

# ------------------------------------------------------- upstream h2
#
# A second Tanod, serving h2c, becomes the origin. The outer proxy is told
# to speak HTTP/2 to it: over cleartext that is prior-knowledge h2c, with no
# ALPN and no upgrade dance, so an origin that could not speak it would fail
# outright rather than fall back and quietly make this test an h1 test.
bench_stop tanod
INNER=$(bench_free_port)
bench_render_config "$BENCH_ROOT/bench/http2-inner.yaml" "$BENCH_DIR/inner.yaml" \
  "LISTEN=$INNER" "ORIGIN=$ORIGIN_PORT"
bench_start_tanod inner "$BENCH_DIR/inner.yaml" "$INNER"

OUTER=$(bench_free_port)
bench_render_config "$BENCH_ROOT/bench/http2.yaml" "$BENCH_DIR/outer.yaml" \
  "LISTEN=$OUTER" "ORIGIN=$INNER" "CEILING=5" "UPSTREAM_VERSION=http2"
bench_start_tanod outer "$BENCH_DIR/outer.yaml" "$OUTER"

bench_origin_reset "$ORIGIN_PORT"
UP_CODE=$(curl -s -o /dev/null -w '%{http_code}' --max-time 30 "http://127.0.0.1:$OUTER/upstream-h2")
UP_RENDERS=$(bench_origin_stat "$ORIGIN_PORT" total)
bench_assert_eq "$UP_CODE" 200 \
  "a request proxied over an HTTP/2 upstream connection returned HTTP $UP_CODE"
bench_assert_eq "$UP_RENDERS" 1 \
  "the h2 upstream hop delivered $UP_RENDERS renders instead of 1"
printf '  %-44s %s\n' "request via an HTTP/2 upstream connection" "$UP_CODE, $UP_RENDERS render"
bench_result upstream_h2_status "$UP_CODE"

# Body integrity across the h2 hop. Framing bugs do not announce themselves.
DIRECT=$(curl -s "http://127.0.0.1:$ORIGIN_PORT/big/h2/1" | cksum)
THROUGH=$(curl -s --max-time 30 "http://127.0.0.1:$OUTER/big/h2/1" | cksum)
[ "$DIRECT" = "$THROUGH" ] || bench_fail \
  "a 1MiB body came back different across the HTTP/2 upstream hop: origin=$DIRECT proxy=$THROUGH"
printf '  %-44s %s\n' "1MiB body intact across the h2 hop" "checksums match"
bench_result upstream_h2_integrity yes

echo
bench_print_params
echo
bench_pass "HTTP/2 holds downstream and upstream: the authority still keys the cache, coalescing collapsed $CONCURRENCY requests to $COALESCE_RENDERS, admission held the peak at $PEAK, and a 1MiB body survived the h2 upstream hop"
