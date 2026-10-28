#!/usr/bin/env bash
# Performance scenarios: throughput, latency, CPU and memory, recorded as
# machine-readable results so two builds can be compared on one machine.
#
#   bench/perf.sh [scenario ...]
#
# Scenarios (default: all of the short ones):
#
#   hit         cache-hit throughput across client concurrency (Scenario A)
#   sizes       cache-hit throughput across body sizes and reader counts
#   miss        every request a distinct key: fill, admit and evict
#   coalesce    cold-cache bursts of identical requests (Scenario B)
#   mixed       a configurable production traffic mix (Scenario C)
#   overload    a capacity-limited origin, direct and through Tanod (Scenario D)
#   passthrough forwarding overhead alone: Tanod in observe mode, no cache
#   stream      progressive chunks to a herd that includes slow readers (E)
#   pressure    a working set far past the cache budget, with slow readers (F)
#
# This is a measurement tool, not a gate. It asserts only what makes a
# measurement meaningless when false — no errors where none are expected, a
# hit actually being a hit — and leaves throughput to be compared, never held
# to a threshold: see docs/RELEASE-GATES.md on why performance is not gated.
#
# Comparing two builds:
#
#   TANOD_BIN=/path/to/old/tanod PERF_LABEL=baseline bench/perf.sh hit sizes
#   TANOD_BIN=/path/to/new/tanod PERF_LABEL=optimized bench/perf.sh hit sizes
#   python3 bench/perf-compare.py benchmark-results/perf/baseline \
#       benchmark-results/perf/optimized
#
# Knobs (environment):
#
#   TANOD_BIN        Tanod binary (default: target/release/tanod, built here)
#   PERF_LABEL       results subdirectory (default: the commit's short hash)
#   PERF_OUT         results root (default: benchmark-results/perf)
#   PERF_TRIALS      repetitions of every measurement (default 3)
#   PERF_DURATION    measured seconds per run (default 8)
#   PERF_WARMUP      unrecorded seconds before each run (default 2)
#   PERF_CONNECTIONS client concurrency sweep for `hit` (default "1 8 32 128 512")
#   PERF_THREADS     `server.threads` for builds that have the key (unset: omit)
#   PERF_TANOD_CPUS / PERF_LOAD_CPUS / PERF_ORIGIN_CPUS
#                    CPU sets for taskset (default 0-2 / 3-4 / 5 on a machine
#                    with at least six CPUs, otherwise unpinned)
#   PERF_MIX         traffic mix file for `mixed` (default bench/perf-mixed.mix)
#   PERF_APPEND=1    keep earlier results for the label and number this run's
#                    trials after them, so two builds can be interleaved trial
#                    by trial (see bench/perf-ab.sh)
#   CACHE_EVICTION   clock (default) or tinyufo, as for every bench script
BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"

