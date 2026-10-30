#!/usr/bin/env bash
# A real proxy cache hit must survive operational edits and disappear when the
# same URL's sharing policy changes, without requiring a deployment-id change.
BENCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$BENCH_ROOT/bench/lib.sh"

bench_init cache_policy_reload
bench_param render_ms 20
bench_build
ORIGIN_PORT=$(bench_free_port)
LISTEN_PORT=$(bench_free_port)
ADMIN_PORT=$(bench_free_port)
CONFIG="$BENCH_DIR/policy.yaml"

write_config() { # override, ttl, class, ceiling, extra cache fields, deployment
  cat > "$CONFIG" <<YAML
version: 1
server:
  listen: "127.0.0.1:$LISTEN_PORT"
origin:
  upstreams: ["127.0.0.1:$ORIGIN_PORT"]
  concurrency:
    max: ${4:-100}
cache:
  eviction: ${CACHE_EVICTION:-clock}
telemetry:
  admin:
    listen: "127.0.0.1:$ADMIN_PORT"
debug_headers: true
routes:
  - id: pages
    match: "/**"
    class: $3
    cache:
      override_origin: $1
      ttl:
        max: $2
YAML
  [ -z "${5:-}" ] || printf '%s\n' "$5" >> "$CONFIG"
  [ -z "${6:-}" ] || printf 'deployment:\n  id: %s\n' "$6" >> "$CONFIG"
}

request() {
  curl -fsS --max-time 10 -o /dev/null -D "$BENCH_DIR/headers" \
    "http://127.0.0.1:$LISTEN_PORT/product/iphone" || bench_fail "request failed"
}

GENERATION=1
reload_policy() {
  GENERATION=$((GENERATION + 1))
  kill -HUP "$PID"
  local observed=0
  # Poll the published generation, not a guessed delay after SIGHUP.
  for _ in $(seq 1 100); do
    observed=$(curl -fsS "http://127.0.0.1:$ADMIN_PORT/status" \
      | sed -n 's/.*"generation":\([0-9]*\).*/\1/p')
    [ "$observed" = "$GENERATION" ] && break
    sleep 0.05
  done
  bench_assert_eq "$observed" "$GENERATION" "cache policy reload publication"
}

assert_render_delta() { # expected, description
  local before after
  before=$(bench_origin_stat "$ORIGIN_PORT" total)
  request
  after=$(bench_origin_stat "$ORIGIN_PORT" total)
  bench_assert_eq "$((after - before))" "$1" "$2"
}

write_config true 60s public_ssr
bench_spawn origin "$(bench_bin slow-origin)" "$ORIGIN_PORT" 20
bench_wait_port 127.0.0.1 "$ORIGIN_PORT" "slow-origin"
bench_start_tanod tanod "$CONFIG" "$LISTEN_PORT"
bench_wait_http "http://127.0.0.1:$ADMIN_PORT/health/live" "tanod admin"
PID=$(bench_pid tanod)
bench_origin_reset "$ORIGIN_PORT"

assert_render_delta 1 "initial cache fill"
assert_render_delta 0 "initial cache hit"
write_config true 60s public_ssr 120
reload_policy
assert_render_delta 0 "concurrency-only reload preserves the cache"

write_config true 2s public_ssr
reload_policy
assert_render_delta 1 "shorter TTL cannot reuse the old override"
assert_render_delta 0 "new TTL still permits reuse"

# The selector headers/parameters are absent here. Without a policy namespace,
# these policy edits leave the canonical request key unchanged.
write_config true 60s public_ssr 100 '      vary: {headers: [accept-language]}'
reload_policy
assert_render_delta 1 "variant policy invalidates an old body"
assert_render_delta 0 "variant cache hit"
write_config true 60s public_ssr 100 '      query: {mode: include, keys: [sort]}'
reload_policy
assert_render_delta 1 "query policy invalidates an old body"
assert_render_delta 0 "query cache hit"

write_config false 60s public_ssr
reload_policy
assert_render_delta 1 "removing override cannot reuse an origin no-store response"
assert_render_delta 1 "origin no-store response remains unshared"

write_config true 60s public_ssr
reload_policy
assert_render_delta 1 "public route warmup"
assert_render_delta 0 "public route cache hit"
write_config false 60s private_dynamic
reload_policy
assert_render_delta 1 "private route cannot reuse a formerly public body"
assert_render_delta 1 "private route stays unshared"

write_config true 60s public_ssr 100 '' build-1
reload_policy
assert_render_delta 1 "first deployment fill"
assert_render_delta 0 "first deployment hit"
write_config true 60s public_ssr 100 '' build-2
reload_policy
assert_render_delta 1 "deployment rollover isolates entries"
assert_render_delta 0 "new deployment cache hit"
bench_result reloads "$((GENERATION - 1))"
bench_print_params
bench_pass "operational reload preserved a hit; TTL, variants, query, override, private-route and deployment changes forced new origin work"
