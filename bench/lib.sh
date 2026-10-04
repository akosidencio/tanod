# shellcheck shell=bash
# Shared harness for the bench/ scripts. Source it, never execute it.
#
# The scripts in this directory are the evidence behind the claims in the
# README, so the harness they share has to be trustworthy before the numbers
# are. Three habits it exists to remove:
#
#   * `pkill -f target/debug/tanod` kills every Tanod on the machine,
#     including another developer's, another benchmark's, and — on a laptop
#     running the fixture — the one under test in a different terminal. Every
#     process here is started through `bench_spawn`, which records the exact
#     pid, and torn down by that pid alone.
#   * Fixed ports 3000/8080/9090 collide with whatever is already running and
#     turn a benchmark into a measurement of somebody else's server. Ports are
#     allocated from the kernel's free range per run.
#   * `sleep 2 && hope` reports a startup race as a benchmark failure. Readiness
#     is polled.
#
# Every script also records the parameters it ran with next to its results, so
# a number can be compared against another machine instead of being taken as a
# universal baseline.

# Bash 3.2 (the macOS system bash) is the floor: no associative arrays, no
# ${var,,}, no `mapfile`.

set -uo pipefail

BENCH_NAME=""
BENCH_DIR=""
BENCH_PIDS=""
BENCH_STATUS="incomplete"
BENCH_FAIL_REASON=""
BENCH_PARAM_KEYS=""
BENCH_RESULT_KEYS=""

bench_cpu_count() {
  { command -v nproc >/dev/null && nproc; } \
    || sysctl -n hw.ncpu 2>/dev/null \
    || echo unknown
}

# ---------------------------------------------------------------- lifecycle

bench_init() {
  BENCH_NAME=$1
  BENCH_DIR=$(mktemp -d "${TMPDIR:-/tmp}/tanod-bench-XXXXXX")
  mkdir -p "$BENCH_DIR/pids" "$BENCH_DIR/logs"
  BENCH_STARTED_AT=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  trap bench_cleanup EXIT
  trap 'bench_fail "interrupted"' INT TERM

  bench_param os "$(uname -s)"
  bench_param arch "$(uname -m)"
  bench_param cpus "$(bench_cpu_count)"
  bench_param rustc "$(rustc --version 2>/dev/null | awk '{print $2}')"
  bench_param profile "${BENCH_PROFILE:-debug}"
  bench_param commit "$(git -C "$BENCH_ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
}

bench_cleanup() {
  local code=$?
  bench_stop_all
  bench_report_write
  if [ -n "${BENCH_KEEP_DIR:-}" ]; then
    echo "  (working directory kept at $BENCH_DIR)"
  else
    rm -rf "$BENCH_DIR"
  fi
  return $code
}

# Build the workspace once, from the repository root, before anything is
# spawned. A benchmark that races the compiler measures the compiler.
# `BENCH_FEATURES` is for the scripts that need an optional feature compiled in
# — `tls` today. It is a parameter rather than a default because the TLS stack
# is a two-minute compile that every other benchmark would pay for.
bench_build() {
  # Bash 3.2 (still shipped by macOS) treats expansion of an empty local array
  # as an unbound variable under `set -u`. Keep the no-option path scalar so
  # the same benchmark harness works on both macOS and Linux.
  if [ "${BENCH_PROFILE:-debug}" = "release" ]; then
    if [ -n "${BENCH_FEATURES:-}" ]; then
      cargo build --workspace --release --features "$BENCH_FEATURES" -q \
        || bench_fail "workspace build failed"
    else
      cargo build --workspace --release -q || bench_fail "workspace build failed"
    fi
  elif [ -n "${BENCH_FEATURES:-}" ]; then
    cargo build --workspace --features "$BENCH_FEATURES" -q \
      || bench_fail "workspace build failed"
  else
    cargo build --workspace -q || bench_fail "workspace build failed"
  fi
}

bench_bin() {
  echo "$BENCH_ROOT/target/${BENCH_PROFILE:-debug}/$1"
}

# ------------------------------------------------------------------- ports

# Is something listening on this port right now?
bench_port_open() {
  (exec 3<>"/dev/tcp/$1/$2") >/dev/null 2>&1
}

