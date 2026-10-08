#!/usr/bin/env bash
# Run every local benchmark and report which ones held.
#
# Each script asserts its own claim and exits non-zero when it fails, so this
# is a gate, not a summary. Set BENCH_REPORT_DIR to collect one JSON file per
# benchmark, each carrying the parameters that produced its numbers — CI does
# this and publishes the directory as an artifact.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1

export BENCH_REPORT_DIR=${BENCH_REPORT_DIR:-}
FAILED=""
PASSED=""

# Is a Docker daemon actually reachable?
#
# Not `docker info` on its own: with Docker Desktop installed but not running,
# that command blocks rather than failing, and the whole gate stalls on it with
# no output. A bounded probe turns "the daemon is down" into the skip it should
# always have been.
docker_available() {
  # Backgrounded directly rather than inside `( … ) &`: with a subshell, `$!`
  # is the subshell and killing it leaves the real `docker` process running and
  # still holding the terminal. `$!` has to be the process being bounded.
  docker info >/dev/null 2>&1 &
  local probe=$! waited=0
  while kill -0 "$probe" 2>/dev/null; do
    waited=$((waited + 1))
    if [ "$waited" -gt 50 ]; then
      kill -KILL "$probe" 2>/dev/null
      wait "$probe" 2>/dev/null
      return 1
    fi
    sleep 0.2
  done
  wait "$probe"
}

run() { # script, args...
  local script=$1; shift
  echo
  echo "=================================================================="
  echo "  $script $*"
  echo "=================================================================="
  if "./bench/$script" "$@"; then
    PASSED="$PASSED $script"
  else
    FAILED="$FAILED $script"
  fi
}

# Concurrency is kept modest so a two-core CI runner still produces genuine
# overlap. The headline numbers in the README come from the larger parameters
# printed in each script's own parameter block.
run demo.sh "${DEMO_CONCURRENCY:-40}" "${DEMO_RENDER_MS:-800}"
run coalesce.sh "${COALESCE_CONCURRENCY:-40}" "${COALESCE_RENDER_MS:-800}"
run stream.sh "${STREAM_CONCURRENCY:-20}" 5 400
run safety.sh "${SAFETY_CONCURRENCY:-30}"
run reload.sh
run slowclient.sh

# Phase 1: protocol coverage, the security properties around forwarded
# metadata, and the response spool.
run protocol.sh
run forwarded.sh
run http2.sh
run spool.sh "${SPOOL_MIB:-8}" "${SPOOL_RATE:-32k}"
run adversarial.sh "${ADVERSARIAL_SECONDS:-20}"

# Phase 2: operability. The readiness and status surface, restarting without
# dropping requests, and the three failure classes that only appear with time
# or with something breaking underneath.
#
# The parameters here are CI-sized. The release gate runs the same scripts far
# longer — an hour of soak, forty rounds of memory pressure — because a leak of
# a few kilobytes per thousand requests is invisible in a minute. See
# docs/RELEASE-GATES.md.
run admin.sh
run upgrade.sh "${UPGRADE_SECONDS:-6}"
run supervise.sh "${SUPERVISE_RENDER_MS:-2500}"
run tracing.sh
run soak.sh "${SOAK_SECONDS:-45}" "${SOAK_WORKERS:-10}"
run memory.sh "${MEMORY_ROUNDS:-6}" "${MEMORY_SLOW_READERS:-12}"
run chaos.sh "${CHAOS_ROUNDS:-2}"

# Phase 3: origin resilience. Both scripts turn a backend into the failure the
# feature exists for — one that passes its health check and fails its renders,
# and one that is simply gone — and assert on what the *origins* counted rather
# than on what the proxy says about itself.
run breaker.sh "${BREAKER_REQUESTS:-40}" "${BREAKER_RENDER_MS:-20}"
run retry.sh "${RETRY_REQUESTS:-40}" "${RETRY_RENDER_MS:-20}"

# Phase 4: the cache lifecycle. Tags and the purge API, including the half
# that matters most — that an unauthorised or misspelled purge changes nothing.
run purge.sh "${PURGE_RENDER_MS:-20}"
# The measurement behind docs/CACHE-STORAGE-EVALUATION.md. It is a gate rather
# than a report so the argument for keeping the cache in process decays into a
# test failure rather than into a stale claim in a document.
run storage.sh "${STORAGE_SAMPLES:-60}" "${STORAGE_RENDER_MS:-200}"

# bench/tracing.sh also needs python3 (for the OTLP collector it asserts
# against) and skips itself with a message when it is missing.
#
# WebSockets need a Python interpreter for the client. Skipped loudly, on the
# same reasoning as the Docker block below.
if command -v python3 >/dev/null; then
  run websocket.sh
else
  echo
  echo "SKIPPED bench/websocket.sh — no python3 for the WebSocket client."
  FAILED="$FAILED websocket.sh(no-python3)"
fi

# TLS needs its own build (`--features tls`, a two-minute compile) and
# openssl to mint a certificate. SKIP_TLS=1 acknowledges leaving it out.
if [ "${SKIP_TLS:-0}" = "1" ]; then
  echo
  echo "SKIPPED bench/tls.sh (SKIP_TLS=1)"
elif command -v openssl >/dev/null; then
  run tls.sh
else
  echo
  echo "SKIPPED bench/tls.sh — no openssl to mint a test certificate."
  FAILED="$FAILED tls.sh(no-openssl)"
fi

# The Next.js proof needs Docker. It is skipped loudly rather than quietly: a
# suite that reports success while silently omitting its only real-framework
# test is exactly the kind of evidence this phase exists to remove.
if [ "${SKIP_DOCKER:-0}" = "1" ]; then
  echo
  echo "SKIPPED bench/nextjs.sh (SKIP_DOCKER=1)"
elif docker_available; then
  run nextjs.sh
  if [ "${SKIP_BROWSER:-0}" = "1" ]; then
    echo "SKIPPED bench/nextjs-browser.sh (SKIP_BROWSER=1)"
  else
    run nextjs-browser.sh
  fi
else
  echo
  echo "SKIPPED bench/nextjs.sh — no Docker daemon reachable within 10s."
  echo "  The real Next.js integration proof did NOT run. Set SKIP_DOCKER=1 to"
  echo "  acknowledge this deliberately."
  FAILED="$FAILED nextjs.sh(no-docker)"
fi

echo
echo "=================================================================="
[ -n "$PASSED" ] && echo "passed:$PASSED"
if [ -n "$FAILED" ]; then
  echo "FAILED:$FAILED"
  exit 1
fi
echo "all benchmarks held"
