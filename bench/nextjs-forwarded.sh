#!/usr/bin/env bash
# Forwarded headers against a real Next.js server, not a mock.
#
# Next.js reads the forwarding headers in places that matter for security:
# `X-Forwarded-Host` is what it compares a Server Action's `Origin` with
# (its CSRF check), and `X-Forwarded-Proto` decides the scheme of
# `request.url`, so of every redirect a route handler builds. Unit tests can
# show what Tanod sends; only the framework can show what that does.
#
# One fixture origin container, three Tanod configurations:
#
#   direct   no trusted proxies: a client's forged headers change nothing
#   cdn      a trusted, TLS-terminating proxy with `trusted_proxies.host`
#   nohost   the same proxy without it: the public host is not believed
#
# Requires Docker and the fixture image (built by bench/nextjs.sh, or here if
# missing). TANOD_NEXT_IMAGE selects another image tag.
BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"

IMAGE=${TANOD_NEXT_IMAGE:-tanod-next-origin:reference}
NEXT_VERSION=${TANOD_NEXT_VERSION:-16.4.3}
CONTAINER="tanod-nextjs-forwarded-$$"

bench_init nextjs_forwarded
bench_param image "$IMAGE"
bench_build

if ! docker image inspect "$IMAGE" > /dev/null 2>&1; then
  echo "building $IMAGE (Next $NEXT_VERSION)..."
  docker build -q -t "$IMAGE" \
    --build-arg NODE_VERSION="${TANOD_NEXT_NODE:-22}" \
    --build-arg NEXT_VERSION="$NEXT_VERSION" \
    --build-arg TANOD_BUILD_ID=next-fixture-v1 \
    --build-arg NEXT_SERVER_ACTIONS_ENCRYPTION_KEY=MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY= \
    "$BENCH_ROOT/fixtures/next-storefront" > /dev/null || bench_fail "could not build $IMAGE"
fi
NEXT_PORT=$(bench_free_port)
docker run -d --rm --name "$CONTAINER" -p "127.0.0.1:$NEXT_PORT:3000" \
  -e TANOD_BUILD_ID=next-fixture-v1 -e INSTANCE_ID=forwarded "$IMAGE" > /dev/null \
  || bench_fail "could not start the Next.js origin"
trap 'docker rm -f "$CONTAINER" > /dev/null 2>&1; bench_cleanup' EXIT
bench_wait_http "http://127.0.0.1:$NEXT_PORT/healthz" "next.js origin"
NEXT_RUNNING=$(docker exec "$CONTAINER" sed -n 's/^  "version": "\(.*\)",$/\1/p' /app/node_modules/next/package.json 2>/dev/null)
bench_param next_version "$NEXT_RUNNING"

start() { # name, trust list, host source
  PORT=$(bench_free_port)
  bench_render_config "$BENCH_ROOT/bench/nextjs-forwarded.yaml.tpl" "$BENCH_DIR/$1.yaml" \
    "LISTEN=$PORT" "ORIGIN=$NEXT_PORT" "TRUST=$2" "HOST_SOURCE=$3"
  bench_start_tanod "$1" "$BENCH_DIR/$1.yaml" "$PORT"
}
check() { printf '  %-56s %s\n' "$1" "$2"; }
header() { # name, curl args... : one response header's value
  local name=$1; shift
  curl -s -o /dev/null -D - --max-time 20 "$@" | tr -d '\r' \
    | sed -n "s/^$name: //Ip" | head -1
}
code() { curl -s -o /dev/null -w '%{http_code}' --max-time 20 "$@"; }

# A Server Action with an id no build contains. Next checks the origin first:
# a mismatch is `500 Invalid Server Actions request`; a match proceeds to the
# lookup and answers with `x-nextjs-action-not-found` (400 on 16.4, 404 on
# 16.3), reported here as `passed`.
action() { # base url, curl args...
  local base=$1; shift
  curl -s -o /dev/null -D - --max-time 20 -X POST "$base/cart" \
    -H 'Next-Action: 00feedfacecafe00' -H 'Content-Type: text/plain;charset=UTF-8' \
    --data '[]' "$@" | tr -d '\r' | awk 'NR == 1 { s = $2 } tolower($0) ~ /^x-nextjs-action-not-found:/ { nf = 1 } END { print (nf ? "passed" : s) }'
}

