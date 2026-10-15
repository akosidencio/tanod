# Benchmark config for bench/memory.sh.
#
# Every budget here is deliberately smaller than the workload the script
# drives through it. A configuration that comfortably fits its traffic proves
# nothing about what happens when it does not.
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
    max: 16
    queue:
      max: 200
      timeout: 30s
cache:
  enabled: true
  max_memory: 8MiB
  max_body_size: 2MiB
spool:
  enabled: true
  max_body: 2MiB
  max_memory: 4MiB
timeouts:
  downstream_write: 60s
telemetry:
  prometheus:
    listen: "127.0.0.1:@METRICS@"
routes:
  - id: spool
    match: "/big/1/spool"
    class: public_ssr
    cache:
      enabled: false
    coalesce:
      enabled: false
  - id: bulk
    match: "/big/**"
    class: public_ssr
    cache:
      override_origin: true
      ttl:
        max: 60s
  - id: hot
    match: "/hot/**"
    class: public_ssr
    cache:
      override_origin: true
      ttl:
        max: 5s
  - id: catchall
    match: "/**"
