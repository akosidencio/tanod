#!/usr/bin/env bash
# Smoke-test a locally loaded Tanod image before it is published.
#
#   scripts/image-smoke.sh IMAGE EXPECTED_VERSION
#
# The release workflow runs this against the exact image it is about to push,
# loaded from the OCI archive that the publish job later uploads unchanged, so
# a failure here means nothing reaches the registry. It needs Docker, curl and
# python3 only: no toolchain, no registry, no Next.js build. Exits non-zero on
# the first failed check.
#
# Environment:
#   EXPECT_FEATURES   features `tanod version` must report (default: tls)
#   EXPECT_REVISION   if set, the org.opencontainers.image.revision label
#   SMOKE_REPORT      if set, a file to write one JSON object of results to
#
# The container runs as production runs it (ops/production/compose.yaml):
# read-only root, uid 10001, every capability dropped, no-new-privileges, a
# pid and memory limit, tmpfs for /tmp. Host networking keeps the origin on the
# runner's loopback, so nothing here listens beyond 127.0.0.1.
set -euo pipefail

IMAGE=${1:?usage: image-smoke.sh IMAGE EXPECTED_VERSION}
VERSION=${2:?usage: image-smoke.sh IMAGE EXPECTED_VERSION}
FEATURES=${EXPECT_FEATURES:-tls}
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d)"
NAME="tanod-smoke-$$"
ORIGIN_PID=""
STARTED=0

cleanup() {
  [ -z "$ORIGIN_PID" ] || kill "$ORIGIN_PID" 2>/dev/null || true
  if [ "$STARTED" = 1 ]; then
    docker logs "$NAME" > "$WORK/tanod.log" 2>&1 || true
    docker rm -f "$NAME" >/dev/null 2>&1 || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

fail() {
  echo "::error::image smoke test: $*" >&2
  if [ "$STARTED" = 1 ]; then
    echo "--- container log (last 40 lines)" >&2
    docker logs "$NAME" 2>&1 | tail -40 >&2 || true
  fi
  exit 1
}
ok() { echo "ok  $*"; }

free_port() {
  python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'
}

wait_status() { # url expected-status tries
  local got=""
  for _ in $(seq 1 "$3"); do
    got=$(curl -s -o /dev/null -w '%{http_code}' --max-time 1 "$1" || true)
    [ "$got" = "$2" ] && return 0
    sleep 0.1
  done
  echo "$got"
  return 1
}

docker image inspect "$IMAGE" > "$WORK/inspect.json" 2>/dev/null || fail "image $IMAGE is not loaded"

# 1. Image metadata: the identity and the unprivileged user are part of what is
#    published, so they are checked like behaviour.
python3 - "$WORK/inspect.json" "${EXPECT_REVISION:-}" <<'PY' || fail "image metadata"
import json, sys
config = json.load(open(sys.argv[1]))[0]["Config"]
labels = config.get("Labels") or {}
expected_revision = sys.argv[2]
problems = []
if config.get("User") not in ("tanod", "10001", "10001:10001"):
    problems.append(f"runs as {config.get('User')!r}, not the unprivileged tanod user")
if config.get("Entrypoint") != ["/usr/local/bin/tanod"]:
    problems.append(f"entrypoint {config.get('Entrypoint')!r}")
for key in ("org.opencontainers.image.revision", "io.tanod.build-inputs-sha256",
            "org.opencontainers.image.source"):
    if labels.get(key, "unknown") in ("", "unknown"):
        problems.append(f"label {key} missing")
if expected_revision and labels.get("org.opencontainers.image.revision") != expected_revision:
    problems.append(f"revision label {labels.get('org.opencontainers.image.revision')!r}, expected {expected_revision!r}")
if problems:
    print("\n".join(problems), file=sys.stderr)
    sys.exit(1)
PY
ok "metadata: non-root user, entrypoint, revision and build-input labels"

# 2–3. The binary runs on the image's empty base, reports the release version
#      and was compiled with the features the image promises.
version_line=$(docker run --rm --network none "$IMAGE" version) || fail "'tanod version' did not run"
case "$version_line" in
  "tanod $VERSION "*) ;;
  *) fail "version reports '$version_line', expected tanod $VERSION" ;;
