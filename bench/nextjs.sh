#!/usr/bin/env bash
# A real Next.js integration proof: one Tanod process, three standalone
# origins, and machine-checked public/private/streaming behaviour across both
# the App Router and the Pages Router.
#
# The two things a curl-driven test cannot reach — a router prefetch and a real
# Server Action submission — are covered by bench/nextjs-browser.sh, which
# drives the same stack with Chromium.
set -euo pipefail

cd "$(dirname "$0")/.."

COMPOSE=(docker compose -p tanod-nextjs-fixture -f compose.nextjs.yaml)
PROXY_URL=${PROXY_URL:-http://127.0.0.1:18080}
METRICS_URL=${METRICS_URL:-http://127.0.0.1:19090}
CONCURRENCY=${CONCURRENCY:-24}
RESULT_DIR=$(mktemp -d)

down_stack() {
  "${COMPOSE[@]}" down --remove-orphans >/dev/null 2>&1 || true
}

cleanup() {
  down_stack
  rm -rf "$RESULT_DIR"
}
trap cleanup EXIT

fail() {
  echo "FAIL: $*" >&2
  echo "Tanod logs:" >&2
  "${COMPOSE[@]}" logs --no-color --tail=80 tanod >&2 || true
  exit 1
}

metric_sum() {
  local route=$1
  curl -fsS "$METRICS_URL/metrics" | awk -v route="$route" '
    /^tanod_origin_requests_total[{]/ && index($1, "route=\"" route "\"") {
      sum += $2
    }
    END { print sum + 0 }
  '
}

wait_until_ready() {
  for _ in $(seq 1 90); do
    if curl -fsS -o /dev/null "$PROXY_URL/healthz" \
      && curl -fsS -o /dev/null "$METRICS_URL/metrics"; then
      return 0
    fi
    sleep 1
  done
  return 1
}

echo "Building and starting one Tanod instance with three Next.js origins..."
down_stack
"${COMPOSE[@]}" up --build --detach || fail "the fixture stack did not start"
wait_until_ready || fail "services did not become ready"

RUN_ID="$(date +%s)-$$"

echo
echo "1/11 identical public SSR requests coalesce across the origin pool"
BEFORE=$(metric_sum products)
seq 1 "$CONCURRENCY" | xargs -P "$CONCURRENCY" -I{} \
  curl -sS -o "$RESULT_DIR/coalesce-{}.html" -w '%{http_code}\n' \
  "$PROXY_URL/products/coalesce-$RUN_ID" > "$RESULT_DIR/coalesce-status"
SERVED=$(grep -c '^200$' "$RESULT_DIR/coalesce-status" || true)
AFTER=$(metric_sum products)
ORIGIN_REQUESTS=$((AFTER - BEFORE))
[ "$SERVED" -eq "$CONCURRENCY" ] || fail "served $SERVED/$CONCURRENCY coalescing requests"
[ "$ORIGIN_REQUESTS" -eq 1 ] || fail "$CONCURRENCY identical requests reached the origins $ORIGIN_REQUESTS times"
RENDER_IDS=$(sed -n 's/.*data-render-id="\([^"]*\)".*/\1/p' "$RESULT_DIR"/coalesce-*.html | sort -u | wc -l | tr -d ' ')
[ "$RENDER_IDS" -eq 1 ] || fail "coalesced clients received $RENDER_IDS render ids"
echo "PASS: $CONCURRENCY responses, one origin render, one shared render id"

echo
echo "2/11 distinct paths are distributed across all three Next.js origins"
for index in $(seq 1 18); do
  curl -fsS "$PROXY_URL/products/spread-$RUN_ID-$index" > "$RESULT_DIR/spread-$index.html"
done
INSTANCES=$(sed -n 's/.*data-origin-instance="\([^"]*\)".*/\1/p' "$RESULT_DIR"/spread-*.html | sort -u)
INSTANCE_COUNT=$(echo "$INSTANCES" | sed '/^$/d' | wc -l | tr -d ' ')
[ "$INSTANCE_COUNT" -eq 3 ] || fail "expected three origins, observed $INSTANCE_COUNT: $INSTANCES"
echo "PASS: observed $(echo "$INSTANCES" | tr '\n' ' ')"

echo
echo "3/11 HTML and React Server Component payloads use separate cache keys"
RSC_PATH="/products/rsc-$RUN_ID"
RSC_URL="$PROXY_URL$RSC_PATH"
# This is the router tree emitted by the fixture homepage. Depending on the
# request host, Next either serves this RSC request directly or redirects it to
# the canonical `_rsc` URL used by its browser client.
RSC_TREE='%5B%22%22%2C%7B%22children%22%3A%5B%22__PAGE__%22%2C%7B%7D%2Cnull%2Cnull%2C4096%5D%7D%2Cnull%2Cnull%2C4112%5D'
BEFORE=$(metric_sum products)
curl -fsS -D "$RESULT_DIR/html.headers" -o "$RESULT_DIR/html.body" "$RSC_URL"
curl -fsS -D "$RESULT_DIR/rsc-initial.headers" -o "$RESULT_DIR/rsc-initial.body" \
  -H 'RSC: 1' -H 'Next-Url: /' -H "Next-Router-State-Tree: $RSC_TREE" \
  "$RSC_URL"
RSC_LOCATION=$(sed -n 's/^location: //Ip' "$RESULT_DIR/rsc-initial.headers" | tr -d '\r' | tail -1)
if [ -n "$RSC_LOCATION" ]; then
  case "$RSC_LOCATION" in
    "$RSC_PATH"'?_rsc='*) ;;
    *) fail "Next.js returned an unexpected RSC location: $RSC_LOCATION" ;;
  esac
  curl -fsS -D "$RESULT_DIR/rsc.headers" -o "$RESULT_DIR/rsc.body" \
    -H 'RSC: 1' -H 'Next-Url: /' -H "Next-Router-State-Tree: $RSC_TREE" \
    "$PROXY_URL$RSC_LOCATION"
  EXPECTED_RSC_ORIGIN_REQUESTS=3
else
  RSC_LOCATION="$RSC_PATH"
  cp "$RESULT_DIR/rsc-initial.headers" "$RESULT_DIR/rsc.headers"
  cp "$RESULT_DIR/rsc-initial.body" "$RESULT_DIR/rsc.body"
  EXPECTED_RSC_ORIGIN_REQUESTS=2
fi
# Both variants should now be cache hits and must remain different types.
curl -fsS -o "$RESULT_DIR/rsc-again.body" \
  -H 'RSC: 1' -H 'Next-Url: /' -H "Next-Router-State-Tree: $RSC_TREE" \
  "$PROXY_URL$RSC_LOCATION"
curl -fsS -o "$RESULT_DIR/html-again.body" "$RSC_URL"
AFTER=$(metric_sum products)
ORIGIN_REQUESTS=$((AFTER - BEFORE))
[ "$ORIGIN_REQUESTS" -eq "$EXPECTED_RSC_ORIGIN_REQUESTS" ] \
  || fail "HTML plus RSC sequence used $ORIGIN_REQUESTS origin requests instead of $EXPECTED_RSC_ORIGIN_REQUESTS"
grep -qi '^content-type: text/html' "$RESULT_DIR/html.headers" || fail "document request was not HTML"
grep -qi '^content-type: text/x-component' "$RESULT_DIR/rsc.headers" || fail "RSC request was not a component payload"
cmp -s "$RESULT_DIR/html.body" "$RESULT_DIR/html-again.body" || fail "cached HTML response changed after an RSC request"
cmp -s "$RESULT_DIR/rsc.body" "$RESULT_DIR/rsc-again.body" || fail "cached RSC response changed on reuse"
echo "PASS: HTML and canonical RSC variants cached independently without mixing"

echo
echo "4/11 Set-Cookie responses are never shared"
PRIVATE_COUNT=16
BEFORE=$(metric_sum private-session)
seq 1 "$PRIVATE_COUNT" | xargs -P "$PRIVATE_COUNT" -I{} \
  curl -sS -o "$RESULT_DIR/private-{}.json" -w '%{http_code}\n' \
  "$PROXY_URL/api/private-session" > "$RESULT_DIR/private-status"
SERVED=$(grep -c '^200$' "$RESULT_DIR/private-status" || true)
AFTER=$(metric_sum private-session)
ORIGIN_REQUESTS=$((AFTER - BEFORE))
SESSIONS=$(sed -n 's/.*"session_id":"\([^"]*\)".*/\1/p' "$RESULT_DIR"/private-*.json | sort -u | wc -l | tr -d ' ')
[ "$SERVED" -eq "$PRIVATE_COUNT" ] || fail "served $SERVED/$PRIVATE_COUNT private requests"
[ "$ORIGIN_REQUESTS" -eq "$PRIVATE_COUNT" ] || fail "$PRIVATE_COUNT private requests caused $ORIGIN_REQUESTS origin requests"
[ "$SESSIONS" -eq "$PRIVATE_COUNT" ] || fail "$PRIVATE_COUNT private requests returned $SESSIONS sessions"
echo "PASS: $PRIVATE_COUNT requests, $PRIVATE_COUNT renders, $PRIVATE_COUNT sessions"

echo
echo "5/11 Next-Action mutation requests bypass a deliberately public route"
MUTATION_COUNT=12
BEFORE=$(metric_sum action-probe)
seq 1 "$MUTATION_COUNT" | xargs -P "$MUTATION_COUNT" -I{} \
  curl -sS -X POST -H 'Next-Action: fixture-probe' \
  -o "$RESULT_DIR/mutation-{}.json" -w '%{http_code}\n' \
  "$PROXY_URL/api/action-probe" > "$RESULT_DIR/mutation-status"
SERVED=$(grep -c '^200$' "$RESULT_DIR/mutation-status" || true)
AFTER=$(metric_sum action-probe)
ORIGIN_REQUESTS=$((AFTER - BEFORE))
MUTATIONS=$(sed -n 's/.*"mutation_id":"\([^"]*\)".*/\1/p' "$RESULT_DIR"/mutation-*.json | sort -u | wc -l | tr -d ' ')
[ "$SERVED" -eq "$MUTATION_COUNT" ] || fail "served $SERVED/$MUTATION_COUNT mutation requests"
[ "$ORIGIN_REQUESTS" -eq "$MUTATION_COUNT" ] || fail "$MUTATION_COUNT mutations caused $ORIGIN_REQUESTS origin requests"
[ "$MUTATIONS" -eq "$MUTATION_COUNT" ] || fail "$MUTATION_COUNT mutations returned $MUTATIONS mutation ids"
echo "PASS: $MUTATION_COUNT mutations bypassed cache reuse and coalescing"

echo
echo "6/11 Draft Mode bypasses a cached public preview without contaminating it"
BEFORE=$(metric_sum preview)
curl -fsS -o "$RESULT_DIR/preview-public.html" "$PROXY_URL/preview"
curl -sS -D "$RESULT_DIR/draft.headers" -o /dev/null "$PROXY_URL/api/draft"
DRAFT_COOKIE=$(sed -n 's/^[Ss]et-[Cc]ookie: \([^;]*\).*/\1/p' "$RESULT_DIR/draft.headers" | tr -d '\r' | paste -sd ';' -)
[ -n "$DRAFT_COOKIE" ] || fail "Draft Mode endpoint returned no cookie"
curl -fsS -H "Cookie: $DRAFT_COOKIE" -o "$RESULT_DIR/preview-draft.html" "$PROXY_URL/preview"
curl -fsS -o "$RESULT_DIR/preview-public-again.html" "$PROXY_URL/preview"
AFTER=$(metric_sum preview)
ORIGIN_REQUESTS=$((AFTER - BEFORE))
[ "$ORIGIN_REQUESTS" -eq 2 ] || fail "public/draft/public preview sequence used $ORIGIN_REQUESTS origin requests instead of 2"
grep -q 'Unpublished winter catalog' "$RESULT_DIR/preview-draft.html" || fail "Draft Mode content was not returned"
cmp -s "$RESULT_DIR/preview-public.html" "$RESULT_DIR/preview-public-again.html" || fail "Draft Mode contaminated the cached public preview"
echo "PASS: draft content bypassed; the public cache entry remained unchanged"

echo
echo "7/11 coalescing preserves a real Suspense stream"
STREAM_COUNT=10
STREAM_URL="$PROXY_URL/flash-sale?run=$RUN_ID"
BEFORE=$(metric_sum flash-sale)
seq 1 "$STREAM_COUNT" | xargs -P "$STREAM_COUNT" -I{} \
  curl -sS -o /dev/null --max-time 20 \
  -w '%{time_starttransfer} %{time_total} %{http_code}\n' \
  "$STREAM_URL" > "$RESULT_DIR/stream-times"
AFTER=$(metric_sum flash-sale)
ORIGIN_REQUESTS=$((AFTER - BEFORE))
SERVED=$(grep -c ' 200$' "$RESULT_DIR/stream-times" || true)
MAX_TTFB=$(awk '{print $1}' "$RESULT_DIR/stream-times" | sort -n | tail -1)
MEDIAN_TOTAL=$(awk '{print $2}' "$RESULT_DIR/stream-times" | sort -n | awk '{v[NR]=$1} END {print v[int(NR/2)+1]}')
STREAMED=$(awk -v first="$MAX_TTFB" -v total="$MEDIAN_TOTAL" 'BEGIN { print (first < total / 2) ? 1 : 0 }')
[ "$SERVED" -eq "$STREAM_COUNT" ] || fail "served $SERVED/$STREAM_COUNT streaming requests"
[ "$ORIGIN_REQUESTS" -eq 1 ] || fail "$STREAM_COUNT streaming requests caused $ORIGIN_REQUESTS origin requests"
[ "$STREAMED" -eq 1 ] || fail "max TTFB ${MAX_TTFB}s was not below half of median total ${MEDIAN_TOTAL}s"
echo "PASS: one origin render; max TTFB ${MAX_TTFB}s, median total ${MEDIAN_TOTAL}s"

echo
echo "8/11 Pages Router getServerSideProps coalesces like the App Router"
LEGACY_COUNT=16
LEGACY_URL="$PROXY_URL/legacy/coalesce-$RUN_ID"
BEFORE=$(metric_sum legacy-pages)
seq 1 "$LEGACY_COUNT" | xargs -P "$LEGACY_COUNT" -I{} \
  curl -sS -o "$RESULT_DIR/legacy-{}.html" -w '%{http_code}\n' \
  "$LEGACY_URL" > "$RESULT_DIR/legacy-status"
SERVED=$(grep -c '^200$' "$RESULT_DIR/legacy-status" || true)
AFTER=$(metric_sum legacy-pages)
ORIGIN_REQUESTS=$((AFTER - BEFORE))
LEGACY_RENDER_IDS=$(sed -n 's/.*data-render-id="\([^"]*\)".*/\1/p' "$RESULT_DIR"/legacy-*.html | sort -u | wc -l | tr -d ' ')
[ "$SERVED" -eq "$LEGACY_COUNT" ] || fail "served $SERVED/$LEGACY_COUNT Pages Router requests"
[ "$ORIGIN_REQUESTS" -eq 1 ] || fail "$LEGACY_COUNT identical Pages Router requests reached the origins $ORIGIN_REQUESTS times"
[ "$LEGACY_RENDER_IDS" -eq 1 ] || fail "coalesced Pages Router clients received $LEGACY_RENDER_IDS render ids"
grep -q 'data-route-kind="pages-router-ssr"' "$RESULT_DIR/legacy-1.html" || fail "response did not come from the Pages Router"
echo "PASS: $LEGACY_COUNT responses, one origin render, one shared render id"

echo
echo "9/11 the Pages Router data payload never mixes with its own document"
# A client-side navigation to a Pages Router route fetches JSON props from
# /_next/data/<buildId>/<route>.json. Same page, different URL, different
# content type — and a browser handed the wrong one renders nothing.
BUILD_ID=$(sed -n 's/.*"buildId":"\([^"]*\)".*/\1/p' "$RESULT_DIR/legacy-1.html" | head -1)
[ -n "$BUILD_ID" ] || fail "could not read the Next.js build id out of the Pages Router document"
DATA_PATH="/_next/data/$BUILD_ID/legacy/data-$RUN_ID.json"
DOC_PATH="/legacy/data-$RUN_ID"
BEFORE_DOC=$(metric_sum legacy-pages)
BEFORE_DATA=$(metric_sum legacy-data)
curl -fsS -D "$RESULT_DIR/legacy-doc.headers" -o "$RESULT_DIR/legacy-doc.body" "$PROXY_URL$DOC_PATH"
curl -fsS -D "$RESULT_DIR/legacy-data.headers" -o "$RESULT_DIR/legacy-data.body" \
  -H 'x-nextjs-data: 1' "$PROXY_URL$DATA_PATH"
# Both again: each must come back from its own entry, unchanged.
curl -fsS -o "$RESULT_DIR/legacy-doc-again.body" "$PROXY_URL$DOC_PATH"
curl -fsS -o "$RESULT_DIR/legacy-data-again.body" \
  -H 'x-nextjs-data: 1' "$PROXY_URL$DATA_PATH"
AFTER_DOC=$(metric_sum legacy-pages)
AFTER_DATA=$(metric_sum legacy-data)
[ $((AFTER_DOC - BEFORE_DOC)) -eq 1 ] || fail "the Pages Router document was rendered $((AFTER_DOC - BEFORE_DOC)) times instead of once"
[ $((AFTER_DATA - BEFORE_DATA)) -eq 1 ] || fail "the Pages Router data payload was rendered $((AFTER_DATA - BEFORE_DATA)) times instead of once"
grep -qi '^content-type: text/html' "$RESULT_DIR/legacy-doc.headers" || fail "the Pages Router document was not HTML"
grep -qi '^content-type: application/json' "$RESULT_DIR/legacy-data.headers" || fail "the Pages Router data route was not JSON"
grep -q '"pageProps"' "$RESULT_DIR/legacy-data.body" || fail "the data route did not return page props"
cmp -s "$RESULT_DIR/legacy-doc.body" "$RESULT_DIR/legacy-doc-again.body" || fail "the cached Pages Router document changed after a data request"
cmp -s "$RESULT_DIR/legacy-data.body" "$RESULT_DIR/legacy-data-again.body" || fail "the cached Pages Router data payload changed on reuse"
echo "PASS: document and data payload cached independently, one render each"

echo
echo "10/11 a Pages Router Set-Cookie is never shared either"
# Same permissive route that just collapsed sixteen requests into one render.
# `getServerSideProps` setting a cookie has to defeat it exactly as a Route
# Handler does: the barrier is a property of the response, not of the API used
# to produce it.
LEGACY_SESSION_COUNT=12
BEFORE=$(metric_sum legacy-pages)
seq 1 "$LEGACY_SESSION_COUNT" | xargs -P "$LEGACY_SESSION_COUNT" -I{} \
  curl -sS -D "$RESULT_DIR/legacy-session-{}.headers" -o /dev/null -w '%{http_code}\n' \
  "$PROXY_URL/legacy/session" > "$RESULT_DIR/legacy-session-status"
SERVED=$(grep -c '^200$' "$RESULT_DIR/legacy-session-status" || true)
AFTER=$(metric_sum legacy-pages)
ORIGIN_REQUESTS=$((AFTER - BEFORE))
LEGACY_SESSIONS=$(sed -n 's/^[Ss]et-[Cc]ookie: legacy_session=\([^;]*\).*/\1/p' \
  "$RESULT_DIR"/legacy-session-*.headers | tr -d '\r' | sort -u | wc -l | tr -d ' ')
[ "$SERVED" -eq "$LEGACY_SESSION_COUNT" ] || fail "served $SERVED/$LEGACY_SESSION_COUNT Pages Router session requests"
[ "$ORIGIN_REQUESTS" -eq "$LEGACY_SESSION_COUNT" ] || fail "$LEGACY_SESSION_COUNT cookie-setting requests caused $ORIGIN_REQUESTS origin requests"
[ "$LEGACY_SESSIONS" -eq "$LEGACY_SESSION_COUNT" ] || fail "$LEGACY_SESSION_COUNT requests returned only $LEGACY_SESSIONS distinct sessions"
echo "PASS: $LEGACY_SESSION_COUNT requests, $LEGACY_SESSION_COUNT renders, $LEGACY_SESSION_COUNT sessions"

echo
echo "11/11 the image optimiser is cached per negotiated format, not per URL"
# The failure this exists to catch shipped in the README for a while: Next
# content-negotiates the output format on `Accept` and answers `Vary: Accept`,
# so a route that keys only on url+w+q is refused storage entirely and quietly
# gets a 0% hit rate. Both halves are asserted here — that it caches at all,
# and that two clients with different `Accept` never share an entry.
IMG="$PROXY_URL/_next/image?url=%2Fbands.png&w=640&q=75"
WEBP_ACCEPT='image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8'
PNG_ACCEPT='image/png,image/*'

BEFORE=$(metric_sum next-image)
for n in 1 2; do
  curl -fsS -D "$RESULT_DIR/img-webp-$n.headers" -o /dev/null -H "Accept: $WEBP_ACCEPT" "$IMG"
done
for n in 1 2; do
  curl -fsS -D "$RESULT_DIR/img-png-$n.headers" -o /dev/null -H "Accept: $PNG_ACCEPT" "$IMG"
done
AFTER=$(metric_sum next-image)
ORIGIN_REQUESTS=$((AFTER - BEFORE))

tanod_status() { sed -n 's/^[Xx]-[Tt]anod: //p' "$1" | tr -d '\r'; }
img_type() { sed -n 's/^[Cc]ontent-[Tt]ype: //p' "$1" | tr -d '\r'; }

# The origin negotiated Vary: Accept, so Tanod must have honoured it.
grep -qi '^vary:.*accept' "$RESULT_DIR/img-webp-1.headers" \
  || fail "the image response carried no Vary: Accept; this assertion is no longer testing anything"

# Four requests, two distinct formats, so exactly two renders.
[ "$ORIGIN_REQUESTS" -eq 2 ] \
  || fail "four image requests in two formats caused $ORIGIN_REQUESTS origin requests instead of 2"
[ "$(tanod_status "$RESULT_DIR/img-webp-2.headers")" = "HIT" ] \
  || fail "the second WebP image request was $(tanod_status "$RESULT_DIR/img-webp-2.headers"), not HIT — check that cache.vary lists Accept"
[ "$(tanod_status "$RESULT_DIR/img-png-2.headers")" = "HIT" ] \
  || fail "the second PNG image request was $(tanod_status "$RESULT_DIR/img-png-2.headers"), not HIT"

# The safety half: a client that cannot read WebP must never be handed the
# cached WebP entry.
WEBP_TYPE=$(img_type "$RESULT_DIR/img-webp-2.headers")
PNG_TYPE=$(img_type "$RESULT_DIR/img-png-2.headers")
[ "$WEBP_TYPE" = "image/webp" ] || fail "WebP-capable client received $WEBP_TYPE"
[ "$PNG_TYPE" = "image/png" ] || fail "PNG-only client received $PNG_TYPE — formats are sharing a cache entry"

# Nothing was refused storage for a Vary the key could not honour.
UNSUPPORTED=$(curl -fsS "$METRICS_URL/metrics" | awk '
  /^tanod_cache_bypass_reason_total[{]/ && /reason="unsupported_vary"/ && /route="next-image"/ { sum += $2 }
  END { print sum + 0 }')
[ "$UNSUPPORTED" -eq 0 ] || fail "$UNSUPPORTED image responses were refused storage as unsupported_vary"

# And the low-priority tier ceiling is 50% of the global 8.
TIER_LOW=$(curl -fsS "$METRICS_URL/metrics" \
  | sed -n 's/^tanod_concurrency_limit{limiter="tier:low"} \([0-9]*\)$/\1/p' | head -1)
[ "${TIER_LOW:-0}" -eq 4 ] \
  || fail "the low-priority tier ceiling is ${TIER_LOW:-missing}, expected 4 (50% of 8)"
echo "PASS: 4 requests, 2 renders, WebP and PNG kept apart, low tier capped at 4"

echo
echo "PASS: real Next.js integration proof completed"
