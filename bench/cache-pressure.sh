#!/usr/bin/env bash
# What a full cache does to the responses passing through it.
#
# A cache is an optimisation, so running out of cache memory must not become
# an outage where it can be avoided, and where it cannot it must fail loudly:
# a response is either delivered byte-for-byte or aborted so the client can
# tell. A short body presented as complete is the one unacceptable outcome.
#
#   1. Budget too small for any fill: every response is served uncached, and a
#      coalesced herd still gets complete bodies.
#   2. Budget exhausted mid-body: the fill is abandoned and finishes from the
#      headroom; its coalesced follower gets the full body too; nothing over
#      budget is stored; a later fill that fits is cached normally.
#   3. Headroom exhausted by a burst: every response is complete or a
#      detectable transfer error, and the accounting settles to zero pinned
#      bytes afterwards.
#   4. A chunked body larger than `cache.max_body_size`: Pingora's own limit,
#      recorded so a change in its behaviour is noticed.
#
# Usage: bench/cache-pressure.sh [RENDER_MS]
# CACHE_EVICTION=clock|tinyufo selects the retention policy (default clock).
BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"

RENDER_MS=${1:-800}
EVICTION=${CACHE_EVICTION:-clock}

bench_init cache_pressure
bench_param render_ms "$RENDER_MS"
bench_param eviction "$EVICTION"
bench_build

ORIGIN_PORT=$(bench_start_origin origin "$RENDER_MS")
# One 256 KiB stream chunk plus framing, and a two-chunk body.
CHUNK=$((256 * 1024 + 11))
TWO=$((2 * CHUNK))

start() { # name, max_memory, max_body
  PORT=$(bench_free_port)
  ADMIN=$(bench_free_port)
  bench_render_config "$BENCH_ROOT/bench/cache-pressure.yaml.tpl" "$BENCH_DIR/$1.yaml" \
    "LISTEN=$PORT" "ORIGIN=$ORIGIN_PORT" "ADMIN=$ADMIN" "EVICTION=$EVICTION" \
    "MAX_MEMORY=$2" "MAX_BODY=$3"
  bench_start_tanod "$1" "$BENCH_DIR/$1.yaml" "$PORT"
  bench_wait_port 127.0.0.1 "$ADMIN" "tanod admin"
}

status() { curl -s --max-time 5 "http://127.0.0.1:$ADMIN/status"; }
# One number from the status document's cache section.
stat_field() { # field under .cache, or under .cache.memory_pressure
  status | python3 -c 'import json,sys
c = json.load(sys.stdin)["cache"]
f = sys.argv[1]
print(c[f] if f in c else c["memory_pressure"][f])' "$1"
}
check() { printf '  %-52s %s\n' "$1" "$2"; }

# Fetch one URL into a file and record "exit code, HTTP status, bytes" there.
fetch() { # url, out
  local code
  code=$(curl -s -o "$2" -w '%{http_code}' --max-time 60 "$1")
  echo "$? $code $(wc -c < "$2" | tr -d ' ')" > "$2.result"
}

# A response is acceptable if it is complete, or if curl saw it fail.
judge() { # out, expected_bytes, label
  local exit code bytes
  read -r exit code bytes < "$1.result"
  if [ "$exit" = 0 ]; then
    [ "$code" = 200 ] || bench_fail "$3: HTTP $code"
    [ "$bytes" = "$2" ] || bench_fail \
      "$3: a ${bytes}-byte body was delivered as complete, expected $2 — truncation presented as success"
    echo complete
  else
    echo aborted
  fi
}

wait_settled() { # wait for every fill to end and its bytes to return
  local i
  for i in $(seq 1 100); do
    [ "$(stat_field pinned_bytes)" = 0 ] && return 0
    sleep 0.1
  done
  bench_fail "pinned cache bytes never returned to zero: $(status)"
}

echo "cache memory pressure through a real proxy ($EVICTION)"
echo