esac
case "$version_line" in
  *"features: $FEATURES"*) ;;
  *) fail "version reports '$version_line', expected features: $FEATURES" ;;
esac
ok "version: $version_line"

# 4. Configuration validation accepts the production example and refuses a
#    configuration this release must refuse. The second half is a negative
#    control: a `check` that accepted everything would pass the first half.
docker run --rm --network none \
  -v "$ROOT/ops/production/compose-tanod.yaml:/etc/tanod/tanod.yaml:ro" \
  -e TANOD_PURGE_TOKEN=smoke-test-token-not-a-secret \
  "$IMAGE" check --config /etc/tanod/tanod.yaml > "$WORK/check.log" 2>&1 \
  || { cat "$WORK/check.log" >&2; fail "'tanod check' refused the production example"; }
cat > "$WORK/invalid.yaml" <<'YAML'
version: 1
server: {listen: "127.0.0.1:8080"}
origin:
  upstreams: ["127.0.0.1:9"]
  concurrency: {max: 1}
routes:
  - id: unclassified
    match: "/**"
    coalesce: {override_origin: true}
YAML
if docker run --rm --network none -v "$WORK/invalid.yaml:/etc/tanod/tanod.yaml:ro" \
  "$IMAGE" check --config /etc/tanod/tanod.yaml > "$WORK/invalid.log" 2>&1; then
  fail "'tanod check' accepted coalesce.override_origin without a class"
fi
grep -q 'without declaring a class' "$WORK/invalid.log" \
  || { cat "$WORK/invalid.log" >&2; fail "'tanod check' refused the invalid config for the wrong reason"; }
ok "check: production example accepted, unclassified coalesce override refused"

# 5–7. Start it hardened, against an origin that counts its own renders.
ORIGIN_PORT=$(free_port)
PROXY_PORT=$(free_port)
ADMIN_PORT=$(free_port)
METRICS_PORT=$(free_port)
python3 "$ROOT/scripts/smoke-origin.py" "$ORIGIN_PORT" &
ORIGIN_PID=$!
wait_status "http://127.0.0.1:$ORIGIN_PORT/healthz" 200 50 >/dev/null || fail "smoke origin did not start"

cat > "$WORK/tanod.yaml" <<YAML
version: 1
server:
  listen: "127.0.0.1:$PROXY_PORT"
  graceful:
    drain_period: 2s
    shutdown_timeout: 5s
origin:
  upstreams: ["127.0.0.1:$ORIGIN_PORT"]
  concurrency:
    max: 1
    queue:
      max: 1
      timeout: 100ms
health:
  path: /healthz
  interval: 1s
  timeout: 1s
cache:
  enabled: true
  max_memory: 8MiB
  max_body_size: 1MiB
timeouts:
  origin: 10s
telemetry:
  admin: {listen: "127.0.0.1:$ADMIN_PORT"}
  prometheus: {listen: "127.0.0.1:$METRICS_PORT"}
routes:
  - id: pages
    match: "/**"
    class: private_dynamic
YAML

docker run -d --name "$NAME" --network host --read-only \
  --user 10001:10001 --cap-drop ALL --security-opt no-new-privileges \
  --pids-limit 128 --memory 128m \
  --tmpfs /tmp:uid=10001,gid=10001,mode=1777,size=8m \
  -v "$WORK/tanod.yaml:/etc/tanod/tanod.yaml:ro" "$IMAGE" > /dev/null \
  || fail "container did not start"
STARTED=1
ADMIN="http://127.0.0.1:$ADMIN_PORT"
PROXY="http://127.0.0.1:$PROXY_PORT"
wait_status "$ADMIN/health/live" 200 100 >/dev/null || fail "liveness never answered 200"
got=$(wait_status "$ADMIN/health/ready" 200 100) || fail "readiness answered '$got', not 200"
ok "started read-only, non-root, no capabilities; live and ready"

body=$(curl -fsS --max-time 5 "$PROXY/hello") || fail "proxy did not serve /hello"
[ "$body" = "hello from the smoke origin" ] || fail "proxy served '$body'"
ok "proxy served an origin response"

