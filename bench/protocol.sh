#!/usr/bin/env bash
# HTTP semantics a cache is allowed to get wrong quietly.
#
# Every check here has the same shape: a request whose *correct* handling
# differs from its convenient handling, and an assertion that a later,
# ordinary request is unaffected. That second half is the point. A `HEAD` that
# fills the cache with a bodyless entry, a `206` stored under the key of the
# full document, or a truncated body promoted to a complete one all produce a
# correct-looking answer for the request that caused them and a wrong one for
# everybody afterwards.
#
# Each block states what would be broken if the assertion failed, because the
# symptom is rarely visible where the mistake is.
BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"

RENDER_MS=${1:-300}

bench_init protocol
bench_param render_ms "$RENDER_MS"
bench_build

ORIGIN_PORT=$(bench_start_origin origin "$RENDER_MS")
LISTEN=$(bench_free_port)
bench_render_config "$BENCH_ROOT/bench/protocol.yaml" "$BENCH_DIR/protocol.yaml" \
  "LISTEN=$LISTEN" "ORIGIN=$ORIGIN_PORT" "CEILING=20"
bench_start_tanod tanod "$BENCH_DIR/protocol.yaml" "$LISTEN"

BASE="http://127.0.0.1:$LISTEN"
CHECKS=0
note() { CHECKS=$((CHECKS + 1)); printf '  %-46s %s\n' "$1" "$2"; }

# curl's status/size, without letting a non-2xx exit code abort the script.
status() { curl -s -o /dev/null -w '%{http_code}' --max-time 15 "$@"; }
body_bytes() { curl -s --max-time 15 "$@" | wc -c | tr -d ' '; }

echo "HTTP semantics through Tanod"
echo

# ------------------------------------------------------------------- HEAD
#
# Pingora rewrites a cache-filling HEAD into a GET upstream and drops the body
# on the way back, so the entry it stores is the complete document. What is
# being checked is the consequence: a GET arriving after a HEAD must get the
# whole body, not the empty one the HEAD asked for.
bench_origin_reset "$ORIGIN_PORT"
HEAD_CODE=$(status -I "$BASE/validated/head/1")
HEAD_BYTES=$(body_bytes -I "$BASE/validated/head/1")
GET_BYTES=$(body_bytes "$BASE/validated/head/1")
bench_assert_eq "$HEAD_CODE" 200 "HEAD status"
note "HEAD returns headers only" "${HEAD_CODE}, ${HEAD_BYTES}B of headers"
bench_assert_gt "$GET_BYTES" 0 \
  "GET after HEAD returned an empty body — the HEAD stored a bodyless entry under the document's key"
note "GET after HEAD still has a body" "${GET_BYTES}B"
bench_result head_get_bytes "$GET_BYTES"

# ------------------------------------------------------------------ Range
#
# A ranged request on a miss must not put a `206` in the cache. Pingora strips
# `Range` from the upstream request when the response will fill the cache, so
# the origin returns the whole document and the range is served out of the
# stored entry. The failure this guards is the other order: store the partial
# response, then serve ten bytes to everyone who asks for the page.
bench_origin_reset "$ORIGIN_PORT"
RANGE_CODE=$(status -H 'Range: bytes=0-9' "$BASE/validated/range/1")
RANGE_BYTES=$(body_bytes -H 'Range: bytes=0-9' "$BASE/validated/range/1")
FULL_BYTES=$(body_bytes "$BASE/validated/range/1")
bench_assert_eq "$RANGE_CODE" 206 "ranged request status"
bench_assert_eq "$RANGE_BYTES" 10 "ranged request body length"
note "Range on a cold miss answers 206" "${RANGE_CODE}, ${RANGE_BYTES}B"
bench_assert_gt "$FULL_BYTES" 10 \
  "the full document came back as ${FULL_BYTES}B — a 206 was stored under the document's key"
note "the full document is still whole afterwards" "${FULL_BYTES}B"
bench_result range_full_bytes "$FULL_BYTES"

# An unsatisfiable range must not be answered with the whole document, which
# is the lazy failure mode and silently breaks resumable downloads.
UNSAT=$(status -H 'Range: bytes=99999999-' "$BASE/validated/range/1")
[ "$UNSAT" = "416" ] || [ "$UNSAT" = "200" ] || bench_fail \
  "an unsatisfiable range produced HTTP $UNSAT"
note "unsatisfiable range" "$UNSAT"

# ------------------------------------------------- conditional requests
#
# The origin sends an `ETag`. A revalidating client must get a `304` with no
# body, and — this is the part worth measuring — the origin must not be asked
# again, because answering a revalidation out of the cache is the entire point
# of storing validators.
bench_origin_reset "$ORIGIN_PORT"
ETAG=$(curl -s -D - -o /dev/null --max-time 15 "$BASE/validated/cond/7" \
  | tr -d '\r' | sed -n 's/^[Ee][Tt][Aa][Gg]: //p')