# ======================================================= 1. no room at all
start tiny 256B 256B
bench_origin_reset "$ORIGIN_PORT"
for n in 1 2 3 4 5 6; do
  fetch "http://127.0.0.1:$PORT/reflect/herd" "$BENCH_DIR/herd-$n" &
done
wait
for n in 1 2 3 4 5 6; do
  [ "$(judge "$BENCH_DIR/herd-$n" "$(wc -c < "$BENCH_DIR/herd-1" | tr -d ' ')" "herd $n")" = complete ] \
    || bench_fail "a request failed only because the cache had no memory"
  grep -q '</html>' "$BENCH_DIR/herd-$n" || bench_fail "herd response $n is not a whole page"
done
FALLBACKS=$(stat_field uncached_fallback)
# Only the request that reached the response becomes a fallback; the others
# were waiting on its cache lock, which it gave up, and went upstream uncached.
bench_assert_gt "$FALLBACKS" 0 "no uncached fallback was recorded"
[ "$(stat_field bytes_used)" = 0 ] || bench_fail "a 256-byte cache stored something: $(status)"
check "6 concurrent requests, no cache memory" "all complete, $FALLBACKS uncached fallbacks"
bench_result uncached_fallbacks "$FALLBACKS"
bench_stop tiny

# ============================================== 2. one abandoned fill
#
# A and B are two-chunk streams. B starts while A is between chunks, so when
# A's second chunk arrives the budget holds A's first chunk and B's: A must
# continue on the headroom. A follower joins A before that happens.
start abandon 600KiB 600KiB
bench_origin_reset "$ORIGIN_PORT"
fetch "http://127.0.0.1:$PORT/bigstream/a/2" "$BENCH_DIR/a" &
sleep 0.15
fetch "http://127.0.0.1:$PORT/bigstream/a/2" "$BENCH_DIR/a-follower" &
sleep 0.15
fetch "http://127.0.0.1:$PORT/bigstream/b/2" "$BENCH_DIR/b" &
wait
for f in a a-follower b; do
  [ "$(judge "$BENCH_DIR/$f" "$TWO" "$f")" = complete ] \
    || bench_fail "response $f was aborted although the headroom had room for it"
done
cmp -s "$BENCH_DIR/a" "$BENCH_DIR/a-follower" || bench_fail "the follower of an abandoned fill got a different body"
ABANDONED=$(stat_field fill_abandoned)
bench_assert_eq "$ABANDONED" 1 "expected exactly one abandoned fill"
bench_assert_eq "$(stat_field memory_exhausted)" 0 "a fill failed with headroom to spare"
RENDERS=$(bench_origin_stat "$ORIGIN_PORT" total)
bench_assert_eq "$RENDERS" 2 "A and its follower should share one render, B its own: got $RENDERS"
wait_settled

# The abandoned fill was not stored; B fitted the budget and was. B is asked
# for first: refilling A needs B's room, and evicting it is the budget working.
bench_origin_reset "$ORIGIN_PORT"
fetch "http://127.0.0.1:$PORT/bigstream/b/2" "$BENCH_DIR/b-again"
fetch "http://127.0.0.1:$PORT/bigstream/a/2" "$BENCH_DIR/a-again"
judge "$BENCH_DIR/a-again" "$TWO" "a again" > /dev/null
judge "$BENCH_DIR/b-again" "$TWO" "b again" > /dev/null
bench_assert_eq "$(bench_origin_stat "$ORIGIN_PORT" total)" 1 \
  "after pressure, the abandoned URL must re-render and the stored one must hit"
USED=$(stat_field bytes_used)
bench_assert_le "$USED" $((600 * 1024)) "cache bytes over budget once the fills ended"
check "fill abandoned mid-body, follower attached" "3 complete, 1 abandoned, 2 renders"
check "afterwards: abandoned re-renders, fitted entry hits" "used ${USED}B of 600KiB"
bench_result abandoned "$ABANDONED"
bench_stop abandon

