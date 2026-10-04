#!/usr/bin/env bash
# Production-reference checks that need the full edge -> Tanod -> Next stack.
set -euo pipefail

cd "$(dirname "$0")/.."

COMPOSE=(docker compose -p tanod-nextjs-reference -f compose.nextjs.yaml)
RESULT_DIR=$(mktemp -d)
mkdir -p "$RESULT_DIR/config"
cp bench/nextjs.yaml "$RESULT_DIR/config/nextjs.yaml"
export TANOD_CONFIG_DIR="$RESULT_DIR/config"

cleanup() {
  "${COMPOSE[@]}" down --volumes --remove-orphans >/dev/null 2>&1 || true
  rm -rf "$RESULT_DIR"
}
trap cleanup EXIT

fail() {
  echo "FAIL: $*" >&2
  "${COMPOSE[@]}" logs --no-color --tail=100 >&2 || true
  exit 1
}

config_generation() {
  curl -fsS http://127.0.0.1:19091/status \
    | sed -n 's/.*"generation":\([0-9]*\).*/\1/p'
}

reload_and_wait() {
  local before generation
  before=$(config_generation)
  "${COMPOSE[@]}" kill -s SIGHUP tanod >/dev/null
  for _ in $(seq 1 30); do
    generation=$(config_generation)
    [ "${generation:-0}" -gt "${before:-0}" ] && return 0
    sleep 0.2
  done
  fail "configuration reload did not apply"
}

"${COMPOSE[@]}" down --volumes --remove-orphans >/dev/null 2>&1 || true
"${COMPOSE[@]}" up --build --detach || fail "reference stack did not start"

for attempt in $(seq 1 90); do
  if curl -fsS -o /dev/null http://127.0.0.1:18080/healthz \
    && curl -fsS -o /dev/null http://127.0.0.1:19091/health/ready; then
    break
  fi
  [ "$attempt" = 90 ] && fail "reference stack did not become ready"
  sleep 1
done

CONTAINER_ID=$("${COMPOSE[@]}" ps -q next-1)
[ -n "$CONTAINER_ID" ] || fail "next-1 container was not found"
docker cp "$CONTAINER_ID:/app/.next" "$RESULT_DIR/.next" >/dev/null

node packages/tanod-next/src/cli.js doctor \
  --dist-dir "$RESULT_DIR/.next" \
  --config bench/nextjs.yaml \
  --tanod-bin target/debug/tanod \
  --origin http://127.0.0.1:13001 \
  --origin http://127.0.0.1:13002 \
  --origin http://127.0.0.1:13003 \
  --traffic http://127.0.0.1:18080 \
  --metrics http://127.0.0.1:19090 \
  --admin http://127.0.0.1:19091 \
  --public-url http://127.0.0.1:18080 \
  --doctor-token nextjs-reference-doctor-token \
  --purge-token nextjs-reference-purge-token-0001 \
  --stream-path "/flash-sale?doctor=$$" \
  || fail "tanod-next doctor found a reference deployment problem"

# Observe mode must still proxy and classify requests while disabling all
# protection and reuse, even though the route itself enables both.
OBSERVE_BEFORE=$(curl -fsS http://127.0.0.1:19090/metrics | awk '
  /^tanod_admission_total[{]/ && /decision="observe"/ { sum += $2 }
  END { print sum + 0 }')
sed 's/^mode: protect$/mode: observe/' \
  "$RESULT_DIR/config/nextjs.yaml" > "$RESULT_DIR/config/nextjs.yaml.next"
mv "$RESULT_DIR/config/nextjs.yaml.next" "$RESULT_DIR/config/nextjs.yaml"
reload_and_wait
OBSERVE_URL="http://127.0.0.1:18080/products/observe-$$"
curl -fsS -o "$RESULT_DIR/observe-first.body" "$OBSERVE_URL"
curl -fsS -o "$RESULT_DIR/observe-second.body" "$OBSERVE_URL"
OBSERVE_AFTER=$(curl -fsS http://127.0.0.1:19090/metrics | awk '
  /^tanod_admission_total[{]/ && /decision="observe"/ { sum += $2 }
  END { print sum + 0 }')
[ $((OBSERVE_AFTER - OBSERVE_BEFORE)) -eq 2 ] \
  || fail "observe mode did not record both requests without admission"
cmp -s "$RESULT_DIR/observe-first.body" "$RESULT_DIR/observe-second.body" \
  && fail "observe mode reused a public response"
sed 's/^mode: observe$/mode: protect/' \
  "$RESULT_DIR/config/nextjs.yaml" > "$RESULT_DIR/config/nextjs.yaml.next"
mv "$RESULT_DIR/config/nextjs.yaml.next" "$RESULT_DIR/config/nextjs.yaml"
reload_and_wait

# A deployment-id reload must make the old response unreachable before the
# next request. The origin remains the same so this isolates Tanod's rollover.
ROLLOVER_URL="http://127.0.0.1:18080/products/rollover-$$"
curl -fsS -D "$RESULT_DIR/rollover-first.headers" -o "$RESULT_DIR/rollover-first.body" "$ROLLOVER_URL"
curl -fsS -D "$RESULT_DIR/rollover-hit.headers" -o /dev/null "$ROLLOVER_URL"
grep -qi '^x-tanod: HIT' "$RESULT_DIR/rollover-hit.headers" \
  || fail "rollover precondition did not produce a cache hit"
sed 's/id: "next-fixture-v1"/id: "next-fixture-v2"/' \
  "$RESULT_DIR/config/nextjs.yaml" > "$RESULT_DIR/config/nextjs.yaml.next"
mv "$RESULT_DIR/config/nextjs.yaml.next" "$RESULT_DIR/config/nextjs.yaml"
reload_and_wait
curl -fsS -D "$RESULT_DIR/rollover-after.headers" -o "$RESULT_DIR/rollover-after.body" "$ROLLOVER_URL"
grep -qi '^x-tanod: MISS' "$RESULT_DIR/rollover-after.headers" \
  || fail "the old deployment response remained reachable after rollover"
cmp -s "$RESULT_DIR/rollover-first.body" "$RESULT_DIR/rollover-after.body" \
  && fail "the rollover returned the previous render"

# Restore the reference identity before lifecycle checks.
sed 's/id: "next-fixture-v2"/id: "next-fixture-v1"/' \
  "$RESULT_DIR/config/nextjs.yaml" > "$RESULT_DIR/config/nextjs.yaml.next"
mv "$RESULT_DIR/config/nextjs.yaml.next" "$RESULT_DIR/config/nextjs.yaml"
reload_and_wait

# Both lifecycle layers must finish an in-flight Suspense response during a
# normal container restart.
curl -fsS "http://127.0.0.1:18080/flash-sale?restart=tanod-$$" \
  > "$RESULT_DIR/tanod-restart.body" &
TANOD_CURL=$!
sleep 0.2
"${COMPOSE[@]}" restart -t 25 tanod >/dev/null
wait "$TANOD_CURL" || fail "Tanod restart dropped an in-flight stream"

curl -fsS "http://127.0.0.1:13001/flash-sale?restart=next-$$" \
  > "$RESULT_DIR/next-restart.body" &
NEXT_CURL=$!
sleep 0.2
"${COMPOSE[@]}" restart -t 10 next-1 >/dev/null
wait "$NEXT_CURL" || fail "Next.js restart dropped an in-flight stream"

echo "PASS: production-reference topology and coordination checks completed"
