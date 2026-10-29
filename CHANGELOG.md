# Changelog

Notable changes by version. See the [roadmap](./docs/ROADMAP.md) for remaining
work and [operations guide](./docs/OPERATIONS.md) for deployment details.

Tanod was released as Harmost up to 0.1.4. Entries below 0.2.0 use the current
names.

## 0.4.0 - Unreleased

- Fix: a client disconnect no longer frees origin capacity while the origin is still working. Pingora abandoned an uncached response (including one the cache declined under memory pressure) when its client went away, Tanod released the permit, and a queued request was admitted beside the render still running; a burst of disconnects against a four-unit ceiling reached 13 concurrent renders. Tanod now reads such a response to its end, discards it and holds the permit until the origin finishes, bounded by `timeouts.origin`, as Pingora already did for cache fills. A drained exchange is never retried. **A behaviour change:** an origin is no longer signalled to stop by a closed connection when a client leaves an admitted request; it finishes the work it was given. Adds `tanod_origin_disconnect_drains_active`, `tanod_origin_disconnect_drains_total{outcome="completed"|"deadline"|"upstream_error"}`, access-log `permit_released` values `drain_end`/`drain_deadline`/`drain_error`, and the `TanodOriginWorkOutlivedDeadline` alert. See [what the origin ceiling guarantees](./docs/OPERATIONS.md#what-the-origin-ceiling-guarantees).
- Build: `pingora-proxy` 0.9.0 is vendored with one added hook (`vendor/pingora-proxy/tanod.patch`) and selected by `[patch.crates-io]`; `scripts/check-vendored-pingora.sh` verifies it against the published crate's checksum in CI.
- Benchmarks: `slow-origin` decrements its render count before the last byte of a response instead of after the final write, so a proxy reading that byte can never find the finished render still counted; its counter is now held by an RAII guard, and `SLOW_ORIGIN_EVENTS=<file>` records render starts and ends. Add `bench/origin-concurrency.py`, deterministic and `--stress` origin-lifetime scenarios against an exact in-process witness, to `bench/all.sh`. `bench/soak.sh` records cache-pressure and drain outcomes and fails if a drain is still active after traffic stops.

- Hardening: late writers from retired cache-policy namespaces now reject new follower attachment while preserving the reserved leader. Deterministic lifecycle schedules and a Loom completion/lookup/purge model cover both retention policies.
- Operations: publish accepted/refused reload counters and a process-start timestamp; add alert examples for reload refusal, restart churn, cache pressure and upstream failures. Purge credentials use `subtle`'s constant-time comparison.
- Production: add validated VPS, Compose, Kubernetes and trusted-proxy examples, hardened-container/CPU-quota checks, shutdown regressions, bounded fuzz runners and extended nightly/release stability gates. No release is published by these changes; mandatory release evidence remains subject to the engineering reports.
- Benchmark evidence now includes binary build-input manifests and per-invocation environments. Add configurable body-size/fill concurrency sweeps and a reproducible Phase 2 comparison runner.

- Performance: the proxy now runs one worker thread per available CPU instead of Pingora's default of one thread for everything. New `server.threads` overrides it; **a behaviour change** — set `threads: 1` to restore the old single-core proxy. The count honours CPU affinity and cgroup quotas, is printed at startup, applies to the proxy listeners only, and needs a restart to change.
- Performance: cache hits no longer copy the body. Completed entries are immutable segments that every hit, the leader and coalesced followers share by reference; fills copy each write once into an exact allocation and compact many small writes into 64 KiB segments on completion. Large hits got much cheaper (in-process, 1 MiB hits went from about 14,000 to 220,000 per second on one thread) and a follower copies at most the unsealed 4 KiB tail.
- Performance: completing a fill no longer stops every cache lookup. Completion takes the read side of the lifecycle lock and admits the entry before retiring the fill, so a concurrent lookup finds one or the other; purge and policy retirement remain excluded for the whole transition. A reservation that fits the budget takes the accounting lock alone instead of every cache map lock.
- Performance: refilling a stored key under CLOCK or FIFO no longer scans the eviction queue. Replaced and purged entries' records are retired lazily and compacted when they outnumber live entries; victim order is unchanged. A refill with 100,000 entries cached took about 2.2 ms under every cache write lock.
- Performance: route tables of 16 routes or more resolve through one `GlobSet` that proposes candidate routes, each still confirmed by its own matcher in file order; the winning route is unchanged (differential property test against the linear scan).
- Add performance tooling: `bench/perf.sh` scenarios with a workspace load generator (`bench/loadgen`), interleaved A/B comparison (`bench/perf-ab.sh`, `perf-compare.py`), in-process microbenchmarks (`cargo bench --bench hotpath`), and per-request allocation counts under DHAT (`bench/alloc.sh`). `slow-origin` gains `.../size/<bytes>` bodies and an optional `SLOW_ORIGIN_SLOTS` render capacity.

- Reject new attachments to invalidated streaming fills through both Pingora lookup paths, while preserving existing readers and preventing late admission. Selective purges now cover transient coalescing responses as well.
- Isolate incompatible cache-sharing policies across hot reloads with process-local namespaces and exact policy comparison. Reclaim obsolete entries and prevent late old-generation fills from being retained; compatible operational reloads preserve cache hits.
- Add coordinated CLOCK/TinyUFO regressions for purge, cancellation, replacement, reload, streaming followers and memory accounting.
- Security: stop untrusted forwarding headers from shaping origin URLs. Tanod now restates `X-Forwarded-Host`, `X-Forwarded-Port` and `X-Real-IP` along with `X-Forwarded-For`/`-Proto` on every request, and removes `X-Forwarded-Prefix`, `X-Original-Host`, `X-Original-URL`, `X-Rewrite-URL`, the scheme aliases and (from untrusted peers) CDN client-IP headers. Previously a client's `X-Forwarded-Host` reached the origin unkeyed, so a cacheable page built from it was served to later visitors, and it satisfied Next.js's Server Action origin check.
- Take the forwarded scheme from the same hop as the client address (or from a single overwritten value), never the leftmost of several entries, so a client cannot prefix one to a chain a trusted proxy appends to. RFC 7239 `proto` now comes from the chosen element.
- Add `server.trusted_proxies.host` (default `none`) to believe a trusted proxy's public host. A believed host is validated and enters the cache key. **Upgrade note:** deployments whose proxy rewrites `Host` and relied on `X-Forwarded-Host` passing through must enable it, or Next.js Server Actions fail its origin check.
- Report actual in-flight work. `in_flight` in `/status` and `tanod_origin_in_flight` are counted at the permits, in weighted units, instead of derived as `limit - available`, which reported the new ceiling rather than the running work after a lowered limit. Both can now exceed the limit while it drains. Reloads republish every limiter gauge and withdraw retired route series; a request pinned to an older policy no longer re-creates a retired route limiter.
- Keep cache memory exhaustion from failing good responses. A fill that cannot register is served uncached; a fill that runs out mid-body is abandoned and finishes streaming to the client and its coalesced followers from a headroom of one `cache.max_body_size`, without being stored; only past that headroom is a response aborted, visibly. Reservations that cannot fit beside in-progress fills no longer evict the whole cache first. Adds `tanod_cache_memory_pressure_total{outcome}`, `cache.pinned_bytes`, `cache.headroom_bytes` and `cache.memory_pressure` in `/status`, and the `TanodCacheResponsesAborted` alert. Size memory limits for `cache.max_memory + cache.max_body_size`.
- Fail an oversized streamed cacheable response once rather than every time: a chunked body that outgrows `cache.max_body_size` is still aborted by Pingora, but the URL is remembered and later requests bypass the cache.
- Security: updated the locked rustls-webpki to 0.103.16 (RUSTSEC-2026-0382), zstd-sys to 2.1.2 (RUSTSEC-2026-0410) and tokio to 1.53.3 (RUSTSEC-2026-0386). Tokio is held at `~1.53.3`: 1.54 rejects the zero-capacity stream buffers Pingora 0.9 creates and the proxy panics on its first connection. zstd 0.13 / zstd-safe 7.2 advisories are documented exceptions in `.cargo/audit.toml` until Pingora moves off them; none of the affected APIs is reachable from Tanod.
- Add `bench/cache-pressure.sh` and `bench/nextjs-forwarded.sh` (real Next.js server), and extend `bench/forwarded.sh` with origin-facing headers, cache poisoning, malformed values, trusted-host and IPv6 cases.

## 0.3.2 - 2026-10-15

- Reused Pingora's round-robin algorithm and HTTP health-check client. Health probes validate complete HTTP responses, accept all 2xx statuses, enforce the whole-attempt timeout, and cancel promptly on shutdown. Configured upstream health thresholds, circuit breakers, load-aware scoring, and admission remain Tanod policies.
- Added opt-in `origin.load_balancing: consistent_hash`, using Pingora's Ketama selector across resolved instances. DNS changes rebuild selector snapshots off proxy workers. Existing `hash_by_path` mappings and the default round-robin strategy are preserved.
- Added opt-in `cache.eviction: tinyufo`, using Pingora TinyUFO inside Tanod's bounded streaming store. CLOCK remains the default. Streaming reservations, coalesced follower handoff, metadata revalidation, expiry, and tag/path/deployment purges remain supported; changes to the policy require restarting Tanod.
- Fixed truncated cache/coalescing responses with global response spooling enabled under Pingora 0.9. The extra spool now applies only to requests bypassing cache and coalescing. Strengthened the memory gate to check cached response integrity, require actual spool outcomes, and enforce the exact cache budget.
- Added `tanod_cache_admission_rejected_total` and `cache.admission_rejected` in `/status`, plus TinyUFO lifecycle gates and a comparison of the actual streaming store with CLOCK, FIFO, and the completed-entry reference. See [Pingora reuse findings](./docs/PINGORA-REUSE.md).

- Security: updated the locked Rustls dependency to 0.23.46, fixing RUSTSEC-2026-0346, RUSTSEC-2026-0347, RUSTSEC-2026-0348, and RUSTSEC-2026-0349. Added a documented exception for Pingora's unmaintained unsafe-libyaml dependency (RUSTSEC-2024-0449); Tanod uses serde-saphyr for its configuration and does not call Pingora's YAML configuration parser.
- Added a shared compatibility declaration for Next.js 16.4.0 and 16.3.8 on Node 22 and 24. Every pair is a release gate covering a real build, generated policy validation, HTTP, browser navigation/prefetch, Server Actions, invalidation, and replica recovery, with CI logs attached per pair. Adapter tests cover both Node lines and pinned Bun 1.4.0.
- Clarified that npm peer/engine installation ranges and historical manifest-only verification do not promise full framework integration coverage.
- Fixed Next.js capacity calibration recommending a ceiling when every request fails. Missing or undersampled healthy baselines now return a null recommendation and a nonzero CLI exit status.
- Added separate warm-up and sustained measurement windows, representative route mixes, an optional p95 latency target, and typed calibration reports. Calibration stops increasing load at the first unhealthy step and distinguishes a measured healthy limit from unobserved saturation.
- Calibration steps must start at 1 and strictly increase. The default measurement is now 10 seconds per step after a 2-second warm-up, with at least 20 samples required; use `--requests-per-worker` for request-count experiments. Report consumers must handle nullable `observedKnee` and `recommendedCeiling` values.

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