status=$(curl -s -o /dev/null -w '%{http_code}' --path-as-is --max-time 5 "$PROXY/x/../hello")
[ "$status" = 400 ] || fail "dot-segment target answered $status, not 400"
ok "dot-segment target refused with 400"

# 8–9. Admission: eight concurrent renders against a ceiling of one with a queue
#      of one. Some are shed with 503 and Retry-After, some are served, and the
#      origin's own count never passes one.
load_pids=""
for i in $(seq 1 8); do
  curl -s -o /dev/null -D "$WORK/h$i" -w '%{http_code}\n' --max-time 10 \
    "$PROXY/slow/1500?i=$i" > "$WORK/s$i" &
  load_pids="$load_pids $!"
done
# These requests only: a bare `wait` would also wait for the origin.
for pid in $load_pids; do wait "$pid" || true; done
served=$(cat "$WORK"/s* | grep -c '^200$' || true)
shed=$(cat "$WORK"/s* | grep -c '^503$' || true)
retry_after=$(grep -il '^retry-after:' "$WORK"/h* | wc -l)
witness=$(curl -fsS "http://127.0.0.1:$ORIGIN_PORT/__witness")
peak=$(python3 -c 'import json, sys; print(json.loads(sys.argv[1])["peak"])' "$witness")
[ "$served" -ge 1 ] || fail "no request was served under load ($(cat "$WORK"/s* | tr '\n' ' '))"
[ "$shed" -ge 1 ] || fail "nothing was shed past the ceiling ($(cat "$WORK"/s* | tr '\n' ' '))"
[ $((served + shed)) = 8 ] || fail "unexpected statuses: $(cat "$WORK"/s* | tr '\n' ' ')"
[ "$retry_after" -ge "$shed" ] || fail "$shed shed responses but $retry_after carried Retry-After"
[ "$peak" -le 1 ] || fail "origin peak concurrency $peak exceeded the ceiling of 1"
ok "admission: $served served, $shed shed with Retry-After, origin peak $peak / 1"

metrics=$(curl -fsS "http://127.0.0.1:$METRICS_PORT/metrics") || fail "metrics endpoint unavailable"
grep -q '^tanod_process_start_time_seconds [1-9]' <<< "$metrics" || fail "metrics lack tanod_process_start_time_seconds"
grep -q '^tanod_build_info' <<< "$metrics" || fail "metrics lack tanod_build_info"
ok "metrics exported"

# 10. SIGTERM: readiness withdraws while traffic is still served, then the
#     process exits 0 inside its drain and shutdown budget.
started=$(date +%s)
docker stop --time 30 "$NAME" > /dev/null &
stop_pid=$!
got=$(wait_status "$ADMIN/health/ready" 503 50) || fail "readiness answered '$got' during the drain window, not 503"
curl -fsS --max-time 2 "$PROXY/hello" > /dev/null || fail "traffic failed inside the drain window"
wait "$stop_pid" || fail "docker stop failed"
elapsed=$(( $(date +%s) - started ))
exit_code=$(docker inspect "$NAME" --format '{{.State.ExitCode}}')
[ "$exit_code" = 0 ] || fail "exit code $exit_code after SIGTERM"
[ "$elapsed" -lt 25 ] || fail "shutdown took ${elapsed}s, past the 2s drain and 5s shutdown budget"
if docker logs "$NAME" 2>&1 | grep -q 'panicked'; then
  fail "the log contains a panic"
fi
ok "SIGTERM: not-ready while serving, exit 0 after ${elapsed}s, no panic"

if [ -n "${SMOKE_REPORT:-}" ]; then
  python3 - "$SMOKE_REPORT" "$IMAGE" "$version_line" "$served" "$shed" "$peak" "$elapsed" <<'PY'
import json, sys
path, image, version, served, shed, peak, elapsed = sys.argv[1:]
json.dump({"image": image, "version": version, "served": int(served), "shed": int(shed),
           "origin_peak": int(peak), "ceiling": 1, "shutdown_seconds": int(elapsed),
           "result": "pass"}, open(path, "w"), indent=2)
PY
fi
echo "image smoke test passed: $IMAGE"
