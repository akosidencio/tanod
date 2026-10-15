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

COMPOSE=(docker compose -p "${TANOD_NEXTJS_PROJECT:-tanod-nextjs-fixture}" -f compose.nextjs.yaml)
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
BROWSER_IN_DOCKER=${TANOD_BROWSER_DOCKER:-auto}
if [ "$BROWSER_IN_DOCKER" = "1" ] || \
  { [ "$BROWSER_IN_DOCKER" = "auto" ] && [ "$(uname -s)" = "Linux" ] && ! command -v apt-get >/dev/null; }; then
  # Linux hosts outside Playwright's supported distributions can run the same
  # harness in its official image, with a browser matching the pinned package.
  PLAYWRIGHT_VERSION=$(node -p 'require("./bench/browser/node_modules/playwright/package.json").version')
  docker run --rm --network host \
    -v "$PWD:/workspace:ro" -w /workspace \
    -e PROXY_URL="$PROXY_URL" -e METRICS_URL="$METRICS_URL" \
    "mcr.microsoft.com/playwright:v${PLAYWRIGHT_VERSION}-noble" \
    node bench/browser/checks.mjs || fail "container browser checks did not hold"
else
  # Install the libraries Chromium needs on a bare CI runner. A failed install
  # remains a failure, never a skipped browser proof.
  if [ "${TANOD_BROWSER_INSTALL_DEPS:-1}" = "0" ]; then
    ( cd bench/browser && npx --yes playwright install chromium ) \
      || fail "could not install Chromium for the browser checks"
  else
    ( cd bench/browser && npx --yes playwright install --with-deps chromium ) \
      || fail "could not install Chromium for the browser checks"
  fi
  PROXY_URL="$PROXY_URL" METRICS_URL="$METRICS_URL" \
    node bench/browser/checks.mjs || fail "browser checks did not hold"
fi
