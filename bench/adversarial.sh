#!/usr/bin/env bash
# Sustained hostile traffic against every mechanism at once.
#
# The other benchmarks each ask one question under clean conditions. This one
# runs the whole governor — cache, coalescing, admission, spool, upgrades —
# under continuous traffic from clients trying to break each of them, and then
# asserts the four properties that must hold no matter what arrived:
#
#   1. **The ceiling held.** Origin concurrency never exceeded what was
#      configured, whatever the attacker did to the key space.
#   2. **Nothing private was shared.** Every session cookie handed out under
#      load was distinct.
#   3. **Memory stayed inside its budget.** The cache and the spool are both
#      bounded, and a component whose job is absorbing spikes must not be the
#      thing that runs out of memory.
#   4. **It is still running.** A panic in the request path is a denial of
#      service that costs one request to trigger.
#
# The attack traffic is generated from the shapes that have historically
# produced real bugs in this codebase and in caches generally: unbounded key
# spaces, forwarded-header spoofing, obs-text that makes a header unreadable,
# preconditions and ranges, draft-mode cookie probes, and slow readers.
BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"

DURATION=${1:-20}
WORKERS=${2:-8}
CEILING=${3:-6}
CACHE_MEMORY=${4:-4MiB}

bench_init adversarial
bench_param duration_s "$DURATION"
bench_param workers "$WORKERS"
bench_param ceiling "$CEILING"
bench_param cache_memory "$CACHE_MEMORY"
bench_param render_ms 60
bench_build

ORIGIN_PORT=$(bench_start_origin origin 60)
LISTEN=$(bench_free_port)
METRICS=$(bench_free_port)
bench_render_config "$BENCH_ROOT/bench/adversarial.yaml" "$BENCH_DIR/adversarial.yaml" \
  "LISTEN=$LISTEN" "ORIGIN=$ORIGIN_PORT" "METRICS=$METRICS" \
  "CEILING=$CEILING" "CACHE_MEMORY=$CACHE_MEMORY"
bench_start_tanod tanod "$BENCH_DIR/adversarial.yaml" "$LISTEN" "$METRICS"
bench_origin_reset "$ORIGIN_PORT"

BASE="http://127.0.0.1:$LISTEN"
DEADLINE=$(( $(date +%s) + DURATION ))
hit() { curl -s -o /dev/null --max-time 6 "$@" 2>/dev/null || true; }

# Each worker below loops until the deadline. They are separate scripts rather
# than one mixed loop so a hang in one attack does not stop the others, and so
# the pid of each is tracked and torn down by the harness.

# 1. Cache-key flooding through the query string. `cache.query.mode: include`
#    lists only `q`, so an unlisted parameter must be dropped from the key —
#    otherwise every request is a fresh key and therefore a fresh render, and
#    the cache becomes an origin-work amplifier.
cat > "$BENCH_DIR/flood-query.sh" <<EOF
while [ \$(date +%s) -lt $DEADLINE ]; do
  for i in \$(seq 1 20); do
    curl -s -o /dev/null --max-time 6 "$BASE/p/hot?q=stable&cachebust=\$RANDOM\$i" 2>/dev/null
  done
done
EOF

# 2. Key flooding through headers the key is *not* allowed to carry, plus
#    forwarded-header spoofing. An untrusted peer must move neither.
cat > "$BENCH_DIR/flood-headers.sh" <<EOF
while [ \$(date +%s) -lt $DEADLINE ]; do
  curl -s -o /dev/null --max-time 6 \\
    -H "X-Forwarded-Proto: scheme-\$RANDOM" \\
    -H "X-Forwarded-For: 9.9.9.\$((RANDOM % 255))" \\
    -H "Forwarded: for=1.2.3.4;proto=https" \\
    -H "X-Made-Up-\$RANDOM: \$RANDOM" \\
    -H "Accept-Encoding: gzip;q=0.\$((RANDOM % 9)), br, identity" \\
    "$BASE/p/hot?q=stable" 2>/dev/null
done
EOF