# Hand out a port nothing is listening on, and never the same one twice within
# one run. Racy in principle against the rest of the machine; in practice a
# random draw from thousands of unused ports beats a hardcoded 8080 by a wide
# margin.
#
# The band is deliberately *below* the kernel's ephemeral range, and that is
# the whole point rather than a detail. A port inside that range can be handed
# to any outbound connection as its source port in the window between the
# probe below and the fixture's `bind` — and `SO_REUSEADDR`, which std sets on
# every listener, rescues a bind against a `TIME_WAIT` socket but not against a
# live one. This suite opens thousands of loopback connections, including one
# per probe here, so the window is not theoretical: a draw from the old
# 20000-39999 band landed on 34798 while Linux was using its default
# 32768-60999, `slow-origin` exited `AddrInUse`, and the run then spent 30s
# waiting for a port that was never going to open.
#
# The ledger of already-issued ports lives in a file, not a variable: callers
# use this as `PORT=$(bench_free_port)`, and a command substitution runs in a
# subshell whose variable assignments are discarded — so a variable would hand
# out the same port twice.
bench_free_port() {
  local port ledger="$BENCH_DIR/ports" lo=20000 hi=32767 eph_lo eph_hi span
  touch "$ledger"
  if [ -r /proc/sys/net/ipv4/ip_local_port_range ]; then
    read -r eph_lo eph_hi < /proc/sys/net/ipv4/ip_local_port_range
    case "${eph_lo:-x}" in *[!0-9]*) eph_lo="" ;; esac
    case "${eph_hi:-x}" in *[!0-9]*) eph_lo="" ;; esac
    if [ -n "$eph_lo" ]; then
      if [ "$eph_lo" -gt 21024 ]; then
        hi=$((eph_lo - 1))
      elif [ "$eph_hi" -lt 64511 ]; then
        lo=$((eph_hi + 1)); hi=65535
      else
        bench_fail "no port band outside the kernel ephemeral range $eph_lo-$eph_hi"
      fi
    fi
  fi
  span=$((hi - lo + 1))
  for _ in $(seq 1 200); do
    port=$(( lo + RANDOM % span ))
    grep -qx "$port" "$ledger" && continue
    if ! bench_port_open 127.0.0.1 "$port"; then
      echo "$port" >> "$ledger"
      printf '%s\n' "$port"
      return 0
    fi
  done
  bench_fail "no free TCP port found after 200 attempts"
}

# --------------------------------------------------------------- processes

# Start a process, remember its exact pid, and send its output to a log this
# run owns. Nothing else in the harness may start a long-lived process.
bench_spawn() {
  local name=$1; shift
  "$@" >"$BENCH_DIR/logs/$name.log" 2>&1 &
  local pid=$!
  disown 2>/dev/null || true
  echo "$pid" > "$BENCH_DIR/pids/$name"
  BENCH_PIDS="$BENCH_PIDS $pid"
}

bench_pid() {
  cat "$BENCH_DIR/pids/$1" 2>/dev/null
}

bench_log() {
  echo "$BENCH_DIR/logs/$1.log"
}

bench_alive() {
  kill -0 "$1" 2>/dev/null
}

# Stop one named process politely, then insist.
bench_stop() {
  local pid
  pid=$(bench_pid "$1")
  [ -n "$pid" ] || return 0
  kill -TERM "$pid" 2>/dev/null || true
  for _ in $(seq 1 50); do
    bench_alive "$pid" || { rm -f "$BENCH_DIR/pids/$1"; return 0; }
    sleep 0.1
  done
  kill -KILL "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
  rm -f "$BENCH_DIR/pids/$1"
}

