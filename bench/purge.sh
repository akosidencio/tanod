#!/usr/bin/env bash
# Cache tags and the purge API, end to end.
#
# Three claims, each asserted against the origin's own render counter rather
# than against anything Tanod says about itself:
#
#   * a purge by tag removes exactly the entries carrying that tag
#   * an untagged-by-that-tag entry survives it
#   * the endpoint refuses everyone who cannot present the token
BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"

RENDER_MS=${1:-20}
TOKEN="bench-purge-token-0123456789abcdef"

bench_init purge
bench_param render_ms "$RENDER_MS"
bench_build

ORIGIN_PORT=$(bench_free_port)
LISTEN_PORT=$(bench_free_port)
METRICS_PORT=$(bench_free_port)
ADMIN_PORT=$(bench_free_port)
CONFIG="$BENCH_DIR/purge.yaml"
bench_render_config "$BENCH_ROOT/bench/purge.yaml" "$CONFIG" \
  "LISTEN=$LISTEN_PORT" "ORIGIN=$ORIGIN_PORT" "METRICS=$METRICS_PORT" \
  "ADMIN=$ADMIN_PORT" "TOKEN=$TOKEN"

bench_spawn origin "$(bench_bin slow-origin)" "$ORIGIN_PORT" "$RENDER_MS"
bench_wait_port 127.0.0.1 "$ORIGIN_PORT" "slow-origin"
bench_start_tanod tanod "$CONFIG" "$LISTEN_PORT" "$METRICS_PORT"
bench_wait_http "http://127.0.0.1:$ADMIN_PORT/health/live" "tanod admin"
bench_origin_reset "$ORIGIN_PORT"

get() { curl -sS -o /dev/null -D - "http://127.0.0.1:$LISTEN_PORT$1" | tr -d '\r' | sed -n 's/^[Xx]-[Hh]armost: //p'; }
purge() { # query, token -> http status
  curl -sS -o "$BENCH_DIR/purge-body" -w '%{http_code}' -X POST \
    -H "Authorization: Bearer $2" \
    "http://127.0.0.1:$ADMIN_PORT/purge?$1"
}
status_field() { # field
  curl -fsS "http://127.0.0.1:$ADMIN_PORT/status" \
    | sed -n "s/.*\"$1\":\([0-9]*\).*/\1/p" | head -1
}

# ---- fill: two entries under `shoes`, one under `hats`
for path in /tagged/shoes/1 /tagged/shoes/2 /tagged/hats/1; do
  [ "$(get "$path")" = "MISS" ] || bench_fail "$path was not a MISS on first request"
done
for path in /tagged/shoes/1 /tagged/shoes/2 /tagged/hats/1; do
  [ "$(get "$path")" = "HIT" ] || bench_fail "$path did not cache"
done
RENDERS_BEFORE=$(bench_origin_stat "$ORIGIN_PORT" total)
bench_assert_eq "$RENDERS_BEFORE" 3 "origin renders while filling"
bench_assert_eq "$(status_field entries)" 3 "cached entries"

# ---- the security half, before the useful half
for attempt in "wrong-token" ""; do
  CODE=$(purge "tag=shoes" "$attempt")
  [ "$CODE" = "401" ] || bench_fail "purge with a bad token answered $CODE, not 401"
done
CODE=$(curl -sS -o /dev/null -w '%{http_code}' \
  "http://127.0.0.1:$ADMIN_PORT/purge?tag=shoes&all=1")
[ "$CODE" = "405" ] || bench_fail "a GET to /purge answered $CODE, not 405"
CODE=$(purge "tags=shoes" "$TOKEN")
[ "$CODE" = "400" ] || bench_fail "a misspelled parameter answered $CODE, not 400"
bench_assert_eq "$(status_field entries)" 3 "entries after refused purges"

# ---- purge by tag
CODE=$(purge "tag=shoes" "$TOKEN")
[ "$CODE" = "200" ] || bench_fail "an authorised purge answered $CODE: $(cat "$BENCH_DIR/purge-body")"
PURGED=$(sed -n 's/.*"entries":\([0-9]*\).*/\1/p' "$BENCH_DIR/purge-body")
bench_assert_eq "${PURGED:-0}" 2 "entries reported purged"
bench_assert_eq "$(status_field entries)" 1 "entries left in the cache"

# The untagged-by-shoes entry has to be untouched, and the shoes pages have to
# be gone — measured at the origin, which is what actually re-renders.
[ "$(get /tagged/hats/1)" = "HIT" ] || bench_fail "purging the shoes tag took the hats entry with it"
[ "$(get /tagged/shoes/1)" = "MISS" ] || bench_fail "a purged entry was still served"
[ "$(get /tagged/shoes/2)" = "MISS" ] || bench_fail "a purged entry was still served"
RENDERS_AFTER=$(bench_origin_stat "$ORIGIN_PORT" total)
bench_assert_eq "$((RENDERS_AFTER - RENDERS_BEFORE))" 2 "re-renders caused by the purge"

# ---- purge by path, including a query variant of the same path
#
# The variant matters: `/tagged/hats/1?ref=a` is a different cache entry from
# `/tagged/hats/1`, and `revalidatePath()` means both of them. This is the half
# a key-based purge cannot do at all — entries are keyed by a hash, so without
# the path stored there is no way back from a URL to its entries.
get "/tagged/hats/1?ref=a" >/dev/null
[ "$(get "/tagged/hats/1?ref=a")" = "HIT" ] || bench_fail "the query variant did not cache"
# hats/1 survived the tag purge; shoes/1 and shoes/2 were re-cached by the
# assertions above; the query variant is the fourth.
bench_assert_eq "$(status_field entries)" 4 "entries before the path purge"

CODE=$(purge "path=/tagged/hats/1" "$TOKEN")
[ "$CODE" = "200" ] || bench_fail "a path purge answered $CODE: $(cat "$BENCH_DIR/purge-body")"
BY_PATH=$(sed -n 's/.*"entries":\([0-9]*\).*/\1/p' "$BENCH_DIR/purge-body")
bench_assert_eq "${BY_PATH:-0}" 2 "entries removed by a path purge (both variants)"
bench_assert_eq "$(status_field entries)" 2 "entries left after the path purge"

CODE=$(purge "path=tagged/hats/1" "$TOKEN")
[ "$CODE" = "400" ] || bench_fail "a relative path answered $CODE, not 400"

# ---- purge everything
purge "all=1" "$TOKEN" >/dev/null
bench_assert_eq "$(status_field entries)" 0 "entries after purging everything"
bench_assert_eq "$(status_field bytes_used)" 0 "bytes after purging everything"

PURGED_METRIC=$(curl -fsS "http://127.0.0.1:$METRICS_PORT/metrics" \
  | awk '/^tanod_cache_purged_total[{]/ { sum += $2 } END { print sum + 0 }')
bench_assert_gt "$PURGED_METRIC" 2 "tanod_cache_purged_total"

echo
bench_print_params
echo
bench_result purged_by_tag "$PURGED"
bench_result purged_by_path "$BY_PATH"
bench_result rerenders_after_purge "$((RENDERS_AFTER - RENDERS_BEFORE))"

bench_assert_no_panics tanod
bench_pass "a tag purge removed 2 of 3 entries and cost exactly 2 re-renders; a path purge removed both query variants of one page; unauthorised and malformed purges changed nothing"
