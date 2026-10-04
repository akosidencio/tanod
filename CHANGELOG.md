# Changelog

Notable changes by version. See the [roadmap](./docs/ROADMAP.md) for remaining
work and [operations guide](./docs/OPERATIONS.md) for deployment details.

Tanod was released as Harmost up to 0.1.4. Entries below 0.2.0 use the current
names.

## 0.2.0 — 2026-10-04

- Renamed the project from Harmost to Tanod. Apart from the overload page below, behavior is unchanged from Harmost 0.1.4.
- Added an overload page: a shed browser navigation now gets a small page that says the site is busy and reloads itself, instead of an empty `503`. Configure it under `overload.page` (`title`, `message`, `lang`, `refresh`, or a custom HTML `file`); `enabled: false` restores the empty body. Flights, prefetches, `fetch()`, API clients and `HEAD` keep the bare status.
- Upgrade note: every public name moved from `harmost` to `tanod`, so an existing deployment needs these updates:
  - Binary, crate and container image: `tanod`, `ghcr.io/akosidencio/tanod`.
  - Config file and paths: `tanod.yaml`, `/etc/tanod/`, `/run/tanod/`; systemd unit `tanod.service`.
  - Environment variables: `HARMOST_*` → `TANOD_*` (for example `TANOD_CONFIG`, `TANOD_PURGE_TOKEN`).
  - Headers: `X-Harmost-*` → `X-Tanod-*`, including `X-Tanod-Cache-Tags`.
  - Metrics: `harmost_*` → `tanod_*`. Dashboards, recording rules and alerts must switch to the new names; history recorded under the old names does not carry over.
  - Next.js adapter: `@harmost/next` → `@tanod/next`, `withHarmost` → `withTanod`, CLI `tanod-next`.

## 0.1.4 — 2026-09-26

- Security: updated `rustls` to 0.23.45 for [RUSTSEC-2026-0285](https://rustsec.org/advisories/RUSTSEC-2026-0285) (TLS 1.3 handshake messages accepted across encryption-level boundaries).
- Fixed: an upstream name that resolves to several instances now spreads origin connections across all of them. Tanod kept only the first address it resolved at startup, so one instance served everything while the rest idled, and a replaced instance stayed unreachable until restart. Names are re-resolved every `origin.resolve_interval` (default `10s`), a failed lookup keeps the previous addresses, `hash_by_path` keeps a path on one instance, and `tanod_upstream_addresses` reports how many each name has.

## 0.1.3 — 2026-09-12

- Added checked-in Next.js route approvals with strict fail-closed validation.
- Added `inspect`, `doctor`, `explain`, and guarded capacity calibration workflows.
- Added staged `observe`, `protect`, `coalesce`, and `cache` rollout generation.
- Added a production-shaped Next.js reference with shared cache state, stable identity, an edge boundary, and expanded CI checks.
- Added Prometheus recording rules and deployment identity artifacts.
- Added conservative replica-group capacity partitioning, URI-stable ingress, and purge fan-out.
- Added standalone setup with generated configuration, automatic config discovery, and a packaged systemd service.

## 0.1.2 — 2026-09-02

- Added the [Next.js adapter](./packages/tanod-next) for build-based configuration, validation, and cache invalidation, with Node and Bun support.
- Added cache tags, authenticated tag/path purges, and cleanup when the deployment changes.
- Added circuit breakers, bounded retries, load-aware balancing, route priorities, and weighted capacity limits.
- Improved cache eviction and fixed Next.js image caching across negotiated formats.
- Expanded resilience and cache metrics, alerts, benchmarks, and configuration checks.
- Evaluated disk and external cache storage; retained the in-memory cache.
- Upgrade note: cache eviction now defaults to `clock`; set `eviction: fifo` to retain the previous policy. Purges and protection budgets remain per process.

## 0.1.1 — 2026-08-29

- Added HTTP/2, optional TLS, WebSockets, trusted-proxy handling, and optional response spooling for slow clients.
- Added health/status endpoints, graceful draining, and Linux socket-handover restarts.
- Added distributed tracing, configuration versioning, richer metrics, dashboards, and alerts.
- Added soak, memory-pressure, restart, chaos, and security checks, plus release artifact automation.
- Fixed cache isolation across hosts and schemes, forwarded-header spoofing, concurrency resizing, and shutdown/readiness behavior.
- Tightened startup validation, reload checks, and build safeguards.
- Deployment notes: configure readiness and per-instance restart paths, allow enough shutdown time, and use a local collector for plaintext-only trace export. Native TLS remains experimental.

## 0.1.0 — 2026-08-27

- Introduced origin concurrency limits, bounded queues, and overload shedding.
- Added bounded in-memory caching, streaming request coalescing, and stale-response support.
- Added upstream health checks, load balancing, safe configuration reloads, metrics, and access logs.
- Added Next.js integration fixtures, browser checks, benchmarks, property tests, and fuzzing.
- Fixed cache-key collisions, private-response isolation, stale revalidation, and streaming coalescing.
