#!/usr/bin/env bash
# Run only against a freshly rebuilt local candidate image. No publication.
# Tests quotas and hardening, then the production Compose template with a real
# Next.js origin. All containers, networks and ports are unique to this run.
BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"
: "${TANOD_IMAGE:?build the final candidate and set TANOD_IMAGE}"
export SSR_IMAGE=${SSR_IMAGE:-tanod-next-origin:reference}
bench_init container
IMAGE_ID=$(docker image inspect "$TANOD_IMAGE" --format '{{.Id}}') || bench_fail 'candidate image missing'
bench_param image "$IMAGE_ID"
bench_param origin_image "$(docker image inspect "$SSR_IMAGE" --format '{{.Id}}')"
bench_param cgroup_version "$(docker info --format '{{.CgroupVersion}}')"
PROJECT="tanod-production-check-$$"
CONTAINER=""
COMPOSE=(docker compose -p "$PROJECT" -f "$BENCH_ROOT/ops/production/compose.yaml")
cleanup() {
  [ -z "$CONTAINER" ] || docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  "${COMPOSE[@]}" down --volumes --remove-orphans >/dev/null 2>&1 || true
  bench_cleanup
}
trap cleanup EXIT
cat > "$BENCH_DIR/quota.yaml" <<'YAML'
version: 1
server:
  listen: "127.0.0.1:8080"
  graceful:
    drain_period: 0s
    shutdown_timeout: 1s
origin:
  upstreams: ["127.0.0.1:9"]
  concurrency: {max: 1}
cache: {enabled: false, max_memory: 8MiB, max_body_size: 1MiB}
YAML
# Rust rounds fractional capacity down, with a minimum of one. Standard
# library detection is verified here rather than reimplemented in Tanod.
for quota in 0.5 1 1.5 2 4; do
  case "$quota" in 0.5|1|1.5) expected=1 ;; *) expected=$quota ;; esac
  CONTAINER="$PROJECT-quota-${quota//./-}"
  docker run -d --name "$CONTAINER" --network none --read-only \
    --user 10001:10001 --cap-drop ALL --security-opt no-new-privileges \
    --pids-limit 128 --memory 64m --cpus "$quota" \
    --tmpfs /tmp:uid=10001,gid=10001,mode=1777,size=8m \
    -v "$BENCH_DIR/quota.yaml:/etc/tanod/tanod.yaml:ro" "$TANOD_IMAGE" >/dev/null \
    || bench_fail "could not start quota $quota"
  observed=""
  for _ in $(seq 1 100); do
    observed=$(docker logs "$CONTAINER" 2>&1 | sed -n 's/.*(\([0-9]*\) worker threads).*/\1/p')
    [ -n "$observed" ] && break
    sleep 0.1
  done
  bench_assert_eq "$observed" "$expected" "default workers under $quota CPU quota"
  printf '{"quota":%s,"workers":%s,"memory_limit_mib":64,"image":"%s"}\n' \
    "$quota" "$observed" "$IMAGE_ID" >> "$BENCH_DIR/quotas.jsonl"
  docker stop --time 10 "$CONTAINER" >/dev/null || bench_fail "quota process did not stop"
  exit_code=$(docker inspect "$CONTAINER" --format '{{.State.ExitCode}}')
  bench_assert_eq "$exit_code" 0 'graceful quota container exit'
  docker rm "$CONTAINER" >/dev/null
  CONTAINER=""
done
if [ -n "${BENCH_REPORT_DIR:-}" ]; then
  mkdir -p "$BENCH_REPORT_DIR"
  cp "$BENCH_DIR/quotas.jsonl" "$BENCH_REPORT_DIR/container-quotas.jsonl"
fi

export TANOD_PURGE_TOKEN=container-validation-fixture-only
export TANOD_HTTP_PORT=$(bench_free_port)
export TANOD_ADMIN_PORT=$(bench_free_port)
export TANOD_METRICS_PORT=$(bench_free_port)
"${COMPOSE[@]}" config --quiet || bench_fail 'invalid production Compose'
"${COMPOSE[@]}" up -d --no-build || bench_fail 'production Compose failed to start'
bench_wait_http "http://127.0.0.1:$TANOD_HTTP_PORT/healthz" 'real Next.js origin through hardened Tanod'
bench_wait_http "http://127.0.0.1:$TANOD_ADMIN_PORT/health/ready" 'readiness'
base="http://127.0.0.1:$TANOD_ADMIN_PORT"
curl -fsS "$base/status" > "$BENCH_DIR/status.json" || bench_fail 'status unavailable'
if grep -q "$TANOD_PURGE_TOKEN" "$BENCH_DIR/status.json"; then
  bench_fail 'status exposed purge credential'
fi
bad=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$base/purge?all=1" -H 'Authorization: Bearer wrong')
bench_assert_eq "$bad" 401 'invalid purge credential'
good=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$base/purge?all=1" -H "Authorization: Bearer $TANOD_PURGE_TOKEN")
bench_assert_eq "$good" 200 'authenticated purge'
curl -fsS "http://127.0.0.1:$TANOD_METRICS_PORT/metrics" > "$BENCH_DIR/metrics.txt" || bench_fail 'metrics unavailable'
grep -q '^tanod_process_start_time_seconds [1-9]' "$BENCH_DIR/metrics.txt" || bench_fail 'missing start timestamp'
cid=$("${COMPOSE[@]}" ps -q tanod)
security=$(docker inspect "$cid" --format '{{.HostConfig.ReadonlyRootfs}} {{.Config.User}} {{json .HostConfig.CapDrop}} {{json .HostConfig.SecurityOpt}}')
case "$security" in 'true 10001:10001 ["ALL"]'*'no-new-privileges'*) ;; *) bench_fail "unexpected hardening: $security" ;; esac
"${COMPOSE[@]}" stop --timeout 50 tanod > "$BENCH_DIR/logs/stop.log" 2>&1 &
stop_pid=$!
ready=""
for _ in $(seq 1 30); do
  ready=$(curl -s -o /dev/null -w '%{http_code}' --max-time 1 "$base/health/ready")
  [ "$ready" = 503 ] && break
  sleep 0.1
done
bench_assert_eq "$ready" 503 'SIGTERM readiness window in hardened container'
curl -fsS "$base/health/live" >/dev/null || bench_fail 'liveness followed readiness'
curl -fsS "http://127.0.0.1:$TANOD_HTTP_PORT/healthz" >/dev/null || bench_fail 'traffic failed inside drain window'
wait "$stop_pid" || bench_fail 'Compose stop failed'
bench_assert_eq "$(docker inspect "$cid" --format '{{.State.ExitCode}}')" 0 'production container exited intentionally'
bench_result quotas_checked 5
bench_result hardened_compose passed
bench_pass 'CPU quotas, non-root read-only deployment, real origin, private admin auth and graceful stop'
