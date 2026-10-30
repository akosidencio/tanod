# Rendered by bench/showcase/showcase.py for the Tanod arm. The values marked
# @@...@@ come from run.sh options and are recorded in metadata.json.
version: 1
mode: protect

server:
  listen: "0.0.0.0:8080"
  trusted_proxies:
    from: ["172.16.0.0/12"]
    client_ip: x_forwarded
    scheme: x_forwarded

origin:
  upstreams:
    - "next-1:3000"
    - "next-2:3000"
    - "next-3:3000"
  # The same distribution NGINX uses in the baseline arm.
  load_balancing: round_robin
  concurrency:
    max: @@TANOD_MAX@@
    queue:
      max: @@TANOD_QUEUE_MAX@@
      timeout: @@TANOD_QUEUE_TIMEOUT@@

health:
  path: "/healthz"
  interval: 2s
  timeout: 1s
  healthy_after: 1
  unhealthy_after: 2

cache:
  enabled: @@TANOD_CACHE@@
  store: memory
  max_memory: 64MiB
  max_body_size: 4MiB

coalesce:
  enabled: @@TANOD_COALESCE@@
  on_timeout: stale_or_shed

timeouts:
  connect: 1s
  first_byte: 15s
  idle: 30s
  origin: 30s
  downstream_write: 15s

# Adds `X-Tanod: HIT|MISS|SHED|...`, which k6 uses to keep cache hits out of the
# admission-control evidence. It changes no admission or caching decision.
debug_headers: true

routes:
  - id: health
    match: "/healthz"
    class: private_dynamic
    cache:
      enabled: false

  # Mechanism workload: the fixture's sleeping product page.
  - id: products
    match: "/products/**"
    class: public_ssr
    cache:
      override_origin: true
      ttl:
        max: @@TANOD_CACHE_TTL@@
    coalesce:
      enabled: @@TANOD_COALESCE@@

  # Constrained workload: the fixture's CPU-bound catalog page.
  - id: catalog
    match: "/catalog/**"
    class: public_ssr
    cache:
      override_origin: true
      ttl:
        max: @@TANOD_CACHE_TTL@@
    coalesce:
      enabled: @@TANOD_COALESCE@@

  - id: private-default
    match: "/**"
    class: private_dynamic
    cache:
      enabled: false
    coalesce:
      enabled: false

telemetry:
  prometheus:
    listen: "0.0.0.0:9090"
  admin:
    listen: "0.0.0.0:9091"
    require_healthy_upstream: true
  logging:
    format: json