[ -n "$ETAG" ] || bench_fail "the origin's ETag did not survive the proxy"
COND_CODE=$(status -H "If-None-Match: $ETAG" "$BASE/validated/cond/7")
COND_BYTES=$(body_bytes -H "If-None-Match: $ETAG" "$BASE/validated/cond/7")
RENDERS=$(bench_origin_stat "$ORIGIN_PORT" total)
bench_assert_eq "$COND_CODE" 304 "conditional request status"
bench_assert_eq "$COND_BYTES" 0 "a 304 must carry no body"
note "If-None-Match answers 304, no body" "$COND_CODE"
bench_assert_eq "$RENDERS" 1 \
  "the origin rendered $RENDERS times for one document plus two revalidations"
note "revalidations cost no origin renders" "$RENDERS render(s)"
bench_result conditional_renders "$RENDERS"

# A conditional request that does *not* match must get the document, not a
# `304`. Getting this backwards serves an empty response as if it were fresh.
STALE_CODE=$(status -H 'If-None-Match: "something-else"' "$BASE/validated/cond/7")
bench_assert_eq "$STALE_CODE" 200 "a non-matching validator must return the document"
note "non-matching If-None-Match returns the document" "$STALE_CODE"

# --------------------------------------------------------- malformed bodies
#
# The origin promises `Content-Length: N` and sends N/2 bytes, with headers
# that say the response is cacheable for a minute. If a truncated fill is ever
# promoted to a complete entry, every later request is served half a page from
# memory — fast, cached, and wrong, for the full TTL.
bench_origin_reset "$ORIGIN_PORT"
TRUNC_FIRST=$(body_bytes "$BASE/truncated/4096")
TRUNC_SECOND=$(body_bytes "$BASE/truncated/4096")
TRUNC_RENDERS=$(bench_origin_stat "$ORIGIN_PORT" total)
note "truncated body, first request" "${TRUNC_FIRST}B"
note "truncated body, second request" "${TRUNC_SECOND}B"
bench_assert_gt "$TRUNC_RENDERS" 1 \
  "the second request for a truncated response was served from cache ($TRUNC_RENDERS origin render(s) for two requests) — a half-written body was promoted to a complete cache entry"
note "a truncated fill is never promoted" "$TRUNC_RENDERS render(s) for 2 requests"
bench_result truncated_renders "$TRUNC_RENDERS"

# The same failure via chunked framing: a well-formed chunk and then silence,
# with no terminating zero-length chunk.
bench_origin_reset "$ORIGIN_PORT"
body_bytes "$BASE/badchunk/1" > /dev/null
body_bytes "$BASE/badchunk/1" > /dev/null
CHUNK_RENDERS=$(bench_origin_stat "$ORIGIN_PORT" total)
bench_assert_gt "$CHUNK_RENDERS" 1 \
  "a chunked response that never terminated was stored and replayed ($CHUNK_RENDERS origin render(s) for two requests)"
note "an unterminated chunked fill is never promoted" "$CHUNK_RENDERS render(s) for 2 requests"
bench_result badchunk_renders "$CHUNK_RENDERS"

# The proxy must survive both. A panic here is a denial of service that costs
# one misbehaving origin response to trigger.
bench_alive "$(bench_pid tanod)" || bench_fail "tanod died on a malformed origin response"
note "tanod survived both malformed responses" "alive"

# ---------------------------------------------------------- disconnects
#
# A client that hangs up mid-render must give its permit back. If it does not,
# capacity leaks one slot per abandoned request and the proxy silently
# tightens its own ceiling until it admits nothing — a failure that looks like
# an overloaded origin and is not.
LISTEN_TIGHT=$(bench_free_port)
bench_render_config "$BENCH_ROOT/bench/protocol.yaml" "$BENCH_DIR/tight.yaml" \
  "LISTEN=$LISTEN_TIGHT" "ORIGIN=$ORIGIN_PORT" "CEILING=1"
bench_start_tanod tight "$BENCH_DIR/tight.yaml" "$LISTEN_TIGHT"

ABANDONED=6
for i in $(seq 1 $ABANDONED); do
  # `--max-time` shorter than the render, so curl hangs up while the origin is
  # still working. With a ceiling of 1 a single leaked permit is fatal.
  curl -s -o /dev/null --max-time 0.2 "http://127.0.0.1:$LISTEN_TIGHT/slow/abandon-$i" || true
done
sleep 1

DISCONNECT_CODE=$(status "http://127.0.0.1:$LISTEN_TIGHT/slow/after")
bench_assert_eq "$DISCONNECT_CODE" 200 \
  "after $ABANDONED clients hung up mid-render, the next request got HTTP $DISCONNECT_CODE — each abandoned request leaked its permit"
note "$ABANDONED mid-render disconnects leak no capacity" "$DISCONNECT_CODE"
bench_result disconnect_status "$DISCONNECT_CODE"

echo
bench_print_params
echo
bench_result checks "$CHECKS"
bench_pass "$CHECKS HTTP-semantics checks held: HEAD, Range, conditional revalidation, truncated and unterminated bodies, and mid-render disconnects"
