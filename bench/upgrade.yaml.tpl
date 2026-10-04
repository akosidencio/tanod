# Benchmark config for bench/upgrade.sh — the zero-downtime handover.
#
# `upgrade_socket` is the only thing the two processes share, so it is
# allocated per run like the ports. Two Tanods on one host with the same
# default path would hand each other their listeners.
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
    max: 50
cache:
  enabled: false
telemetry:
  admin:
    listen: "127.0.0.1:@ADMIN@"
routes:
  - id: pages
    match: "/**"
    class: public_ssr
