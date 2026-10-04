#!/usr/bin/env bash
# Browser-driven half of the Next.js integration proof.
#
# bench/nextjs.sh asserts what Tanod does with requests a test wrote. This
# asserts what it does with requests Next.js's own client wrote: a router
# prefetch carrying a real `Next-Router-State-Tree`, and a Server Action POST
# carrying an action id this build assigned. Neither can be written down in
# advance, so neither is covered by curl.
#
# Brings the compose stack up itself if it is not already running, so it can be
# used standalone or straight after bench/nextjs.sh.
set -euo pipefail

cd "$(dirname "$0")/.."

COMPOSE=(docker compose -p tanod-nextjs-fixture -f compose.nextjs.yaml)
PROXY_URL=${PROXY_URL:-http://127.0.0.1:18080}
METRICS_URL=${METRICS_URL:-http://127.0.0.1:19090}
STARTED_STACK=0

cleanup() {
  if [ "$STARTED_STACK" = "1" ]; then
    "${COMPOSE[@]}" down --remove-orphans >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

fail() {
  echo "FAIL: $*" >&2
  "${COMPOSE[@]}" logs --no-color --tail=80 tanod >&2 || true
  exit 1
}

docker info >/dev/null 2>&1 || fail "no Docker daemon reachable"

if ! curl -fsS -o /dev/null --max-time 2 "$PROXY_URL/healthz" 2>/dev/null; then
  echo "Starting the Next.js fixture stack..."
  "${COMPOSE[@]}" up --build --detach || fail "the fixture stack did not start"
  STARTED_STACK=1
  for attempt in $(seq 1 90); do
    if curl -fsS -o /dev/null "$PROXY_URL/healthz" \
      && curl -fsS -o /dev/null "$METRICS_URL/metrics"; then
      break
    fi
    [ "$attempt" = 90 ] && fail "services did not become ready"
    sleep 1
  done
else
  echo "Using the fixture stack already running on $PROXY_URL"
fi

echo "Installing the browser harness (playwright + chromium)..."
( cd bench/browser && npm install --silent --no-audit --no-fund ) \
  || fail "could not install the browser harness"
# `--with-deps` is a no-op on macOS and installs the shared libraries Chromium
# needs on a bare CI runner. Failing here must be loud: a browser check that
# silently does not run is worse than no browser check.
( cd bench/browser && npx --yes playwright install --with-deps chromium ) \
  || fail "could not install Chromium for the browser checks"

echo
PROXY_URL="$PROXY_URL" METRICS_URL="$METRICS_URL" \
  node bench/browser/checks.mjs || fail "browser checks did not hold"