echo "forwarded headers through Tanod to Next.js $NEXT_RUNNING"
echo

# ==================================================================== direct
start direct '[]' none
BASE="http://127.0.0.1:$PORT"

# Redirect: `request.url` takes its scheme from X-Forwarded-Proto.
LOCATION=$(header location "$BASE/api/draft" -H 'X-Forwarded-Proto: https' -H 'X-Forwarded-Host: evil.example')
case "$LOCATION" in http://*/preview) ;; *) bench_fail \
  "a client's X-Forwarded-Proto: https shaped Next's redirect: Location '$LOCATION'" ;; esac
check "redirect scheme ignores a client's forged proto" "$LOCATION"

# Draft-mode cookie: every response sets it itself; none is replayed from
# the cache, where a stored Set-Cookie would be served to other visitors.
# (Its value is the build's preview id, identical by design.)
for n in 1 2; do
  RESPONSE=$(curl -s -o /dev/null -D - --max-time 20 "$BASE/api/draft" | tr -d '\r')
  printf '%s' "$RESPONSE" | grep -qi '^set-cookie: __prerender_bypass=' \
    || bench_fail "draft response $n carried no draft cookie"
  printf '%s' "$RESPONSE" | grep -qi '^x-tanod: hit' \
    && bench_fail "a cookie-setting draft response was served from the cache"
done
check "draft cookie set by the origin on every response" "2 of 2, no cache hits"

# Server Actions: a forged X-Forwarded-Host matching a forged Origin must not
# pass Next's origin check; the genuine origin must.
FORGED=$(action "$BASE" -H 'Origin: http://evil.example' -H 'X-Forwarded-Host: evil.example')
[ "$FORGED" = 500 ] || bench_fail \
  "a forged X-Forwarded-Host satisfied Next's Server Action origin check (got $FORGED)"
GENUINE=$(action "$BASE" -H "Origin: $BASE")
[ "$GENUINE" = passed ] || bench_fail \
  "a same-origin Server Action failed Next's origin check through Tanod (got $GENUINE)"
check "Server Action origin check: forged host / genuine" "$FORGED / $GENUINE"

# SSR and RSC: a request carrying forged URL headers fills the cache; the
# next ordinary request must hit that entry and get the same body.
P="$BASE/products/forwarded-$$"
curl -s -o "$BENCH_DIR/ssr-forged" --max-time 20 -H 'X-Forwarded-Host: evil.example' \
  -H 'X-Forwarded-Proto: https' -H 'X-Forwarded-Prefix: /evil' "$P"
STATUS=$(header x-tanod "$P")
curl -s -o "$BENCH_DIR/ssr-plain" --max-time 20 "$P"
[ "$STATUS" = HIT ] || bench_fail "forged forwarding headers split the SSR cache key (second request: $STATUS)"
grep -q 'evil' "$BENCH_DIR/ssr-forged" "$BENCH_DIR/ssr-plain" && bench_fail "a forged value reached a rendered page"
# Next 16.4 redirects a bare RSC request to its canonical `?_rsc` URL; follow
# it, with the router headers its client sends (as bench/nextjs.sh does).
RSC_TREE='%5B%22%22%2C%7B%22children%22%3A%5B%22__PAGE__%22%2C%7B%7D%2Cnull%2Cnull%2C4096%5D%7D%2Cnull%2Cnull%2C4112%5D'
RSC=$(curl -s -L -o /dev/null -w '%{content_type}' --max-time 20 -H 'RSC: 1' -H 'Next-Url: /' \
  -H "Next-Router-State-Tree: $RSC_TREE" -H 'X-Forwarded-Host: evil.example' "$P")
