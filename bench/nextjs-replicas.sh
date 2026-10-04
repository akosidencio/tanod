#!/usr/bin/env bash
# Static group-budget, URI-affinity, purge fan-out, and replica-loss proof.
set -euo pipefail

cd "$(dirname "$0")/.."

COMPOSE=(docker compose -p tanod-nextjs-replicated -f compose.nextjs.yaml -f compose.nextjs-replicated.yaml)
EDGE=http://127.0.0.1:18080
RESULT_DIR=$(mktemp -d)

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

global_in_flight() {
  curl -fsS "$1/metrics" | awk '
    /^tanod_origin_in_flight[{]/ && /limiter="global"/ { sum += $2 }
    END { print sum + 0 }'
}

origin_requests() {
  curl -fsS "$1/metrics" | awk '
    /^tanod_origin_requests_total[{]/ { sum += $2 }
    END { print sum + 0 }'
}

tanod_status() {
  sed -n 's/^[Xx]-[Tt]anod: //p' "$1" | tr -d '\r'
}

target/debug/tanod check --config bench/nextjs-replicated.yaml >/dev/null \
  || fail "the statically partitioned configuration was rejected"
"${COMPOSE[@]}" down --volumes --remove-orphans >/dev/null 2>&1 || true
"${COMPOSE[@]}" up --build --detach || fail "the replicated reference did not start"

for attempt in $(seq 1 90); do
  if curl -fsS -o /dev/null "$EDGE/healthz" \
    && curl -fsS -o /dev/null http://127.0.0.1:19191/health/ready \
    && curl -fsS -o /dev/null http://127.0.0.1:19291/health/ready; then
    break
  fi
  [ "$attempt" = 90 ] && fail "the replicated reference did not become ready"
  sleep 1
done

for admin in http://127.0.0.1:19191 http://127.0.0.1:19291; do
  curl -fsS "$admin/status" | grep -q \
    '"capacity":{"global_max":8,"replicas":2,"per_replica":4,"allocated":8}' \
    || fail "$admin does not report the declared group capacity"
done

# Consistent hashing by URI keeps a cache key on one Tanod process.
AFFINITY_URL="$EDGE/products/affinity-$$"
curl -fsS -D "$RESULT_DIR/affinity-first.headers" -o /dev/null "$AFFINITY_URL"
curl -fsS -D "$RESULT_DIR/affinity-second.headers" -o /dev/null "$AFFINITY_URL"
[ "$(tanod_status "$RESULT_DIR/affinity-first.headers")" = "MISS" ] \
  || fail "the affinity precondition did not start with a miss"
[ "$(tanod_status "$RESULT_DIR/affinity-second.headers")" = "HIT" ] \
  || fail "the same URI did not return to its local Tanod cache"

# Put one path in both local caches, then use @tanod/next fan-out and prove
# neither process can serve its old response.
PURGE_PATH="/products/purge-replicas-$$"
for port in 18181 18282; do
  curl -fsS -H 'Host: storefront.test' -D "$RESULT_DIR/fill-$port.headers" \
    -o /dev/null "http://127.0.0.1:$port$PURGE_PATH"
  curl -fsS -H 'Host: storefront.test' -D "$RESULT_DIR/hit-$port.headers" \
    -o /dev/null "http://127.0.0.1:$port$PURGE_PATH"
  [ "$(tanod_status "$RESULT_DIR/hit-$port.headers")" = "HIT" ] \
    || fail "Tanod on port $port did not fill its local cache"
done

# The single-quoted program is JavaScript; its template expression belongs to Node.
# shellcheck disable=SC2016
PURGE_PATH="$PURGE_PATH" node --input-type=module -e '
  import { createPurger } from "./packages/tanod-next/src/index.js";
  const purger = createPurger({
    endpoints: ["http://127.0.0.1:19191", "http://127.0.0.1:19291"],
    token: "nextjs-reference-purge-token-0001",
  });
  const result = await purger.purgePaths([process.env.PURGE_PATH]);
  if (result.replicas !== 2) throw new Error(`expected 2 purge results, got ${result.replicas}`);
' || fail "replicated purge did not reach both Tanod admin listeners"

for port in 18181 18282; do
  curl -fsS -H 'Host: storefront.test' -D "$RESULT_DIR/purged-$port.headers" \
    -o /dev/null "http://127.0.0.1:$port$PURGE_PATH"
  [ "$(tanod_status "$RESULT_DIR/purged-$port.headers")" = "MISS" ] \
    || fail "Tanod on port $port retained a purged response"
done

# Distinct paths bypass reuse. The sum of both local limiters must stay inside
# the declared group budget even when offered much more parallel work.
PEAK=0
RUN_ID="capacity-$(date +%s)-$$"
seq 1 40 | xargs -P 40 -I{} \
  curl -sS -o /dev/null -w '%{http_code}\n' "$EDGE/products/$RUN_ID-{}" \
  > "$RESULT_DIR/capacity-status" &
LOAD_PID=$!
while kill -0 "$LOAD_PID" 2>/dev/null; do
  FIRST=$(global_in_flight http://127.0.0.1:19190)
  SECOND=$(global_in_flight http://127.0.0.1:19290)
  COMBINED=$((FIRST + SECOND))
  [ "$COMBINED" -gt "$PEAK" ] && PEAK=$COMBINED
  [ "$PEAK" -le 8 ] || fail "combined origin work exceeded the global budget: $PEAK"
  sleep 0.02
done
wait "$LOAD_PID" || fail "the group-capacity burst failed"
[ "$(grep -c '^200$' "$RESULT_DIR/capacity-status" || true)" -eq 40 ] \
  || fail "the group-capacity burst did not serve all requests"
[ "$PEAK" -ge 6 ] || fail "the burst reached only $PEAK concurrent work; it did not test the ceiling"

# Abrupt loss of one governor must reduce capacity instead of producing an
# outage. After restart, URI hashing must use both replicas again.
"${COMPOSE[@]}" kill -s SIGKILL tanod-1 >/dev/null
FAILURE_RUN="failure-$(date +%s)-$$"
seq 1 16 | xargs -P 16 -I{} \
  curl -sS --retry 2 --retry-connrefused -o /dev/null -w '%{http_code}\n' \
    "$EDGE/products/$FAILURE_RUN-{}" > "$RESULT_DIR/failure-status"
[ "$(grep -c '^200$' "$RESULT_DIR/failure-status" || true)" -eq 16 ] \
  || fail "traffic failed while one Tanod replica was unavailable"

"${COMPOSE[@]}" start tanod-1 >/dev/null
for attempt in $(seq 1 60); do
  curl -fsS -o /dev/null http://127.0.0.1:19191/health/ready && break
  [ "$attempt" = 60 ] && fail "the stopped Tanod replica did not recover"
  sleep 1
done
sleep 3
FIRST_BEFORE=$(origin_requests http://127.0.0.1:19190)
SECOND_BEFORE=$(origin_requests http://127.0.0.1:19290)
RECOVERY_RUN="recovery-$(date +%s)-$$"
seq 1 24 | xargs -P 24 -I{} \
  curl -fsS -o /dev/null "$EDGE/products/$RECOVERY_RUN-{}"
FIRST_AFTER=$(origin_requests http://127.0.0.1:19190)
SECOND_AFTER=$(origin_requests http://127.0.0.1:19290)
[ "$FIRST_AFTER" -gt "$FIRST_BEFORE" ] \
  || fail "the recovered Tanod replica received no origin work"
[ "$SECOND_AFTER" -gt "$SECOND_BEFORE" ] \
  || fail "the surviving Tanod replica received no origin work after recovery"

echo "PASS: static group budget, URI affinity, purge fan-out, failure, and recovery"
