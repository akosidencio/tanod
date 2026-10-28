# Cache scenarios for bench/perf.sh: hits, misses, coalescing, streaming and
# memory pressure. Every page is a public document the route declares
# shareable, because the fixture origin answers `no-store` the way a dynamic
# Next.js route does.
#
# `@...@` placeholders are substituted by bench/perf.sh.
version: 1
server:
  listen: "127.0.0.1:@LISTEN@"
@SERVER_EXTRA@
origin:
  upstreams: ["127.0.0.1:@ORIGIN@"]
  concurrency:
    max: @ORIGIN_MAX@
    queue:
      max: 4096
      timeout: 30s
cache:
  enabled: true
  max_memory: @CACHE_MEMORY@
  max_body_size: @MAX_BODY@
timeouts:
  origin: 30s
debug_headers: true
routes:
  - id: pages
    match: "/**"
    class: public_ssr
    cache:
      override_origin: true
      ttl:
        max: @TTL@
    coalesce:
      enabled: true
telemetry:
  prometheus:
    listen: "127.0.0.1:@METRICS@"
