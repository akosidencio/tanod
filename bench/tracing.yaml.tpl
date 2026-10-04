# Benchmark config for bench/tracing.sh — correlation and span export.
#
# `trusted_proxies` names loopback so an inbound `traceparent` from the
# benchmark client is believed. Without it the default applies and every
# inbound context is ignored, which is the correct production default and
# would make half of this benchmark untestable.
version: 1
server:
  listen: "127.0.0.1:@LISTEN@"
  trusted_proxies:
    from: ["127.0.0.1/32"]
  graceful:
    pid_file: "@PIDFILE@"
    upgrade_socket: "@UPGRADESOCK@"
    drain_period: 1s
    shutdown_timeout: 3s
origin:
  upstreams: ["127.0.0.1:@ORIGIN@"]
cache:
  enabled: false
telemetry:
  prometheus:
    listen: "127.0.0.1:@METRICS@"
  tracing:
    service_name: tanod-bench
    # This fixture deliberately tests trusted inbound propagation. Production
    # keeps the safer `never` default unless this opt-in is explicit.
    trust_incoming: from_trusted_proxies
    sample:
      mode: always
    otlp:
      endpoint: "http://127.0.0.1:@COLLECTOR@/v1/traces"
      interval: 500ms
      max_queue: 512
      max_batch: 64
routes:
  - id: pages
    match: "/**"
    class: public_ssr