# 3. Malformed and hostile metadata: obs-text in a cookie (the byte sequence
#    that once made an entire Cookie header unreadable and hid draft mode),
#    draft-mode probes, very long paths and queries, and conflicting framing.
cat > "$BENCH_DIR/malformed.sh" <<EOF
LONG=\$(head -c 3000 /dev/zero | tr '\\0' 'a')
while [ \$(date +%s) -lt $DEADLINE ]; do
  curl -s -o /dev/null --max-time 6 -H \$'Cookie: a=\\xc3\\xa9; b=1' "$BASE/p/hot?q=stable" 2>/dev/null
  curl -s -o /dev/null --max-time 6 -H 'Cookie: __prerender_bypass=probe' "$BASE/p/hot?q=stable" 2>/dev/null
  curl -s -o /dev/null --max-time 6 -H \$'RSC: \\xff\\xfe' "$BASE/p/hot?q=stable" 2>/dev/null
  curl -s -o /dev/null --max-time 6 -H 'Next-Action: probe' "$BASE/p/hot?q=stable" 2>/dev/null
  curl -s -o /dev/null --max-time 6 "$BASE/p/\$LONG" 2>/dev/null
  curl -s -o /dev/null --max-time 6 "$BASE/p/hot?q=stable&\$LONG=1" 2>/dev/null
  curl -s -o /dev/null --max-time 6 -H "Range: bytes=\$RANDOM-\$RANDOM" "$BASE/p/hot?q=stable" 2>/dev/null
  curl -s -o /dev/null --max-time 6 -H 'If-None-Match: "'"\$RANDOM"'"' "$BASE/p/hot?q=stable" 2>/dev/null
  curl -s -o /dev/null --max-time 6 -X HEAD "$BASE/p/hot?q=stable" 2>/dev/null
  curl -s -o /dev/null --max-time 6 "$BASE/truncated/8192" 2>/dev/null
  curl -s -o /dev/null --max-time 6 "$BASE/badchunk/1" 2>/dev/null
done
EOF

# 4. Bodies large enough to exercise the cache's byte budget and the spool's,
#    and slow readers to hold both open.
cat > "$BENCH_DIR/memory.sh" <<EOF
while [ \$(date +%s) -lt $DEADLINE ]; do
  curl -s -o /dev/null --max-time 6 "$BASE/big/\$RANDOM/1" 2>/dev/null
  curl -s -o /dev/null --max-time 4 --limit-rate 16k "$BASE/big/slow-\$RANDOM/1" 2>/dev/null
done
EOF

# 5. Traffic against the route that must never be shared, running the whole
#    time next to the route that may be.
cat > "$BENCH_DIR/private.sh" <<EOF
while [ \$(date +%s) -lt $DEADLINE ]; do
  curl -s --max-time 6 -D - -o /dev/null "$BASE/private/session" 2>/dev/null \\
    | tr -d '\\r' | sed -n 's/^[Ss]et-[Cc]ookie: //p' >> "$BENCH_DIR/cookies.txt"
done
EOF

# 6. Upgrade handshakes past the ceiling, mixed in so the socket limiter is
#    under contention rather than tested in isolation.
cat > "$BENCH_DIR/upgrades.sh" <<EOF
while [ \$(date +%s) -lt $DEADLINE ]; do
  python3 "$BENCH_ROOT/bench/ws-client.py" 127.0.0.1 $LISTEN /ws/flood --hold 1 >/dev/null 2>&1
done
EOF

: > "$BENCH_DIR/cookies.txt"
echo "sustained adversarial traffic for ${DURATION}s across $WORKERS worker groups"
echo

for name in flood-query flood-headers malformed memory private upgrades; do
  chmod +x "$BENCH_DIR/$name.sh"
  for n in $(seq 1 "$(( WORKERS / 6 + 1 ))"); do
    bench_spawn "$name-$n" bash "$BENCH_DIR/$name.sh"
  done
done

# Watch the ceiling *during* the run rather than only after it. A peak read
# once at the end can be satisfied by a proxy that recovered; the origin's own
# high-water mark cannot.
WORST=0
while [ "$(date +%s)" -lt "$DEADLINE" ]; do
  CURRENT=$(bench_origin_stat "$ORIGIN_PORT" peak)
  if bench_is_int "$CURRENT" && [ "$CURRENT" -gt "$WORST" ]; then WORST=$CURRENT; fi
  sleep 1
done

