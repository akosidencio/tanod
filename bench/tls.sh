#!/usr/bin/env bash
# Native TLS, downstream and upstream.
#
# The transport is Pingora's; what this checks is what Tanod does with the
# fact of it. Terminating TLS changes an input the cache key reads — the
# scheme — and an input the origin reads — `X-Forwarded-Proto`. Both must now
# come from the connection rather than from a header, because on a TLS
# listener there is no proxy in front to have written one.
#
# The failure being guarded against is quiet: a TLS listener that still
# reports `http` merges the https and http entries for a URL, so a plaintext
# render can be served to a client that asked for TLS, and every absolute URL
# the origin generates comes out wrong.
#
# Requires a build with the `tls` feature; the script compiles one.
BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"

RENDER_MS=${1:-100}
SNI=tanod.test

BENCH_FEATURES=tls
bench_init tls
bench_param render_ms "$RENDER_MS"
bench_param features tls
bench_param sni "$SNI"
bench_build

command -v openssl >/dev/null || bench_fail "openssl is required to mint a test certificate"

# A self-signed certificate for this run only, with a SAN so a modern client
# will look at it at all.
CERT="$BENCH_DIR/cert.pem"
KEY="$BENCH_DIR/key.pem"
openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -keyout "$KEY" -out "$CERT" -subj "/CN=$SNI" \
  -addext "subjectAltName=DNS:$SNI,IP:127.0.0.1" >/dev/null 2>&1 \
  || bench_fail "could not mint a self-signed certificate"

ORIGIN_PORT=$(bench_start_origin origin "$RENDER_MS")
LISTEN=$(bench_free_port)
TLS_LISTEN=$(bench_free_port)
bench_render_config "$BENCH_ROOT/bench/tls.yaml" "$BENCH_DIR/tls.yaml" \
  "LISTEN=$LISTEN" "TLS_LISTEN=$TLS_LISTEN" "ORIGIN=$ORIGIN_PORT" \
  "CERT=$CERT" "KEY=$KEY"

# `tanod check` first: a binary built without the feature must refuse this
# config rather than start and leave the TLS port dead.
"$(bench_bin tanod)" check --config "$BENCH_DIR/tls.yaml" >/dev/null 2>&1 \
  || bench_fail "tanod check refused a valid TLS config — is this build missing the tls feature?"

bench_start_tanod tanod "$BENCH_DIR/tls.yaml" "$LISTEN"
bench_wait_port 127.0.0.1 "$TLS_LISTEN" "tanod TLS listener"

echo "TLS termination and origin TLS"
echo

# `-k`: the certificate was minted thirty lines ago and no store knows it.
# What is under test is the proxy's behaviour on a TLS connection, not the
# trust chain, which the origin-TLS section covers separately.
https() { curl -sk --max-time 15 --resolve "$SNI:$TLS_LISTEN:127.0.0.1" "$@"; }

# ------------------------------------------------------ the listener serves
CODE=$(https -o /dev/null -w '%{http_code}' "https://$SNI:$TLS_LISTEN/p/tls")
bench_assert_eq "$CODE" 200 "the TLS listener answered HTTP $CODE"
printf '  %-46s %s\n' "TLS listener serves" "$CODE"
bench_result tls_status "$CODE"

# The cleartext listener keeps working. A migration runs both at once, and a
# TLS block that silently took over `server.listen` would be an outage.
PLAIN=$(curl -s -o /dev/null -w '%{http_code}' --max-time 15 "http://127.0.0.1:$LISTEN/p/tls")
bench_assert_eq "$PLAIN" 200 "the cleartext listener answered HTTP $PLAIN alongside TLS"
printf '  %-46s %s\n' "cleartext listener still serves" "$PLAIN"
bench_result plaintext_status "$PLAIN"

# ------------------------------------------------------------ ALPN and h2
VERSION=$(https -o /dev/null -w '%{http_version}' "https://$SNI:$TLS_LISTEN/p/tls")
[ "$VERSION" = "2" ] || bench_fail \
  "the TLS listener negotiated HTTP/$VERSION; server.tls.h2 is set, so ALPN should have offered h2"
printf '  %-46s %s\n' "ALPN negotiated" "HTTP/$VERSION"
bench_result alpn_version "$VERSION"

# `http/1.1` is always offered alongside `h2`, so an HTTP/1.1-only client is
# never locked out of the TLS listener.
H1=$(curl -sk --http1.1 -o /dev/null -w '%{http_version}' --max-time 15 \
  --resolve "$SNI:$TLS_LISTEN:127.0.0.1" "https://$SNI:$TLS_LISTEN/p/tls")
[ "$H1" = "1.1" ] || bench_fail "an HTTP/1.1 client got HTTP/$H1 from the TLS listener"
printf '  %-46s %s\n' "HTTP/1.1 clients are not locked out" "HTTP/$H1"