# ============================================== 3. headroom exhausted
#
# Four three-chunk streams registered within 60ms. Their first chunks fill
# the 1 MiB budget; their second chunks, 800ms later, need 2 MiB against
# budget plus headroom of 2 MiB less metadata, so at least one fill must be
# refused and its response aborted while the others finish.
start burst 1MiB 1MiB
bench_origin_reset "$ORIGIN_PORT"
THREE=$((3 * CHUNK))
for n in 1 2 3 4; do
  fetch "http://127.0.0.1:$PORT/bigstream/burst$n/3" "$BENCH_DIR/burst-$n" &
  sleep 0.02
done
wait
COMPLETE=0; ABORTED=0
for n in 1 2 3 4; do
  case "$(judge "$BENCH_DIR/burst-$n" "$THREE" "burst $n")" in
    complete) COMPLETE=$((COMPLETE + 1)) ;;
    aborted) ABORTED=$((ABORTED + 1)) ;;
  esac
done
EXHAUSTED=$(stat_field memory_exhausted)
ABANDONED=$(stat_field fill_abandoned)
bench_assert_eq "$ABORTED" "$EXHAUSTED" \
  "aborted responses ($ABORTED) must be exactly the fills that exhausted the headroom ($EXHAUSTED)"
bench_assert_gt "$ABANDONED" 0 "the burst never reached the headroom"
bench_assert_gt "$EXHAUSTED" 0 "the burst never exhausted the headroom"
bench_assert_gt "$COMPLETE" 0 "no response survived the burst"
wait_settled
# The proxy is healthy afterwards: an ordinary cacheable request works.
fetch "http://127.0.0.1:$PORT/reflect/after" "$BENCH_DIR/after"
[ "$(judge "$BENCH_DIR/after" "$(wc -c < "$BENCH_DIR/after" | tr -d ' ')" after)" = complete ] \
  || bench_fail "the proxy did not recover after memory pressure"
check "4-stream burst against 1MiB + 1MiB headroom" "$COMPLETE complete, $ABORTED aborted, $ABANDONED abandoned"
bench_result burst_complete "$COMPLETE"
bench_result burst_aborted "$ABORTED"
bench_stop burst

# ============================================== 4. oversized chunked body
#
# Three chunks exceed `max_body_size`. Pingora enforces that limit itself,
# before the store sees the bytes; this records what it does with a body that
# had no Content-Length to reject it up front.
start oversized 4MiB 520KiB
fetch "http://127.0.0.1:$PORT/bigstream/big/3" "$BENCH_DIR/big"
OVERSIZED=$(judge "$BENCH_DIR/big" $((3 * CHUNK)) oversized)
fetch "http://127.0.0.1:$PORT/bigstream/big/3" "$BENCH_DIR/big-again"
OVERSIZED_AGAIN=$(judge "$BENCH_DIR/big-again" $((3 * CHUNK)) "oversized again")
# The first discovery aborts: Pingora finds the size mid-stream, after it has
# started serving from the cache, and cannot switch to pass-through. From then
# on the key is remembered and the response streams uncached.
[ "$OVERSIZED_AGAIN" = complete ] || bench_fail \
  "an oversized streamed response failed again after the first discovery; every request for it fails"
fetch "http://127.0.0.1:$PORT/bigstream/big/3" "$BENCH_DIR/big-third"
[ "$(judge "$BENCH_DIR/big-third" $((3 * CHUNK)) "oversized third")" = complete ] \
  || bench_fail "the oversized response did not stay uncached"
check "chunked body over max_body_size" "first: $OVERSIZED, then: $OVERSIZED_AGAIN"
bench_result oversized_first "$OVERSIZED"
bench_result oversized_repeat "$OVERSIZED_AGAIN"
bench_stop oversized

for name in tiny abandon burst oversized; do
  bench_assert_no_panics "$name"
done

echo
bench_print_params
echo
bench_pass "an exhausted cache budget degrades to uncached serving where Pingora allows it, finishes abandoned fills from a bounded headroom, and otherwise aborts visibly; no response was delivered short as if complete"
