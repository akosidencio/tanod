#!/usr/bin/env bash
# The Tanod overload showcase: NGINX straight to three Next.js origins, against
# the same NGINX in front of Tanod in front of the same origins, under the same
# open-model k6 load. Read docs/demo/README.md first.
#
#   bench/showcase/run.sh                     # delay mode (mechanism test), both workloads
#   bench/showcase/run.sh --mode cpu          # CPU-bound origin work (opt-in)
#   bench/showcase/run.sh --check             # validate configuration only, no load
#
# Each case (mode x workload x arm x trial) gets a freshly started stack, a
# warmup, a measured run, a drain and a teardown. The two arms never run at the
# same time. Results land in benchmark-results/showcase/<run id>/ unless --out
# says otherwise.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
SHOWCASE="$HERE/showcase.py"
PROJECT=${SHOWCASE_PROJECT:-tanod-showcase}
K6_IMAGE=${SHOWCASE_K6_IMAGE:-grafana/k6:2.3.0}
NGINX_IMAGE=${SHOWCASE_NGINX_IMAGE:-nginx:1.27-alpine}
NEXT_IMAGE=${SHOWCASE_NEXT_IMAGE:-tanod-next-origin:showcase}
TANOD_IMAGE=${SHOWCASE_TANOD_IMAGE:-tanod:showcase}
export SHOWCASE_NGINX_IMAGE=$NGINX_IMAGE SHOWCASE_NEXT_IMAGE=$NEXT_IMAGE SHOWCASE_TANOD_IMAGE=$TANOD_IMAGE

usage() {
  cat <<'EOF'
Usage: bench/showcase/run.sh [options]

Experiment
  --mode LIST         delay (default; mechanism test) and/or cpu (constrained), comma-separated
  --workload LIST     unique,repeated (default both)
  --arms LIST         baseline,tanod (default both)
  --trials N          repeat every case N times, alternating arm order (default 1)
  --rate N            offered requests/second during the measured phase (default per mode)
  --duration S        measured phase length in seconds (default 60)
  --profile P         constant (default) or spike (base, ramp, peak, recovery)
  --ceiling N         Tanod global origin concurrency (default per mode)
  --set NAME=VALUE    any other parameter, e.g. --set warmup_s=30 (see showcase.py DEFAULTS)

Run control
  --out DIR           output directory (default benchmark-results/showcase/<run id>)
  --observe           also run Prometheus + Grafana (http://127.0.0.1:13400) and stream k6 metrics
  --no-build          use existing images instead of building them
  --keep              leave the last stack running at the end
  --check             validate configuration, images and tools, then exit without load
  --analyze DIR       re-analyse and re-plot an existing run directory
  -h, --help          this help
EOF
}

die() { echo "showcase: $*" >&2; exit 1; }
log() { printf '\n[%s] %s\n' "$(date +%H:%M:%S)" "$*"; }

OUT="" BUILD=1 KEEP=0 CHECK_ONLY=0 ANALYZE_ONLY=""
while [ $# -gt 0 ]; do
  case "$1" in
    --mode|--modes) export SHOWCASE_MODES=$2; shift 2 ;;
    --workload|--workloads) export SHOWCASE_WORKLOADS=$2; shift 2 ;;
    --arms) export SHOWCASE_ARMS=$2; shift 2 ;;
    --trials) export SHOWCASE_TRIALS=$2; shift 2 ;;
    --rate) export SHOWCASE_RATE=$2; shift 2 ;;
    --duration) export SHOWCASE_DURATION_S=$2; shift 2 ;;
    --profile) export SHOWCASE_PROFILE=$2; shift 2 ;;
    --ceiling) export SHOWCASE_TANOD_MAX=$2; shift 2 ;;
    --set)
      name=${2%%=*} value=${2#*=}
      [ "$name" != "$2" ] && [ -n "$name" ] || die "--set expects NAME=VALUE"
      export "SHOWCASE_$(printf '%s' "$name" | tr '[:lower:]' '[:upper:]')=$value"
      shift 2 ;;
    --out) OUT=$2; shift 2 ;;
    --observe) export SHOWCASE_OBSERVE=1; shift ;;
    --no-build) BUILD=0; shift ;;
    --keep) KEEP=1; shift ;;
    --check) CHECK_ONLY=1; shift ;;
    --analyze) ANALYZE_ONLY=$2; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; die "unknown option: $1" ;;
  esac
done

command -v python3 >/dev/null || die "python3 is required"