# ------------------------------------- the scheme comes from the connection
#
# No forwarded header anywhere: on a TLS listener there is no proxy in front
# to have written one, so `https` can only come from the connection itself.
GOT=$(https "https://$SNI:$TLS_LISTEN/echo-headers/x" \
  | sed -n 's/.*"x_forwarded_proto":"\([^"]*\)".*/\1/p')
[ "$GOT" = "https" ] || bench_fail \
  "over TLS the origin received X-Forwarded-Proto: '$GOT'; a framework generating absolute URLs from that emits http:// links on an https site"
printf '  %-46s %s\n' "origin is told the connection was TLS" "$GOT"
bench_result upstream_scheme "$GOT"

# And the cache key. Same path, both listeners: two entries, because an https
# request and an http one can legitimately produce different bodies.
bench_origin_reset "$ORIGIN_PORT"
curl -s -o /dev/null --max-time 15 "http://127.0.0.1:$LISTEN/p/scheme"
https -o /dev/null "https://$SNI:$TLS_LISTEN/p/scheme"
RENDERS=$(bench_origin_stat "$ORIGIN_PORT" total)
bench_assert_eq "$RENDERS" 2 \
  "one path over both listeners cost $RENDERS render(s) — the scheme is not reaching the cache key, so a plaintext response can be served to a client that asked for TLS"
printf '  %-46s %s\n' "http and https are separate cache entries" "$RENDERS renders"
bench_result scheme_key_renders "$RENDERS"

# The positive control: two https requests are still one entry, so the split
# above is the scheme and not a disabled cache.
bench_origin_reset "$ORIGIN_PORT"
https -o /dev/null "https://$SNI:$TLS_LISTEN/p/reused"
https -o /dev/null "https://$SNI:$TLS_LISTEN/p/reused"
REUSED=$(bench_origin_stat "$ORIGIN_PORT" total)
bench_assert_eq "$REUSED" 1 "two identical https requests cost $REUSED renders"
printf '  %-46s %s\n' "two https requests are one cache entry" "$REUSED render"

# ----------------------------------------------------------- origin TLS
#
# The Tanod above becomes the origin: a second one is pointed at its TLS
# listener with `origin.tls`, so a real TLS client connector talks to a real
# TLS acceptor.
OUTER=$(bench_free_port)
bench_render_config "$BENCH_ROOT/bench/tls-origin.yaml" "$BENCH_DIR/outer.yaml" \
  "LISTEN=$OUTER" "ORIGIN=$TLS_LISTEN" "SNI=$SNI"
bench_start_tanod outer "$BENCH_DIR/outer.yaml" "$OUTER"

bench_origin_reset "$ORIGIN_PORT"
UP=$(curl -s -o /dev/null -w '%{http_code}' --max-time 20 "http://127.0.0.1:$OUTER/p/origin-tls")
UP_RENDERS=$(bench_origin_stat "$ORIGIN_PORT" total)
bench_assert_eq "$UP" 200 "a request over a TLS upstream connection returned HTTP $UP"
bench_assert_gt "$UP_RENDERS" 0 "the TLS upstream hop never reached the origin"
printf '  %-46s %s\n' "request over a TLS upstream connection" "$UP"
bench_result origin_tls_status "$UP"

# Body integrity across the TLS hop.
DIRECT=$(curl -s "http://127.0.0.1:$ORIGIN_PORT/big/tls/1" | cksum)
THROUGH=$(curl -s --max-time 30 "http://127.0.0.1:$OUTER/big/tls/1" | cksum)
[ "$DIRECT" = "$THROUGH" ] || bench_fail \
  "a 1MiB body came back different across the TLS upstream hop: origin=$DIRECT proxy=$THROUGH"
printf '  %-46s %s\n' "1MiB body intact across the TLS hop" "checksums match"
bench_result origin_tls_integrity yes

# -------------------------------------------- unimplementable keys are refused
#
# Pingora 0.8's rustls connector never reads a per-peer CA store. Accepting
# `origin.tls.ca` would mean a config that names a CA, a proxy that verifies
# against the system roots, and no way to tell the difference from outside.
sed 's|verify_cert: false|verify_cert: true\n    ca: "'"$CERT"'"|' \
  "$BENCH_DIR/outer.yaml" > "$BENCH_DIR/with-ca.yaml"
if "$(bench_bin tanod)" check --config "$BENCH_DIR/with-ca.yaml" >/dev/null 2>&1; then
  bench_fail "origin.tls.ca was accepted; Pingora's rustls connector ignores it, so the config would claim a verification that is not happening"
fi
printf '  %-46s %s\n' "origin.tls.ca is refused, not ignored" "check exits non-zero"
bench_result ca_rejected yes

echo
bench_print_params
echo
bench_pass "TLS holds in both directions: the listener negotiated HTTP/$VERSION, the connection's scheme reached both the origin and the cache key, cleartext kept serving alongside it, and a 1MiB body survived a TLS upstream hop"
