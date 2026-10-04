# Roadmap

Tanod protects expensive SSR and dynamic origin workloads. It remains a
working prototype without sustained production validation.

## 6. Complete replica validation

- Validate the static partition and path-stable reference in production-shaped staging.
- Revisit capacity leases, adaptive limits, and distributed reuse only if fixed partitions prove insufficient.

## 7. Complete standalone distribution

- Validate the binary, generated config, systemd unit, and domain guide on a clean server.
- Add Linux ARM64 and package-manager installation when demand justifies them.

## 8. Expand framework support

- After phases 5–6, extract a versioned adapter contract and shared conformance tests.
- Choose the next framework based on demonstrated self-hosting demand.

## 9. Improve what a shed visitor sees

From staging load tests against a real Next.js storefront (2026-09-26/27): a
shed was a `503` with an empty body, so a document request landed on the
browser's own error screen.

- ~~Add a configurable overload body for document requests~~ — done in 0.2.0
  (`overload.page`).
- Serve a stale cached copy instead of shedding, per route, when one exists
  (for example `stale_on_shed: 5m` beside `stale_if_error`). Today stale is
  served only for origin failures; a shed is deliberately excluded
  (`should_serve_stale`), so an overloaded public page fails even when a
  minutes-old copy is in the cache.

## 10. Show what visitors experience

The dashboard could show what Tanod does to the origin, but not what visitors
got. From the same load tests:

- ~~Count responses by route and status class, including Tanod's own sheds~~ —
  done in 0.2.0 (`tanod_responses_total`).
- ~~Measure total time per request, split into queue wait and origin time~~ —
  done in 0.2.0 (`tanod_request_duration_seconds`, `tanod_queue_wait_seconds`).
- Estimate active visitors: distinct client addresses over the last 1 and 5
  minutes (an approximate sketch; no addresses leave the process), plus open
  downstream connections.
- ~~Export a build-info series~~ — done in 0.2.0 (`tanod_build_info`).
- ~~Reconcile `tanod_reuse_eligible_requests_total` with `tanod_cache_total`~~ —
  done in 0.2.0. Background stale-while-revalidate fetches were counted as
  visitor requests; they are now `status="revalidate"` and excluded from the
  eligible count. A per-process over-100% ratio could not be reproduced; if
  it reappears in staging, compare per instance before summing.

## Current boundaries

- Cache and coalescing remain local to each process; replica limits are statically partitioned and purges must reach every admin endpoint.
- Path purges match exact paths; dynamic route-pattern invalidation is not implemented.
- Slow readers can hold origin capacity unless response spooling is enabled, which sacrifices progressive rendering.
- Disk and external cache storage were [evaluated and declined](./CACHE-STORAGE-EVALUATION.md).
- Tanod does not replace an edge server, CDN, authentication, or client rate limiting.

See the [changelog](../CHANGELOG.md) for release history, the
[Next.js production reference](./NEXTJS-PRODUCTION-REFERENCE.md), and
[operations](./OPERATIONS.md) and [release gates](./RELEASE-GATES.md) for
deployment requirements.