SCENARIOS=${*:-"hit sizes miss coalesce mixed overload passthrough stream pressure"}
TRIALS=${PERF_TRIALS:-3}
APPEND=${PERF_APPEND:-0}
DURATION=${PERF_DURATION:-8}
WARMUP=${PERF_WARMUP:-2}
CONNECTIONS=${PERF_CONNECTIONS:-"1 8 32 128 512"}
LABEL=${PERF_LABEL:-$(git -C "$BENCH_ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)}
[ -n "${CACHE_EVICTION:-}" ] && LABEL="$LABEL-$CACHE_EVICTION"
OUT=${PERF_OUT:-$BENCH_ROOT/benchmark-results/perf}/$LABEL
MIX=${PERF_MIX:-$BENCH_ROOT/bench/perf-mixed.mix}

command -v jq >/dev/null || { echo "bench/perf.sh needs jq" >&2; exit 1; }

BENCH_PROFILE=release
export BENCH_PROFILE
bench_init perf
bench_param label "$LABEL"
bench_param trials "$TRIALS"
bench_param duration_s "$DURATION"
bench_param warmup_s "$WARMUP"

if [ -z "${TANOD_BIN:-}" ]; then
  bench_build
  TANOD_BIN=$(bench_bin tanod)
fi
LOADGEN=${LOADGEN_BIN:-$(bench_bin loadgen)}
ORIGIN_BIN=${ORIGIN_BIN:-$(bench_bin slow-origin)}
for bin in "$TANOD_BIN" "$LOADGEN" "$ORIGIN_BIN"; do
  [ -x "$bin" ] || bench_fail "missing binary $bin (build the workspace with --release)"
done

if [ "$(bench_cpu_count)" -ge 6 ] 2>/dev/null; then
  TANOD_CPUS=${PERF_TANOD_CPUS:-0-2}
  LOAD_CPUS=${PERF_LOAD_CPUS:-3-4}
  ORIGIN_CPUS=${PERF_ORIGIN_CPUS:-5}
else
  TANOD_CPUS=${PERF_TANOD_CPUS:-}
  LOAD_CPUS=${PERF_LOAD_CPUS:-}
  ORIGIN_CPUS=${PERF_ORIGIN_CPUS:-}
fi
# The words that pin a command to a CPU set, or none. Expanded unquoted in
# front of the command, so a spawned process keeps the pid the harness
# recorded: taskset execs the command rather than forking it.
pin() { # cpus
  if [ -n "$1" ] && command -v taskset >/dev/null; then
    printf 'taskset -c %s' "$1"
  fi
}

mkdir -p "$OUT"
# Everything a reader needs to decide whether two result sets are comparable.
{
  printf '{"label":"%s","commit":"%s","tanod_bin":"%s","tanod_sha256":"%s",' \
    "$LABEL" "$(git -C "$BENCH_ROOT" rev-parse HEAD 2>/dev/null)" "$TANOD_BIN" \
    "$(sha256sum "$TANOD_BIN" 2>/dev/null | cut -d' ' -f1)"
  printf '"cpu":"%s","cpus":"%s","governor":"%s","kernel":"%s","rustc":"%s",' \
    "$(sed -n 's/^model name[[:space:]]*: //p' /proc/cpuinfo 2>/dev/null | head -1)" \
    "$(bench_cpu_count)" \
    "$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null || echo unknown)" \
    "$(uname -sr)" "$(rustc --version 2>/dev/null)"
  printf '"tanod_cpus":"%s","load_cpus":"%s","origin_cpus":"%s","threads":"%s",' \
    "$TANOD_CPUS" "$LOAD_CPUS" "$ORIGIN_CPUS" "${PERF_THREADS:-default}"
  printf '"eviction":"%s","trials":%s,"duration_s":%s,"warmup_s":%s,"loadavg":"%s","started_at":"%s"}\n' \
    "${CACHE_EVICTION:-clock}" "$TRIALS" "$DURATION" "$WARMUP" \
    "$(cut -d' ' -f1-3 /proc/loadavg 2>/dev/null)" "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
} > "$OUT/environment.json"

# Trial numbers for this invocation: 1..TRIALS, or continuing after the trials
# already recorded for this label when appending.
TRIAL_BASE=0
trials() { seq $((TRIAL_BASE + 1)) $((TRIAL_BASE + TRIALS)); }

server_extra() {
  if [ -n "${PERF_THREADS:-}" ]; then
    printf '  threads: %s' "$PERF_THREADS"
  fi
}

ORIGIN_PORT=""
LISTEN_PORT=""
METRICS_PORT=""

start_origin() { # render_ms [slots]
  ORIGIN_PORT=$(bench_free_port)
  bench_stop origin
  # shellcheck disable=SC2046
  bench_spawn origin env SLOW_ORIGIN_SLOTS="${2:-}" $(pin "$ORIGIN_CPUS") \
    "$ORIGIN_BIN" "$ORIGIN_PORT" "$1"
  bench_wait_port 127.0.0.1 "$ORIGIN_PORT" "slow-origin"
}

start_tanod() { # template, then KEY=VALUE overrides
  local template=$1; shift
  LISTEN_PORT=$(bench_free_port)
  METRICS_PORT=$(bench_free_port)
  local config="$BENCH_DIR/$(basename "$template" .tpl)"
  bench_render_config "$template" "$config" \
    "LISTEN=$LISTEN_PORT" "ORIGIN=$ORIGIN_PORT" "METRICS=$METRICS_PORT" \
    "SERVER_EXTRA=$(server_extra)" "$@"
  bench_stop tanod
  # shellcheck disable=SC2046
  bench_spawn tanod $(pin "$TANOD_CPUS") "$TANOD_BIN" run --config "$config"
  bench_wait_port 127.0.0.1 "$LISTEN_PORT" "tanod"
  bench_wait_port 127.0.0.1 "$METRICS_PORT" "tanod metrics"
}

stop_all() {
  bench_stop tanod
  bench_stop origin
}

# Run the load generator and append its result, tagged, to a scenario file.
record() { # scenario, trial, extra-json, loadgen args...
  local scenario=$1 trial=$2 extra=$3; shift 3
  local raw
  # shellcheck disable=SC2046
  raw=$($(pin "$LOAD_CPUS") "$LOADGEN" "$@") || bench_fail "loadgen failed in $scenario"
  printf '%s\n' "$raw" \
    | jq -c --arg s "$scenario" --argjson t "$trial" --argjson x "$extra" \
        '. + {scenario: $s, trial: $t} + $x' >> "$OUT/$scenario.jsonl"
  printf '%s\n' "$raw" | jq -r --arg s "$scenario" \
    '"  \($s) \(.label): \(.rps|floor) rps, p50 \(.latency_us.p50)us p99 \(.latency_us.p99)us, errors \(.errors), non-2xx \(.status["4xx"] + .status["5xx"]), tanod cpu \(.processes[0].cpu_cores // "-")"'
}

origin_stat() { bench_origin_stat "$ORIGIN_PORT" "$1"; }

# --------------------------------------------------------------- scenarios

scenario_hit() {
  start_origin 0
  start_tanod "$BENCH_ROOT/bench/perf-cache.yaml.tpl" \
    "ORIGIN_MAX=256" "CACHE_MEMORY=256MiB" "MAX_BODY=8MiB" "TTL=1h"
  local status
  status=$(curl -s -o /dev/null -D - "http://127.0.0.1:$LISTEN_PORT/hit/1/size/1024" \
    | tr -d '\r' | sed -n 's/^[Xx]-[Tt]anod: //p')
  status=$(curl -s -o /dev/null -D - "http://127.0.0.1:$LISTEN_PORT/hit/1/size/1024" \
    | tr -d '\r' | sed -n 's/^[Xx]-[Tt]anod: //p')
  [ "$status" = "HIT" ] || bench_fail "hit scenario: second request was $status, not HIT"
  for trial in $(trials); do
    for c in $CONNECTIONS; do
      record hit "$trial" "{\"connections\":$c,\"body\":1024}" run \
        --addr "127.0.0.1:$LISTEN_PORT" --path "/hit/{n:64}/size/1024" \
        --connections "$c" --threads 2 --duration "$DURATION" --warmup "$WARMUP" \
        --watch-pid "$(bench_pid tanod)" --label "hit-1KiB-c$c"
    done
  done
  local renders
  renders=$(origin_stat total)
  bench_result hit_origin_renders "$renders"
  stop_all
}

scenario_sizes() {
  start_origin 0
  start_tanod "$BENCH_ROOT/bench/perf-cache.yaml.tpl" \
    "ORIGIN_MAX=256" "CACHE_MEMORY=512MiB" "MAX_BODY=8MiB" "TTL=1h"
  for trial in $(trials); do
    for size in 1024 16384 65536 1048576 4194304; do
      for c in 1 10 100 500; do
        record sizes "$trial" "{\"connections\":$c,\"body\":$size}" run \
          --addr "127.0.0.1:$LISTEN_PORT" --path "/sizes/{n:16}/size/$size" \
          --connections "$c" --threads 2 --duration "$(( (DURATION + 1) / 2 ))" \
          --warmup 1 --watch-pid "$(bench_pid tanod)" --label "hit-$size-c$c"
      done
    done
  done
  stop_all
}

scenario_miss() {
  # Every request is a new key, so every request renders, registers a fill,
  # streams it, admits it, and — with a 32MiB budget and 16KiB bodies —
  # evicts something to make room.
  start_origin 0
  start_tanod "$BENCH_ROOT/bench/perf-cache.yaml.tpl" \
    "ORIGIN_MAX=512" "CACHE_MEMORY=32MiB" "MAX_BODY=8MiB" "TTL=1h"
  for trial in $(trials); do
    for c in 8 64; do
      record miss "$trial" "{\"connections\":$c,\"body\":16384}" run \
        --addr "127.0.0.1:$LISTEN_PORT" --path "/miss/size/16384?u={u}" \
        --connections "$c" --threads 2 --duration "$DURATION" --warmup "$WARMUP" \
        --watch-pid "$(bench_pid tanod)" --label "miss-16KiB-c$c"
    done
  done
  stop_all
}

scenario_coalesce() {
  # A slow render, so the whole burst arrives while the leader is rendering.
  start_origin 300
  start_tanod "$BENCH_ROOT/bench/perf-cache.yaml.tpl" \
    "ORIGIN_MAX=64" "CACHE_MEMORY=256MiB" "MAX_BODY=8MiB" "TTL=1h"
  local n renders peak
  for trial in $(trials); do
    for n in 10 100 500 1000; do
      bench_origin_reset "$ORIGIN_PORT"
      record coalesce "$trial" "{\"connections\":$n,\"body\":65536}" burst \
        --addr "127.0.0.1:$LISTEN_PORT" --path "/coalesce/$trial-$n/size/65536" \
        --connections "$n" --threads 2 --timeout 30 \
        --watch-pid "$(bench_pid tanod)" --label "burst-$n"
      renders=$(origin_stat total)
      peak=$(origin_stat peak)
      printf '{"scenario":"coalesce-origin","trial":%s,"connections":%s,"origin_renders":%s,"origin_peak":%s}\n' \
        "$trial" "$n" "${renders:-null}" "${peak:-null}" >> "$OUT/coalesce.jsonl"
      echo "    origin renders for $n identical requests: $renders"
    done
  done
  stop_all
}

scenario_stream() {
  # Eight chunks 150ms apart; a tenth of the herd reads at 16KiB/s.
  start_origin 150
  start_tanod "$BENCH_ROOT/bench/perf-cache.yaml.tpl" \
    "ORIGIN_MAX=64" "CACHE_MEMORY=256MiB" "MAX_BODY=8MiB" "TTL=1h"
  local n renders
  for trial in $(trials); do
    for n in 10 100 500; do
      bench_origin_reset "$ORIGIN_PORT"
      record stream "$trial" "{\"connections\":$n,\"slow_readers\":$((n / 10))}" burst \
        --addr "127.0.0.1:$LISTEN_PORT" --path "/stream/8?t=$trial-$n" \
        --connections "$n" --threads 2 --timeout 60 \
        --slow-readers "$((n / 10))" --slow-bps 16384 \
        --watch-pid "$(bench_pid tanod)" --label "stream-$n"
      renders=$(origin_stat total)
      printf '{"scenario":"stream-origin","trial":%s,"connections":%s,"origin_renders":%s}\n' \
        "$trial" "$n" "${renders:-null}" >> "$OUT/stream.jsonl"
    done
  done
  stop_all
}

mixed_tanod() { # mode cache_enabled origin_max queue_max queue_timeout
  start_tanod "$BENCH_ROOT/bench/perf-mixed.yaml.tpl" \
    "MODE=$1" "CACHE_ENABLED=$2" "ORIGIN_MAX=$3" "QUEUE_MAX=$4" "QUEUE_TIMEOUT=$5" \
    "CACHE_MEMORY=256MiB" "TTL=30s"
}

scenario_mixed() {
  # A 10ms render on an origin that can do 64 at once: 6400 renders/s, enough
  # headroom that this measures Tanod rather than queueing at the origin.
  start_origin 10 64
  mixed_tanod protect true 64 512 5s
  for trial in $(trials); do
    bench_origin_reset "$ORIGIN_PORT"
    record mixed "$trial" "{\"connections\":128}" run \
      --addr "127.0.0.1:$LISTEN_PORT" --mix "$MIX" \
      --connections 128 --threads 2 --duration "$DURATION" --warmup "$WARMUP" \
      --watch-pid "$(bench_pid tanod)" --label "mixed-c128"
    printf '{"scenario":"mixed-origin","trial":%s,"origin_renders":%s,"origin_peak":%s}\n' \
      "$trial" "$(origin_stat total)" "$(origin_stat peak)" >> "$OUT/mixed.jsonl"
  done
  stop_all
}

scenario_overload() {
  # An origin that renders 8 pages at a time in 50ms each: 160 renders/s.
  # Every request is uncached dynamic work, so nothing here is a cache win;
  # the comparison is what saturation does to latency and errors.
  local c
  start_origin 50 8
  for trial in $(trials); do
    for c in 8 32 128 512; do
      bench_origin_reset "$ORIGIN_PORT"
      record overload "$trial" "{\"connections\":$c,\"path\":\"direct\"}" run \
        --addr "127.0.0.1:$ORIGIN_PORT" --path "/api/render/size/16384?q={u}" \
        --connections "$c" --threads 2 --duration "$DURATION" --warmup "$WARMUP" \
        --watch-pid "$(bench_pid origin)" --timeout 30 --label "direct-c$c"
      printf '{"scenario":"overload-origin","trial":%s,"connections":%s,"path":"direct","origin_peak":%s}\n' \
        "$trial" "$c" "$(origin_stat peak)" >> "$OUT/overload.jsonl"
    done
  done
  mixed_tanod protect true 8 64 1s
  for trial in $(trials); do
    for c in 8 32 128 512; do
      bench_origin_reset "$ORIGIN_PORT"
      record overload "$trial" "{\"connections\":$c,\"path\":\"tanod\"}" run \
        --addr "127.0.0.1:$LISTEN_PORT" --path "/api/render/size/16384?q={u}" \
        --connections "$c" --threads 2 --duration "$DURATION" --warmup "$WARMUP" \
        --watch-pid "$(bench_pid tanod)" --watch-pid "$(bench_pid origin)" --timeout 30 \
        --label "tanod-c$c"
      printf '{"scenario":"overload-origin","trial":%s,"connections":%s,"path":"tanod","origin_peak":%s}\n' \
        "$trial" "$c" "$(origin_stat peak)" >> "$OUT/overload.jsonl"
    done
  done
  stop_all
}

scenario_passthrough() {
  # Forwarding alone: no cache, no admission. Against a 0ms origin, the
  # difference from `direct` is what one proxy hop costs.
  local c
  start_origin 0
  for trial in $(trials); do
    for c in 8 64; do
      record passthrough "$trial" "{\"connections\":$c,\"path\":\"direct\"}" run \
        --addr "127.0.0.1:$ORIGIN_PORT" --path "/api/pass/size/4096?q={u}" \
        --connections "$c" --threads 2 --duration "$DURATION" --warmup "$WARMUP" \
        --watch-pid "$(bench_pid origin)" --label "direct-c$c"
    done
  done
  mixed_tanod observe false 1024 0 1s
  for trial in $(trials); do
    for c in 8 64; do
      record passthrough "$trial" "{\"connections\":$c,\"path\":\"tanod\"}" run \
        --addr "127.0.0.1:$LISTEN_PORT" --path "/api/pass/size/4096?q={u}" \
        --connections "$c" --threads 2 --duration "$DURATION" --warmup "$WARMUP" \
        --watch-pid "$(bench_pid tanod)" --watch-pid "$(bench_pid origin)" \
        --label "passthrough-c$c"
    done
  done
  stop_all
}

scenario_pressure() {
  # 400 keys of 256KiB (100MiB) against a 16MiB budget: constant eviction,
  # plus a burst in which a tenth of the readers stall.
  start_origin 5
  start_tanod "$BENCH_ROOT/bench/perf-cache.yaml.tpl" \
    "ORIGIN_MAX=128" "CACHE_MEMORY=16MiB" "MAX_BODY=4MiB" "TTL=1h"
  for trial in $(trials); do
    record pressure "$trial" "{\"connections\":64}" run \
      --addr "127.0.0.1:$LISTEN_PORT" --path "/pressure/{n:400}/size/262144" \
      --connections 64 --threads 2 --duration "$DURATION" --warmup "$WARMUP" \
      --watch-pid "$(bench_pid tanod)" --label "pressure-c64"
    record pressure "$trial" "{\"connections\":200,\"slow_readers\":20}" burst \
      --addr "127.0.0.1:$LISTEN_PORT" --path "/pressure-burst/$trial/size/1048576" \
      --connections 200 --threads 2 --timeout 120 --slow-readers 20 --slow-bps 262144 \
      --watch-pid "$(bench_pid tanod)" --label "pressure-burst-200"
  done
  printf '{"scenario":"pressure-final","rss_kb":%s,"cache_bytes":%s}\n' \
    "$(bench_rss_kb "$(bench_pid tanod)")" \
    "$(bench_metric "$METRICS_PORT" tanod_cache_bytes)" >> "$OUT/pressure.jsonl"
  bench_assert_no_panics tanod
  stop_all
}

for scenario in $SCENARIOS; do
  echo
  echo "== $scenario ($LABEL, $TRIALS trials)"
  TRIAL_BASE=0
  if [ "$APPEND" = "1" ] && [ -s "$OUT/$scenario.jsonl" ]; then
    TRIAL_BASE=$(jq -s 'map(.trial // 0) | max' "$OUT/$scenario.jsonl")
  else
    rm -f "$OUT/$scenario.jsonl"
  fi
  case "$scenario" in
    hit|sizes|miss|coalesce|mixed|overload|passthrough|stream|pressure) "scenario_$scenario" ;;
    *) bench_fail "unknown scenario: $scenario" ;;
  esac
done

bench_print_params
bench_pass "results in $OUT"
