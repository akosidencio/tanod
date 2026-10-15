#!/usr/bin/env bash
# Run the full real-framework proof for one declared compatibility pair.
set -euo pipefail
cd "$(dirname "$0")/.."

node scripts/nextjs-compatibility.mjs assert
export TANOD_NEXTJS_PROJECT="tanod-compat-${TANOD_NEXT_VERSION//./-}-node-${TANOD_NEXT_NODE}"
export TANOD_NEXT_IMAGE="tanod-next-origin:next-${TANOD_NEXT_VERSION}-node-${TANOD_NEXT_NODE}"
# Reuse the HTTP stack for the browser check. This runner owns cleanup even
# when an assertion fails; reference/replica scripts still reset their state.
export TANOD_NEXTJS_KEEP_STACK=1
cleanup() {
  docker compose -p "$TANOD_NEXTJS_PROJECT" -f compose.nextjs.yaml \
    -f compose.nextjs-replicated.yaml down --volumes --remove-orphans >/dev/null 2>&1 || true
}
trap cleanup EXIT
REPORT_DIR=${BENCH_REPORT_DIR:-compatibility-results/next-${TANOD_NEXT_VERSION}-node-${TANOD_NEXT_NODE}}
mkdir -p "$REPORT_DIR"
export TANOD_COMPAT_REPORT_DIR="$REPORT_DIR"
node scripts/nextjs-compatibility.mjs matrix > "$REPORT_DIR/required-matrix.json"
printf '{"next":"%s","node":"%s"}\n' "$TANOD_NEXT_VERSION" "$TANOD_NEXT_NODE" > "$REPORT_DIR/target.json"
printf '{"passed":false,"next":"%s","node":"%s"}\n' \
  "$TANOD_NEXT_VERSION" "$TANOD_NEXT_NODE" > "$REPORT_DIR/result.json"

for suite in nextjs nextjs-browser nextjs-reference nextjs-replicas; do
  echo "Checking Next $TANOD_NEXT_VERSION / Node $TANOD_NEXT_NODE: $suite"
  bash "bench/$suite.sh" 2>&1 | tee "$REPORT_DIR/$suite.log"
done
printf '{"passed":true,"next":"%s","node":"%s"}\n' \
  "$TANOD_NEXT_VERSION" "$TANOD_NEXT_NODE" > "$REPORT_DIR/result.json"
