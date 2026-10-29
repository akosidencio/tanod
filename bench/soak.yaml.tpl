# Benchmark config for bench/soak.sh.
#
# Deliberately tight: a 16MiB cache budget against a working set that outgrows
# it, so eviction runs for the whole soak rather than never. A generous budget
# would make the run prove nothing about the eviction path.
version: 1
server:
  listen: "127.0.0.1:@LISTEN@"
  graceful:
    pid_file: "@PIDFILE@"
    upgrade_socket: "@UPGRADESOCK@"
    drain_period: 1s
    shutdown_timeout: 3s
origin:
  upstreams: ["127.0.0.1:@ORIGIN@"]
  concurrency:
    max: 8
    queue:
      max: 200
      timeout: 5s
cache:
  enabled: true
  max_memory: 16MiB
  max_body_size: 2MiB
spool:
  enabled: true
  max_body: 1MiB
  max_memory: 8MiB
telemetry:
  admin:
    listen: "127.0.0.1:@ADMIN@"
  prometheus:
    listen: "127.0.0.1:@METRICS@"
routes:
  # A small hot set: cache hits and coalescing, which must consume no permit.
  - id: hot
    match: "/hot/**"
    class: public_ssr
    cache:
      override_origin: true
      ttl:
        max: 2s
  # Unique keys: pure admission pressure, and what fills and evicts the cache.
  - id: cold
    match: "/cold/**"
    class: public_ssr
    cache:
      override_origin: true
      ttl:
        max: 5s
  # Set-Cookie responses. The barrier no override reaches.
  - id: private
    match: "/private/**"
  # Large cacheable bodies with unique keys. `/big/1` is a 1MiB body and the
  # query string varies the cache key, so the 16MiB budget fills in sixteen
  # requests and then evicts continuously — which is the only way this soak
  # exercises eviction at all rather than merely filling a cache that is
  # always big enough.
  - id: bulk
    match: "/big/**"
    class: public_ssr
    cache:
      override_origin: true
      ttl:
        max: 30s
  # A progressive 1MiB response lasts at least 75ms. The 50ms disconnect
  # deadline therefore exercises cancellation while the origin is unfinished.
  - id: disconnect
    match: "/bigstream/**"
    class: public_ssr
    cache:
      override_origin: true
      ttl:
        max: 30s
  - id: catchall
    match: "/**"
