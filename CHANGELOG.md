# Changelog

Notable changes by version. See the [roadmap](./docs/ROADMAP.md) for remaining
work and [operations guide](./docs/OPERATIONS.md) for deployment details.

Tanod was released as Harmost up to 0.1.4. Entries below 0.2.0 use the current
names.

## 0.4.1 - 2026-10-31

**Maintenance release.** No runtime, configuration or CLI changes; a 0.4.0 configuration runs unchanged. See [upgrading](./docs/UPGRADING-0.4.md#from-040-to-041).

- The project moved to the [`tanod-sh`](https://github.com/tanod-sh/tanod) GitHub organization. The image is now published to `ghcr.io/tanod-sh/tanod`; 0.4.0 and earlier remain at `ghcr.io/akosidencio/tanod`. npm package names are unchanged.
- The release workflow no longer announces an image that cannot be pulled anonymously.
- Security (test fixture only): the Next.js fixture moves to 16.4.3 for three critical and high Next.js advisories. Tanod never shipped Next.js; applications on Next.js 16.4.0 or earlier should upgrade Next.js.
- Added a contributing guide, issue and pull request templates, and a CI check for outdated repository links.
- Corrected release status, install examples and the roadmap in the documentation.

## 0.4.0 - 2026-10-30

**Security release.** Fixes four vulnerabilities present in every earlier release, 0.3.2 included. There is no 0.3.3. See the [release notes](./docs/RELEASE-NOTES-0.4.0.md#2-security-fixes) for affected configurations and mitigations, and [upgrading](./docs/UPGRADING-0.4.md) for every behavior change.

- Security: fixed responses shared through `coalesce.override_origin` (which now requires an explicit route `class:`), `cache.query` names matched before decoding, dot-segment paths routed differently from the origin (now `400` in protect mode), and untrusted forwarding headers reaching the origin outside the cache key (new `server.trusted_proxies.host`).
- Origin ceiling: a client disconnect no longer frees capacity while the origin is still rendering. The render finishes and keeps its permit, bounded by `timeouts.origin`; this needs a one-hook patch to the vendored Pingora proxy.
- Performance: one worker thread per CPU by default (`server.threads`), zero-copy cache hits, and less locking on cache fills, refills and large route tables.
- Reliability: safer cache-policy reloads, an uncached fallback under cache memory pressure, accurate in-flight reporting, and new reload and restart metrics with alert examples.
- Releases: the image is smoke-tested before it is pushed and is published by digest, workflow actions are pinned by commit SHA, and `SECURITY.md` is added.
- Updated locked dependencies for rustls-webpki, zstd-sys and tokio advisories.

## 0.3.2 - 2026-10-15

- Added opt-in `origin.load_balancing: consistent_hash` and `cache.eviction: tinyufo`, both built on Pingora; round-robin and CLOCK remain the defaults.
- Health checks reuse Pingora's HTTP client: full responses, any 2xx, whole-attempt timeouts.
- Fixed truncated cache and coalescing responses when global response spooling was enabled.
- Next.js calibration no longer recommends a ceiling when requests fail, and reports warm-up and sustained windows separately.
- Next.js 16.4.0 and 16.3.8 on Node 22 and 24 are now release-gated integration targets.
- Security: updated rustls for four advisories.

## 0.3.1 — 2026-10-11

- `tanod init` and the sample config now use a queue as deep as the concurrency ceiling with a `1s` deadline (was twice the ceiling and `2s`). Existing configs are unchanged; see [sizing the queue](./docs/OPERATIONS.md#sizing-the-queue).
- Added a [deployment guide](./docs/DEPLOYMENT-GUIDE.md): setups per scenario, finding the origin ceiling, and running with an autoscaler.
- Verified `@tanod/next` against Next 16.4.0 and 16.3.8 (same manifest versions as 16.3.3).
- Security (test-only): updated the Next.js fixture and the browser benchmark's Playwright, closing 11 Dependabot alerts.

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
- Fixed: an upstream name that resolves to several instances now spreads load across all of them, and is re-resolved every `origin.resolve_interval` (default `10s`).

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