case "$RSC" in text/x-component*) ;; *) bench_fail "the RSC request got '$RSC', not a component payload" ;; esac
check "SSR entry shared despite forged headers; RSC keyed apart" "$STATUS / $RSC"
bench_stop direct

# ======================================================== trusted CDN, opt-in
start cdn '["127.0.0.1/32"]' x_forwarded
BASE="http://127.0.0.1:$PORT"
LOCATION=$(header location "$BASE/api/draft" -H 'X-Forwarded-For: 203.0.113.7' \
  -H 'X-Forwarded-Proto: https' -H 'X-Forwarded-Host: shop.example')
case "$LOCATION" in https://*/preview) ;; *) bench_fail \
  "TLS terminated at a trusted proxy, yet Next redirected to '$LOCATION'" ;; esac
check "TLS ended at the proxy: redirect stays https" "$LOCATION"

PUBLIC=$(action "$BASE" -H 'Origin: https://shop.example' -H 'X-Forwarded-Proto: https' \
  -H 'X-Forwarded-Host: shop.example')
[ "$PUBLIC" = passed ] || bench_fail \
  "a Server Action from the public origin failed behind a trusted proxy (got $PUBLIC)"
DEFAULT_PORT=$(action "$BASE" -H 'Origin: https://shop.example' -H 'X-Forwarded-Proto: https' \
  -H 'X-Forwarded-Host: shop.example:443')
[ "$DEFAULT_PORT" = passed ] || bench_fail \
  "a proxy writing the default port broke the Server Action origin check (got $DEFAULT_PORT)"
SPOOFED=$(action "$BASE" -H 'Origin: https://evil.example' -H 'X-Forwarded-For: 9.9.9.9, 203.0.113.7' \
  -H 'X-Forwarded-Proto: https, https' -H 'X-Forwarded-Host: evil.example, shop.example')
[ "$SPOOFED" = 500 ] || bench_fail \
  "a host prefixed to a trusted proxy's chain passed Next's origin check (got $SPOOFED)"
check "public-origin action / default port / spoofed prefix" "$PUBLIC / $DEFAULT_PORT / $SPOOFED"

# http and https public URLs are separate entries; each hits on its own.
P="$BASE/products/scheme-$$"
A=$(header x-tanod "$P" -H 'X-Forwarded-Proto: http' -H 'X-Forwarded-Host: shop.example')
B=$(header x-tanod "$P" -H 'X-Forwarded-Proto: https' -H 'X-Forwarded-Host: shop.example')
C=$(header x-tanod "$P" -H 'X-Forwarded-Proto: https' -H 'X-Forwarded-Host: shop.example:443')
D=$(header x-tanod "$P" -H 'X-Forwarded-Proto: https' -H 'X-Forwarded-Host: other.example')
[ "$A $B $C $D" = "MISS MISS HIT MISS" ] || bench_fail \
  "scheme/host cache isolation: expected MISS MISS HIT MISS, got $A $B $C $D"
check "http, https, https:443, other host" "$A $B $C $D"
bench_stop cdn

# ========================================================= trusted, no opt-in
start nohost '["127.0.0.1/32"]' none
BASE="http://127.0.0.1:$PORT"
NOHOST=$(action "$BASE" -H 'Origin: https://shop.example' -H 'X-Forwarded-Proto: https' \
  -H 'X-Forwarded-Host: shop.example')
[ "$NOHOST" = 500 ] || bench_fail \
  "without trusted_proxies.host a proxy's X-Forwarded-Host was still believed (got $NOHOST)"
check "host not opted in: proxy's public host not believed" "$NOHOST"
bench_stop nohost

for name in direct cdn nohost; do bench_assert_no_panics "$name"; done
echo
bench_print_params
echo
bench_pass "Next.js builds redirects and checks Server Action origins from what Tanod states, and nothing a client forges reaches either; a trusted proxy's public URL is honoured only when opted in"