bench_stop_all() {
  local file
  [ -d "${BENCH_DIR:-/nonexistent}/pids" ] || return 0
  for file in "$BENCH_DIR"/pids/*; do
    [ -e "$file" ] || continue
    bench_stop "$(basename "$file")"
  done
}

# --------------------------------------------------------------- readiness

# What every process this run started had to say, printed when a wait gives
# up. "The port never opened" is a symptom; the cause is in the log of the
# process that was supposed to open it, and reporting only the symptom once
# cost a CI run 30s and an afternoon — the origin had exited `AddrInUse`
# immediately and said so, into a log nobody printed.
bench_dump_logs() {
  local log name
  [ -d "${BENCH_DIR:-/nonexistent}/logs" ] || return 0
  for log in "$BENCH_DIR"/logs/*.log; do
    [ -f "$log" ] || continue
    [ -s "$log" ] || continue
    name=$(basename "$log" .log)
    echo "  --- $name, last 15 lines ---" >&2
    tail -n 15 "$log" | sed 's/^/    /' >&2
  done
}

bench_wait_port() {
  local host=$1 port=$2 label=${3:-"$1:$2"}
  for _ in $(seq 1 300); do
    bench_port_open "$host" "$port" && return 0
    sleep 0.1
  done
  bench_dump_logs
  bench_fail "$label never accepted a connection on $host:$port"
}

bench_wait_http() {
  local url=$1 label=${2:-$1}
  for _ in $(seq 1 300); do
    if curl -fsS -o /dev/null --max-time 2 "$url" 2>/dev/null; then return 0; fi
    sleep 0.1
  done
  bench_dump_logs
  bench_fail "$label never answered $url"
}

# Wait for a line to appear in a spawned process's log. Used where the
# observable event is a log line rather than a socket — a config reload, for
# example, has no port of its own to poll.
bench_wait_log() {
  local name=$1 pattern=$2
  for _ in $(seq 1 100); do
    grep -q "$pattern" "$(bench_log "$name")" 2>/dev/null && return 0
    sleep 0.1
  done
  return 1
}

# ----------------------------------------------------------------- origins

# Start bench/slow-origin and wait until it serves. Returns the port on stdout
# via the variable the caller names, because bash 3.2 has no nameref.
bench_start_origin() { # name, render_ms -> echoes port
  local name=$1 render_ms=$2 port
  port=$(bench_free_port)
  bench_spawn "$name" "$(bench_bin slow-origin)" "$port" "$render_ms"
  bench_wait_port 127.0.0.1 "$port" "slow-origin"
  echo "$port"
}

# The origin is the witness. Ask it directly rather than parsing headers off a
# response the proxy may have served from its own cache, or counting lines in
# the proxy's log — both of which measure the component under test.
bench_origin_stat() { # port, field
  curl -fsS --max-time 5 "http://127.0.0.1:$1/__stats" 2>/dev/null \
    | sed -n "s/.*\"$2\":\([0-9]*\).*/\1/p"
}

bench_origin_reset() {
  curl -fsS -o /dev/null --max-time 5 "http://127.0.0.1:$1/__reset" 2>/dev/null \
    || bench_fail "slow-origin on port $1 did not answer /__reset"
}

# ------------------------------------------------------------------ config

# Render a bench config template, substituting the ports this run allocated.
# The templates keep their placeholders in the repository so that no benchmark
# can quietly depend on a port that happened to be free on one laptop.
bench_render_config() { # template, out, then KEY=VALUE...
  local template=$1 out=$2; shift 2
  local script="" pair
  for pair in "$@"; do
    script="$script s|@${pair%%=*}@|${pair#*=}|g;"
  done
  sed "$script" "$template" > "$out" || bench_fail "could not render $template"
  if grep -q '@[A-Z_]*@' "$out"; then
    bench_fail "unsubstituted placeholder left in $out: $(grep -o '@[A-Z_]*@' "$out" | sort -u | tr '\n' ' ')"
  fi
}

bench_start_tanod() { # name, config, listen_port [, metrics_port]
  local name=$1 config=$2 listen=$3 metrics=${4:-}
  bench_spawn "$name" "$(bench_bin tanod)" run --config "$config"
  bench_wait_port 127.0.0.1 "$listen" "tanod"
  [ -n "$metrics" ] && bench_wait_port 127.0.0.1 "$metrics" "tanod metrics"
  return 0
}

# ------------------------------------------------------------- observation

# Resident set size of a pid, in KiB. `ps -o rss=` reports KiB on both Linux
# and macOS, which is the only reason this is one line rather than two
# platform branches.
#
# RSS is the honest number for "did this process stay inside its budget": it
# counts what is actually resident, which is what an OOM killer and a container
# limit both look at. Heap-allocator statistics would flatter a leak that the
# allocator has not returned to the OS.
bench_rss_kb() { # pid
  ps -o rss= -p "$1" 2>/dev/null | tr -d ' '
}

# One Prometheus metric value, by exact series name. The `^` anchor matters:
# without it `tanod_cache_bytes` also matches nothing useful but
# `tanod_spool_bytes` would match a HELP line.
bench_metric() { # port, series
  curl -s --max-time 5 "http://127.0.0.1:$1/metrics" \
    | sed -n "s/^$2 \([0-9.e+-]*\)$/\1/p" | head -1
}