# Plotting is the only step that needs Matplotlib. SHOWCASE_PLOT_PYTHON may name
# an interpreter that has it; otherwise python3, then `uv run --with`.
plot_python() {
  if [ -n "${SHOWCASE_PLOT_PYTHON:-}" ]; then echo "$SHOWCASE_PLOT_PYTHON"; return; fi
  if python3 -c 'import matplotlib' 2>/dev/null; then echo python3; return; fi
  if command -v uv >/dev/null; then echo "uv run --no-project --with matplotlib==3.10.1 python"; return; fi
  echo ""
}

analyze_run() { # run dir
  python3 "$SHOWCASE" aggregate --run "$1"
  local py
  py=$(plot_python)
  if [ -z "$py" ]; then
    echo "  charts skipped: no Python with Matplotlib (set SHOWCASE_PLOT_PYTHON, or install uv)"
    return
  fi
  # shellcheck disable=SC2086 # $py may be a multi-word command
  $py "$HERE/plot.py" "$1"
}

if [ -n "$ANALYZE_ONLY" ]; then
  [ -f "$ANALYZE_ONLY/params.json" ] || die "$ANALYZE_ONLY has no params.json"
  for case_dir in "$ANALYZE_ONLY"/cases/*/; do
    [ -f "$case_dir/k6-summary.json" ] && python3 "$SHOWCASE" case --params "$ANALYZE_ONLY/params.json" --dir "$case_dir"
  done
  analyze_run "$ANALYZE_ONLY"
  exit 0
fi

# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------

docker info >/dev/null 2>&1 || die "Docker is not reachable (docker info failed)"
docker compose version >/dev/null 2>&1 || die "the Docker Compose plugin is required"

RUN_ID=${SHOWCASE_RUN_ID:-$(date -u +%Y%m%d-%H%M%S)}
OUT=${OUT:-$REPO/benchmark-results/showcase/$RUN_ID}
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)
PARAMS="$OUT/params.json"
python3 "$SHOWCASE" params --out "$PARAMS" || die "invalid parameters"
get() { python3 "$SHOWCASE" get --params "$PARAMS" "$@"; }

# Compose reads these; they come from params.json so the run records them.
export SHOWCASE_PORT=$(get port)
export SHOWCASE_ORIGIN_CPUS=$(get origin_cpus) SHOWCASE_ORIGIN_MEMORY=$(get origin_memory)
export SHOWCASE_TANOD_CPUS=$(get tanod_cpus) SHOWCASE_EDGE_CPUS=$(get edge_cpus)
export SHOWCASE_RENDER_DELAY_MS=$(get render_delay_ms) SHOWCASE_CATALOG_UNITS=$(get catalog_units)
export SHOWCASE_TANOD_METRICS_PORT=$(get tanod_metrics_port) SHOWCASE_TANOD_ADMIN_PORT=$(get tanod_admin_port)
export SHOWCASE_PROMETHEUS_PORT=$(get prometheus_port)
IFS=, read -r SHOWCASE_ORIGIN_PORT_1 SHOWCASE_ORIGIN_PORT_2 SHOWCASE_ORIGIN_PORT_3 <<<"$(get origin_ports)"
export SHOWCASE_ORIGIN_PORT_1 SHOWCASE_ORIGIN_PORT_2 SHOWCASE_ORIGIN_PORT_3
# Compose interpolates every service, so it needs a config dir even for `down`.
export SHOWCASE_CONFIG_DIR="$OUT/.idle-config"
mkdir -p "$SHOWCASE_CONFIG_DIR"
: > "$SHOWCASE_CONFIG_DIR/nginx.conf"

OBSERVE=$(get observe)
COMPOSE=(docker compose -p "$PROJECT" -f "$HERE/compose.yaml" --profile tanod)
[ "$OBSERVE" = true ] && COMPOSE+=(--profile observe)

# k6 on the host if installed, otherwise the pinned container image with host
# networking. Paths are mounted at the same location so both read alike.
K6_ENV=(-e K6_NO_USAGE_REPORT=true)
if [ "$OBSERVE" = true ]; then
  K6_ENV+=(-e "K6_PROMETHEUS_RW_SERVER_URL=http://127.0.0.1:$SHOWCASE_PROMETHEUS_PORT/api/v1/write"
           -e "K6_PROMETHEUS_RW_TREND_STATS=p(50),p(95),p(99)")
fi
if [ -n "${SHOWCASE_K6:-}" ]; then
  read -r -a K6 <<<"$SHOWCASE_K6"
elif command -v k6 >/dev/null; then
  K6=(k6)
else
  K6=(docker run --rm -i --network host -u "$(id -u):$(id -g)"
      -v "$HERE/k6:$HERE/k6:ro" -v "$OUT:$OUT" "${K6_ENV[@]}" "$K6_IMAGE")
fi
k6_run() { # k6 run arguments...
  "${K6[@]}" run --no-usage-report "$@"
}
if [ "$OBSERVE" = true ]; then
  export K6_PROMETHEUS_RW_SERVER_URL="http://127.0.0.1:$SHOWCASE_PROMETHEUS_PORT/api/v1/write"
  export K6_PROMETHEUS_RW_TREND_STATS="p(50),p(95),p(99)"
fi

port_free() { ! (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }

APP_SERVICES=(edge tanod next-1 next-2 next-3)
stop_app() {
  "${COMPOSE[@]}" stop --timeout 3 "${APP_SERVICES[@]}" >/dev/null 2>&1 || true
  "${COMPOSE[@]}" rm --force "${APP_SERVICES[@]}" >/dev/null 2>&1 || true
  # Every case must start from new containers; a survivor would carry state
  # (or, in the baseline arm, a running Tanod) into the next case.
  local left
  left=$("${COMPOSE[@]}" ps --all --quiet "${APP_SERVICES[@]}" 2>/dev/null || true)
  [ -z "$left" ] || die "containers survived teardown: $left"
}

SAMPLER_PID=""
cleanup() {
  local status=$?
  [ -n "$SAMPLER_PID" ] && kill "$SAMPLER_PID" 2>/dev/null || true
  if [ "$KEEP" = 1 ]; then
    echo "showcase: --keep: stack left running; stop it with: docker compose -p $PROJECT -f $HERE/compose.yaml --profile tanod --profile observe down"
  else
    "${COMPOSE[@]}" down --remove-orphans --timeout 3 >/dev/null 2>&1 || true
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

wait_http() { # url, expected status regex, seconds, label
  local deadline=$((SECONDS + $3)) code
  while [ $SECONDS -lt $deadline ]; do
    code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 2 "$1" || true)
    [[ "$code" =~ ^($2)$ ]] && return 0
    sleep 0.5
  done
  die "$4 did not become ready at $1 (last status ${code:-none})"
}

log "Preflight (run $RUN_ID -> $OUT)"
for port in "$SHOWCASE_PORT" "$SHOWCASE_ORIGIN_PORT_1" "$SHOWCASE_ORIGIN_PORT_2" "$SHOWCASE_ORIGIN_PORT_3" \
            "$SHOWCASE_TANOD_METRICS_PORT" "$SHOWCASE_TANOD_ADMIN_PORT"; do
  port_free "$port" || die "port $port is in use; stop whatever holds it (another showcase: docker compose -p $PROJECT -f $HERE/compose.yaml down)"
done
"${COMPOSE[@]}" config --quiet || die "compose.yaml does not validate"

if [ "$BUILD" = 1 ]; then
  log "Building the Next.js origin and Tanod images (cached layers are reused)"
  "${COMPOSE[@]}" build next-1 tanod
fi
docker image inspect "$NEXT_IMAGE" >/dev/null 2>&1 || die "missing image $NEXT_IMAGE (drop --no-build)"
docker image inspect "$TANOD_IMAGE" >/dev/null 2>&1 || die "missing image $TANOD_IMAGE (drop --no-build)"
docker image inspect "$NGINX_IMAGE" >/dev/null 2>&1 || docker pull -q "$NGINX_IMAGE" >/dev/null

MODES=$(get modes) WORKLOADS=$(get workloads) ARMS=$(get arms) TRIALS=$(get trials)
IFS=, read -r -a MODE_LIST <<<"$MODES"
IFS=, read -r -a WORKLOAD_LIST <<<"$WORKLOADS"
IFS=, read -r -a ARM_LIST <<<"$ARMS"

# Every configuration this run would use, validated by the real programs.
log "Validating rendered configuration"
for mode in "${MODE_LIST[@]}"; do
  for arm in "${ARM_LIST[@]}"; do
    dir="$OUT/config/$mode-$arm"
    python3 "$SHOWCASE" render --params "$PARAMS" --mode "$mode" --arm "$arm" --out "$dir"
    docker run --rm --entrypoint nginx --add-host next-1:127.0.0.1 --add-host next-2:127.0.0.1 \
      --add-host next-3:127.0.0.1 --add-host tanod:127.0.0.1 -v "$dir/nginx.conf:/etc/nginx/nginx.conf:ro" \
      "$NGINX_IMAGE" -t -q \
      || die "nginx rejected $dir/nginx.conf"
  done
  docker run --rm -v "$OUT/config/$mode-tanod/tanod.yaml:/etc/showcase/tanod.yaml:ro" "$TANOD_IMAGE" \
    check --config /etc/showcase/tanod.yaml >/dev/null || die "tanod rejected $OUT/config/$mode-tanod/tanod.yaml"
  echo "  $mode: nginx (baseline, tanod) and tanod configs valid"
done
"${K6[@]}" inspect "$HERE/k6/overload.js" >/dev/null || die "k6 cannot load overload.js"

python3 "$SHOWCASE" metadata --params "$PARAMS" --out "$OUT/metadata.json" --run-id "$RUN_ID" \
  --image "next=$NEXT_IMAGE" --image "tanod=$TANOD_IMAGE" --image "nginx=$NGINX_IMAGE" --k6 "${K6[@]}"
echo "  metadata.json written"

if [ "$CHECK_ONLY" = 1 ]; then
  log "Check complete: configuration, images and k6 are usable. No load was generated."
  exit 0
fi

stop_app
if [ "$OBSERVE" = true ]; then
  log "Starting Prometheus and Grafana"
  "${COMPOSE[@]}" up -d prometheus grafana >/dev/null
  echo "  Grafana:    http://127.0.0.1:13400/d/tanod-showcase"
  echo "  Prometheus: http://127.0.0.1:$SHOWCASE_PROMETHEUS_PORT"
fi

# ---------------------------------------------------------------------------
# One case
# ---------------------------------------------------------------------------

k6_load() { # case dir, workload, rate, duration, profile, summary file, arm, extra args...
  local dir=$1 workload=$2 rate=$3 duration=$4 profile=$5 summary=$6 arm=$7
  shift 7
  k6_run --quiet "$@" \
    -e "TARGET=http://127.0.0.1:$SHOWCASE_PORT" -e "WORKLOAD=$workload" -e "ROUTE=$ROUTE" \
    -e "PROFILE=$profile" -e "RATE=$rate" -e "DURATION_S=$duration" \
    -e "BASE_RATE=$(get base_rate)" -e "BASE_S=$(get base_s)" -e "RAMP_S=$(get ramp_s)" \
    -e "RECOVERY_S=$(get recovery_s)" -e "HOT_PATHS=$(get hot_paths)" -e "CLIENTS=$(get clients)" \
    -e "PRE_VUS=$(get pre_vus)" -e "MAX_VUS=$(get max_vus)" -e "REQUEST_TIMEOUT=$(get request_timeout_s)s" \
    -e "RUN_ID=$CASE_RUN_ID" -e "ARM=$arm" -e "SUMMARY_PATH=$summary" \
    "$HERE/k6/overload.js"
}

run_case() { # mode, workload, arm, trial
  local mode=$1 workload=$2 arm=$3 trial=$4
  local dir="$OUT/cases/$mode-$workload-$arm-t$trial"
  ROUTE=$(get route --mode "$mode")
  local rate
  rate=$(get rate --mode "$mode")
  # Per trial, not per arm: both arms request the same URL sequence.
  CASE_RUN_ID="t$trial"
  mkdir -p "$dir/logs"
  cp -r "$OUT/config/$mode-$arm" "$dir/config"
  export SHOWCASE_CONFIG_DIR="$dir/config"
  python3 "$SHOWCASE" case-meta --params "$PARAMS" --out "$dir/case-meta.json" \
    --mode "$mode" --workload "$workload" --arm "$arm" --trial "$trial" --run-id "$RUN_ID"

  log "Case $mode/$workload/$arm (trial $trial): $rate req/s for $(get duration_s)s"
  stop_app
  "${COMPOSE[@]}" up -d --wait --wait-timeout 120 next-1 next-2 next-3 >/dev/null \
    || die "origins did not become healthy"
  if [ "$arm" = tanod ]; then
    "${COMPOSE[@]}" up -d --no-deps tanod >/dev/null
    wait_http "http://127.0.0.1:$SHOWCASE_TANOD_ADMIN_PORT/health/ready" 200 60 "Tanod"
  fi
  "${COMPOSE[@]}" up -d --no-deps edge >/dev/null
  wait_http "http://127.0.0.1:$SHOWCASE_PORT/healthz" '200|204' 60 "the NGINX edge"
  for port in "$SHOWCASE_ORIGIN_PORT_1" "$SHOWCASE_ORIGIN_PORT_2" "$SHOWCASE_ORIGIN_PORT_3"; do
    wait_http "http://127.0.0.1:$port/api/fixture-stats?reader=readiness" 200 30 "origin stats on $port"
  done

  echo "  warmup: $(get warmup_rate) req/s for $(get warmup_s)s on URLs the measured phase never uses"
  k6_load "$dir" warmup "$(get warmup_rate)" "$(get warmup_s)" constant "$dir/k6-warmup-summary.json" "$arm" \
    >"$dir/logs/k6-warmup.log" 2>&1 || die "warmup failed; see $dir/logs/k6-warmup.log"
  python3 "$SHOWCASE" drain --params "$PARAMS" --out "$dir/drain-before.json"

  python3 "$SHOWCASE" snapshot --params "$PARAMS" --arm "$arm" --out "$dir/snapshot-before.json" --reset-window
  local containers=() service id
  for service in next-1 next-2 next-3 edge tanod; do
    id=$("${COMPOSE[@]}" ps -q "$service" 2>/dev/null || true)
    [ -n "$id" ] && containers+=(--container "$service=$id")
  done
  rm -f "$dir/.stop-sampler"
  python3 "$SHOWCASE" sample --params "$PARAMS" --arm "$arm" --out "$dir/samples.csv" \
    --stop "$dir/.stop-sampler" "${containers[@]}" &
  SAMPLER_PID=$!

  local k6_out=(--out "csv=$dir/k6-requests.csv.gz")
  [ "$OBSERVE" = true ] && k6_out+=(--out experimental-prometheus-rw)
  k6_load "$dir" "$workload" "$rate" "$(get duration_s)" "$(get profile)" "$dir/k6-summary.json" "$arm" \
    "${k6_out[@]}" 2> >(grep -v 'Request Failed' >"$dir/logs/k6.log") | tee -a "$dir/logs/k6.log"

  # Renders the clients gave up on may still be running at the origin. Keep
  # sampling until they finish, and record how long that took.
  python3 "$SHOWCASE" drain --params "$PARAMS" --out "$dir/drain.json"
  touch "$dir/.stop-sampler"
  wait "$SAMPLER_PID" || echo "  warning: the sampler exited with an error"
  SAMPLER_PID=""
  rm -f "$dir/.stop-sampler"
  python3 "$SHOWCASE" snapshot --params "$PARAMS" --arm "$arm" --out "$dir/snapshot-after.json"

  # Container state catches an origin the OOM killer took during the run.
  local ids=()
  for service in next-1 next-2 next-3 edge tanod; do
    id=$("${COMPOSE[@]}" ps -aq "$service" 2>/dev/null || true)
    [ -n "$id" ] && ids+=("$id")
  done
  docker inspect "${ids[@]}" >"$dir/containers.json" 2>/dev/null || true
  # Timestamped: the origins' render_start/render_end lines are the exact
  # record of their concurrency, and the analysis reads it from here.
  for service in edge tanod next-1 next-2 next-3; do
    "${COMPOSE[@]}" logs --no-color --timestamps "$service" 2>/dev/null | gzip >"$dir/logs/$service.log.gz" || true
  done

  python3 "$SHOWCASE" case --params "$PARAMS" --dir "$dir"
  stop_app
  sleep "$(get cooldown_s)"
}

# ---------------------------------------------------------------------------
# Every case, arms alternating order per trial so neither always goes first
# ---------------------------------------------------------------------------

for trial in $(seq 1 "$TRIALS"); do
  if [ $((trial % 2)) -eq 1 ]; then order=("${ARM_LIST[@]}"); else
    order=(); for ((i = ${#ARM_LIST[@]} - 1; i >= 0; i--)); do order+=("${ARM_LIST[i]}"); done
  fi
  for mode in "${MODE_LIST[@]}"; do
    for workload in "${WORKLOAD_LIST[@]}"; do
      for arm in "${order[@]}"; do
        run_case "$mode" "$workload" "$arm" "$trial"
      done
    done
  done
done

log "Analysing"
analyze_run "$OUT"
log "Done: $OUT"
echo "  results.json  results.csv  summary.md  charts/"
