# Mixed production traffic and origin overload for bench/perf.sh.
#
# One route per traffic class in bench/perf-mixed.mix, so each class reaches
# the policy it would in a real Next.js deployment: static assets and public
# pages cached, account pages private, the search API never cached. The
# origin itself is capacity-limited (SLOW_ORIGIN_SLOTS), and the ceiling here
# is what keeps it from being driven past that.
#
# `@...@` placeholders are substituted by bench/perf.sh.
version: 1
mode: @MODE@
server:
  listen: "127.0.0.1:@LISTEN@"
@SERVER_EXTRA@
origin:
  upstreams: ["127.0.0.1:@ORIGIN@"]
  concurrency:
    max: @ORIGIN_MAX@
    queue:
      max: @QUEUE_MAX@
      timeout: @QUEUE_TIMEOUT@
cache:
  enabled: @CACHE_ENABLED@
  max_memory: @CACHE_MEMORY@
  max_body_size: 8MiB
timeouts:
  origin: 30s
routes:
  - id: next-static
    match: "/_next/static/**"
    class: static
    cache:
      override_origin: true
      ttl:
        max: 1h
  - id: products
    match: "/products/**"
    class: public_ssr
    cache:
      override_origin: true
      ttl:
        max: @TTL@
    coalesce:
      enabled: true
  - id: account
    match: "/account/**"
    class: private_dynamic
  - id: api
    match: "/api/**"
    class: public_dynamic
    cache:
      enabled: false
  - id: catchall
    match: "/**"
telemetry:
  prometheus:
    listen: "127.0.0.1:@METRICS@"