for name in flood-query flood-headers malformed memory private upgrades; do
  for n in $(seq 1 "$(( WORKERS / 6 + 1 ))"); do
    bench_stop "$name-$n"
  done
done
sleep 1

# ------------------------------------------------------------- assertions

# 1. The ceiling.
bench_assert_le "$WORST" "$CEILING" \
  "origin concurrency reached $WORST against a ceiling of $CEILING under adversarial load"
printf '  %-46s %s\n' "origin concurrency peak" "$WORST (ceiling $CEILING)"
bench_result peak "$WORST"

TOTAL=$(bench_origin_stat "$ORIGIN_PORT" total)
bench_assert_gt "$TOTAL" 10 \
  "only $TOTAL origin renders in ${DURATION}s — the load generators did not produce meaningful traffic, so nothing above was actually tested"
printf '  %-46s %s\n' "origin renders during the run" "$TOTAL"
bench_result origin_renders "$TOTAL"

# 2. Nothing private was shared. One cookie per response, always.
COOKIES=$(wc -l < "$BENCH_DIR/cookies.txt" | tr -d ' ')
DISTINCT=$(sort -u "$BENCH_DIR/cookies.txt" | wc -l | tr -d ' ')
bench_assert_gt "$COOKIES" 5 "only $COOKIES private responses were collected; the check is vacuous"
bench_assert_eq "$DISTINCT" "$COOKIES" \
  "$COOKIES private responses produced only $DISTINCT distinct session cookies — a Set-Cookie response was shared between clients"
printf '  %-46s %s\n' "private responses / distinct sessions" "$COOKIES / $DISTINCT"
bench_result private_responses "$COOKIES"
bench_result distinct_sessions "$DISTINCT"

# 3. Memory stayed inside its budget. Read from the proxy's own metrics,
#    which is the number an operator would page on.
metric() { curl -s --max-time 5 "http://127.0.0.1:$METRICS/metrics" | awk -v m="$1" '$1 == m { print $2 }'; }
CACHE_BYTES=$(metric tanod_cache_bytes)
SPOOL_BYTES=$(metric tanod_spool_bytes)
CACHE_LIMIT=$(( $(echo "$CACHE_MEMORY" | tr -dc '0-9') * 1024 * 1024 ))
bench_assert_int "${CACHE_BYTES%%.*}" "tanod_cache_bytes"
bench_assert_le "${CACHE_BYTES%%.*}" "$CACHE_LIMIT" \
  "the cache held ${CACHE_BYTES} bytes against a budget of $CACHE_MEMORY"
bench_assert_le "${SPOOL_BYTES%%.*}" 8388608 \
  "the spool held ${SPOOL_BYTES} bytes against a budget of 8MiB"
printf '  %-46s %s\n' "cache bytes / budget" "${CACHE_BYTES%%.*} / $CACHE_LIMIT"
printf '  %-46s %s\n' "spool bytes / budget" "${SPOOL_BYTES%%.*} / 8388608"
bench_result cache_bytes "${CACHE_BYTES%%.*}"
bench_result spool_bytes "${SPOOL_BYTES%%.*}"

# 4. Still running, and still correct — not merely alive.
bench_alive "$(bench_pid tanod)" || bench_fail \
  "tanod died during the run; see $(bench_log tanod)"
FINAL=$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 "$BASE/p/after?q=stable")
bench_assert_eq "$FINAL" 200 \
  "after the run the proxy answered HTTP $FINAL to an ordinary request"
printf '  %-46s %s\n' "alive and serving afterwards" "$FINAL"
bench_result final_status "$FINAL"

# A panic that was caught per-request would leave the process alive and the
# log full. Nothing in the request path may panic at all.
if grep -qi "panicked at" "$(bench_log tanod)"; then
  bench_fail "the proxy logged a panic under adversarial load:
$(grep -i -m3 'panicked at' "$(bench_log tanod)")"
fi
printf '  %-46s %s\n' "no panic in the request path" "clean"

echo
bench_print_params
echo
bench_pass "${DURATION}s of adversarial traffic: origin concurrency peaked at $WORST against a ceiling of $CEILING, $COOKIES private responses produced $DISTINCT distinct sessions, cache and spool stayed inside their budgets, and nothing panicked"
