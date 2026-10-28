# Benchmark config for bench/cache-pressure.sh.
#
# The budget fits two 256 KiB stream chunks in flight and little more, so a
# handful of concurrent cacheable streams is enough to run it out mid-body.
# The headroom is one `max_body_size`, which is what an abandoned fill may use
# to finish. Both are tiny on purpose: a budget the workload fits proves
# nothing about what happens when it does not.
version: 1
server:
  listen: "127.0.0.1:@LISTEN@"
origin:
  upstreams: ["127.0.0.1:@ORIGIN@"]
  concurrency:
    max: 32
    queue:
      max: 64
      timeout: 30s
cache:
  enabled: true
  eviction: @EVICTION@
  max_memory: @MAX_MEMORY@
  max_body_size: @MAX_BODY@
timeouts:
  origin: 30s
  idle: 30s
  first_byte: 30s
telemetry:
  admin:
    listen: "127.0.0.1:@ADMIN@"
debug_headers: true
routes:
  - id: streams
    match: "/bigstream/**"
    class: public_ssr
    cache:
      override_origin: true
      ttl:
        max: 60s
  - id: pages
    match: "/reflect/**"
    class: public_ssr
    cache:
      override_origin: true
      ttl:
        max: 60s