# Did the process log anything that means it fell over? A soak that ends with
# the proxy alive but its log full of panics is not a passing soak.
bench_assert_no_panics() { # name
  local log
  log=$(bench_log "$1")
  if grep -qE "panicked at|attempt to (add|subtract|multiply) with overflow" "$log"; then
    bench_fail "$1 panicked: $(grep -m1 -E "panicked at|overflow" "$log")"
  fi
}

# ------------------------------------------------------- parameters/results

bench_param() {
  BENCH_PARAM_KEYS="$BENCH_PARAM_KEYS $1"
  eval "BENCH_PARAM_$1=\$2"
}

bench_result() {
  BENCH_RESULT_KEYS="$BENCH_RESULT_KEYS $1"
  eval "BENCH_RESULT_$1=\$2"
}

bench_get() { eval "printf '%s' \"\${$1-}\""; }

# The parameter block is printed with the result, not buried in the script, so
# a pasted benchmark output says what produced it.
bench_print_params() {
  local key first=1
  printf 'parameters: '
  for key in $BENCH_PARAM_KEYS; do
    [ $first = 1 ] || printf ', '
    printf '%s=%s' "$key" "$(bench_get "BENCH_PARAM_$key")"
    first=0
  done
  printf '\n'
}

# One JSON object per run. `BENCH_REPORT_DIR` is set by CI so results can be
# published as an artifact alongside the parameters that produced them.
bench_report_write() {
  [ -n "${BENCH_REPORT_DIR:-}" ] || return 0
  [ -n "$BENCH_NAME" ] || return 0
  mkdir -p "$BENCH_REPORT_DIR"
  {
    printf '{"benchmark":"%s","started_at":"%s","status":"%s"' \
      "$BENCH_NAME" "${BENCH_STARTED_AT:-}" "$BENCH_STATUS"
    [ -n "$BENCH_FAIL_REASON" ] && printf ',"reason":"%s"' "$BENCH_FAIL_REASON"
    printf ',"parameters":{'
    local key first=1
    for key in $BENCH_PARAM_KEYS; do
      [ $first = 1 ] || printf ','
      printf '"%s":"%s"' "$key" "$(bench_get "BENCH_PARAM_$key")"
      first=0
    done
    printf '},"results":{'
    first=1
    for key in $BENCH_RESULT_KEYS; do
      [ $first = 1 ] || printf ','
      printf '"%s":"%s"' "$key" "$(bench_get "BENCH_RESULT_$key")"
      first=0
    done
    printf '}}\n'
  } > "$BENCH_REPORT_DIR/$BENCH_NAME.json"
}

# --------------------------------------------------------------- assertions

# Every check is machine-evaluated and every failure names both sides. A
# benchmark that prints numbers for a human to eyeball is not evidence.

bench_fail() {
  BENCH_STATUS="fail"
  BENCH_FAIL_REASON="$*"
  echo "FAIL: $*" >&2
  exit 1
}

bench_pass() {
  BENCH_STATUS="pass"
  echo "PASS: $*"
}

bench_is_int() { case "${1:-}" in ''|*[!0-9]*) return 1 ;; *) return 0 ;; esac; }

bench_assert_int() { # value, label
  bench_is_int "${1:-}" || bench_fail "$2 was not a number: '${1:-<empty>}'"
}

bench_assert_eq() { # actual, expected, label
  bench_assert_int "$1" "$3"
  [ "$1" -eq "$2" ] || bench_fail "$3: expected $2, observed $1"
}

bench_assert_le() { # actual, bound, label
  bench_assert_int "$1" "$3"
  [ "$1" -le "$2" ] || bench_fail "$3: expected at most $2, observed $1"
}

bench_assert_gt() { # actual, bound, label
  bench_assert_int "$1" "$3"
  [ "$1" -gt "$2" ] || bench_fail "$3: expected more than $2, observed $1"
}

# Compare floating-point seconds without depending on bc.
bench_lt_float() { awk -v a="$1" -v b="$2" 'BEGIN { exit !(a < b) }'; }

bench_median() { sort -n | awk '{ v[NR]=$1 } END { print (NR ? v[int(NR/2)+1] : "") }'; }
bench_max() { sort -n | tail -1; }
