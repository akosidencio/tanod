# Changelog

Notable changes by version. See the [roadmap](./docs/ROADMAP.md) for remaining
work and [operations guide](./docs/OPERATIONS.md) for deployment details.

Tanod was released as Harmost up to 0.1.4. Entries below 0.2.0 use the current
names.

## 0.3.1 — 2026-10-11

- Changed `tanod init` to generate a queue as deep as the concurrency ceiling with a `1s` deadline, down from twice the ceiling and `2s`. Under sustained overload a deep queue never empties and adds its whole wait to every admitted request without adding throughput. Existing configs are unchanged.
- Resized the sample `tanod.yaml` queues the same way.
- Added [sizing the queue](./docs/OPERATIONS.md#sizing-the-queue) to the operations guide: the wait estimate, where it stops holding, and the metrics that show a standing queue.
- Added a [deployment guide](./docs/DEPLOYMENT-GUIDE.md): where Tanod sits beside a CDN and the framework cache, a recommended setup per scenario, finding the origin ceiling, and running with an autoscaler (HPA or KEDA) without the two blocking each other.
- Verified `@tanod/next` against Next 16.4.0 and 16.3.8 (same manifest versions as 16.3.3).
- Security: moved the Next.js storefront fixture to Next 16.4.0 (pulling in sharp 0.35.5 and source-map-js 1.2.2) and the browser bench to Playwright 1.55.1, closing all 11 Dependabot alerts. Both are test-only; nothing Tanod ships depended on the affected versions.

## 0.3.0 — 2026-10-08

- Added `origin.command`: Tanod starts and supervises the app server, so one container or systemd unit runs both with no runner process, for any framework.
- Fixed graceful shutdown cancelling in-flight requests the moment the drain ended; they now get `server.graceful.shutdown_timeout` to finish.
- Fixed a crash at config load when a quoted value had a backslash before a non-ASCII character.
- Upgraded to Pingora 0.9 (HTTP request-smuggling hardening, bounded HTTP/2 limits, hop-by-hop header sanitizing) and `prometheus` 0.14, which removes three ignored advisories and the `aws-lc` C dependency.

## 0.2.0 — 2026-10-04

- Renamed the project from Harmost to Tanod, continuing from Harmost 0.1.4.
- Added a configurable overload page for browser page loads that are shed; other requests keep the bare `503`.
- Added visitor metrics for responses by status and source, request duration, queue wait, and build info, with dashboard panels and recording rules.
- Fixed background cache revalidations being counted as visitor requests.
- Added `tanod-next start` to run the app and Tanod as one process tree, with the binary shipped through npm as `@tanod/linux-x64` (static, glibc and Alpine); any HTTP server can be the origin.
- Added `${VAR}` environment references in the config, for secrets and per-environment values.
- Added OTLP metric push (`telemetry.metrics.otlp`) and `https://` with custom headers for OTLP export in TLS builds, so no collector is needed beside Tanod.
- Added a static musl binary to releases, and npm publishing of `@tanod/next` and `@tanod/linux-x64`; both share Tanod's version.
- Shrank the container image from 155 MB to about 22 MB (6 MB compressed): the static binary on `scratch`, with no shell. Kubernetes `preStop` hooks that exec a shell need the built-in `sleep` action instead.
- Upgrade note: every `harmost` name is now `tanod` — binary, image, config paths, systemd unit, `TANOD_*` environment variables, `X-Tanod-*` headers, `tanod_*` metrics, and `@tanod/next`. Update dashboards and alerts; metric history does not carry over.

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
