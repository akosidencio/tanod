# Benchmark config for bench/nextjs-forwarded.sh: one real Next.js origin
# behind one Tanod, with the trust settings substituted per run.
version: 1
mode: protect
server:
  listen: "127.0.0.1:@LISTEN@"
  trusted_proxies:
    from: @TRUST@
    client_ip: x_forwarded
    scheme: x_forwarded
    host: @HOST_SOURCE@
origin:
  upstreams: ["127.0.0.1:@ORIGIN@"]
  concurrency:
    max: 8
cache:
  enabled: true
debug_headers: true
routes:
  - id: products
    match: "/products/**"
    class: public_ssr
    cache:
      override_origin: true
      ttl:
        max: 30s
  - id: api
    match: "/api/**"
    class: private_dynamic
    cache:
      enabled: false
