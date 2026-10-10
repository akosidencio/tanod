# Tanod: Rust reverse proxy for SSR origin protection

[![CI](https://github.com/akosidencio/tanod/actions/workflows/ci.yml/badge.svg)](https://github.com/akosidencio/tanod/actions/workflows/ci.yml)
[![Version 0.3.1](https://img.shields.io/badge/version-0.3.1-blue)](https://github.com/akosidencio/tanod/releases/tag/v0.3.1)
[![Apache 2.0 license](https://img.shields.io/badge/license-Apache%202.0-green)](./LICENSE)
[![Rust 1.88 or newer](https://img.shields.io/badge/Rust-1.88%2B-orange)](./Cargo.toml)
[![Linux x64](https://img.shields.io/badge/platform-Linux%20x64-informational)](#installation)
[![Pre-1.0](https://img.shields.io/badge/status-pre--1.0-yellow)](#project-status-and-roadmap)

![Tanod reverse proxy: stop traffic spikes from becoming render spikes](./assets/tanod-banner.png)

Tanod is an open-source Rust reverse proxy that protects self-hosted server-side rendering (SSR) and expensive HTTP applications from origin overload. It limits concurrent origin work, bounds waiting requests, and rejects excess work before it overwhelms your application. Safe request coalescing and microcaching reduce repeated work, with a dedicated integration for self-hosted Next.js. Tanod is built on [Cloudflare's Pingora proxy framework](https://github.com/cloudflare/pingora).

**Stop traffic spikes from becoming render spikes.**

> [!IMPORTANT]
> Tanod is pre-1.0. The current source version is **0.3.1**. Repository tests and bounded staging tests support its mechanisms, but sustained production validation and an independent cache-safety review remain open. Configuration and behavior may change between releases.

[Get started](#getting-started) · [Next.js integration](#using-tanod-with-nextjs) · [Standalone server](./docs/STANDALONE.md) · [Operations](./docs/OPERATIONS.md) · [Roadmap](./docs/ROADMAP.md) · [Changelog](./CHANGELOG.md)

## What Tanod is

Tanod sits between incoming traffic and your application server, also called the origin. You configure how much work the origin can handle, and Tanod controls how much reaches it at once.

Its main job is **origin workload control**. A cache helps when requests repeat; concurrency limits still protect the origin when every request targets a different URL or cannot be cached.

Tanod can proxy an HTTP application without a framework-specific build integration. Next.js has the dedicated adapter for request variants, build-derived route policy, and cache invalidation. Other frameworks need reviewed manual route configuration; they do not yet have equivalent integrations.

The name comes from the Tagalog *tanod*: a barangay watchman who keeps order at the gate.

## Why Tanod exists

A server-rendered page can trigger a React render, database queries, and calls to a content management system or another service. A static asset and that page both count as one HTTP request, but they place different demands on the origin.

Product launches, crawler traffic, and application prefetches can increase concurrent work before the origin has capacity to finish it. A cache reduces repeated requests, but a crawler visiting thousands of distinct pages can generate thousands of unique misses. Private pages also need origin capacity even when response sharing is disabled.

Tanod aims to solve three problems:

- **Too much simultaneous work:** global and per-route limits bound concurrent origin requests, with optional weights for expensive routes
- **Too much waiting:** bounded queues and deadlines prevent requests from accumulating indefinitely inside the proxy
- **Unnecessary repeated work:** eligible identical requests share an in-flight response or reuse a short-lived cached response

You choose the capacity limits. Tanod does not automatically measure rendering cost or infer how much CPU or memory each request needs.

## Goals and intended benefits

Tanod aims to keep expensive origins within a measured capacity budget while making overload visible and predictable. Its goals are:

- Keep admitted origin work within configured limits, including traffic that cannot benefit from caching
- Reduce duplicate public renders without sharing personalized responses
- Return a defined overload response when the queue fills or its deadline expires
- Preserve streaming where configured and expose the trade-offs of buffering responses
- Show origin load, queue waits, response status, and reuse through logs and metrics

These are design goals backed by repository checks, not a promise that every deployment becomes faster or cheaper. Limiting concurrency can increase latency when the origin could have handled more work. Adding Tanod also adds a process and a network hop to operate.

## When Tanod fits

Tanod is intended for teams that operate their own application servers and need to control expensive dynamic workloads. A content delivery network (CDN) may already absorb traffic for static or prerendered pages.

| Your workload | Where Tanod can help |
| --- | --- |
| Self-hosted Next.js storefront with public dynamic product pages | Bound rendering and reuse reviewed public responses during bursts |
| Crawler visiting distinct URLs | Limit origin concurrency even when caching and coalescing contribute nothing |
| Personalized pages or private HTTP endpoints | Bound work while keeping response sharing disabled |
| Several origins behind a proxy | Combine capacity limits with health-aware balancing and optional circuit breakers |
| Static or mostly prerendered site | Limited value when a CDN already removes the origin workload |

For a managed hosting platform, first establish whether you control an origin tier where Tanod can run. For a small deployment, compare the added operational work with using a CDN or increasing origin capacity.

Keep an edge proxy, CDN, load balancer, or ingress responsible for the public boundary. Tanod does not replace authentication, a web application firewall, or per-client rate limiting.

## How Tanod works

Tanod checks whether a response can be reused before spending origin capacity on a new request.

![Tanod request flow: classification, safe cache reuse, coalescing, and bounded origin admission](./assets/tanod-flow.svg)

For governed requests, the flow is:

1. Match the route and classify the request, including framework variants and privacy signals.
2. Serve an eligible cache hit or attach to a safely shareable in-flight response.
3. For remaining origin work, acquire the configured route and global capacity permits.
4. Wait only within the configured queue size and deadline when capacity is busy.
5. Forward admitted requests and check origin responses before sharing or storing them.
6. Shed excess requests with the configured error status, `503` by default, and `Retry-After`.

Eligible stale responses can be served during background revalidation or an upstream failure. **An overload shed does not currently fall back to stale content.**

Browser document requests can receive a configurable busy page that refreshes automatically. It does not reserve a queue position; each refresh makes a new request. API requests, Next.js payloads, and prefetches receive the error status without that HTML page.

## Features available today

The current code includes these capabilities; several require explicit configuration:

| Capability | What it does |
| --- | --- |
| Origin admission control | Global and per-route concurrency ceilings, bounded queues, deadlines, route weights, and priorities |
| Request coalescing | Share eligible concurrent requests through one in-flight origin response |
| Microcaching | Store eligible public responses in a bounded in-memory cache with route-level time-to-live (TTL) ceilings |
| Cache invalidation | Authenticated tag and exact-path purges, plus deployment identity in cache keys |
| Next.js integration | Separate HTML and React Server Components (RSC) payloads; handle prefetches, Server Actions, draft mode, and `/_next/*` assets |
| Origin resilience | Health checks, DNS re-resolution, balancing strategies, optional circuit breakers, and budgeted retries |
| Protocol support | HTTP/1.1, configurable HTTP/2, optional native Transport Layer Security (TLS), and opt-in WebSocket upgrades |
| Application supervision | Start and stop one local application through `origin.command` |
| Operations | Config validation, policy reloads, readiness and liveness checks, graceful draining, and Linux listener handover |
| Observability | Structured access logs, Prometheus metrics, traces, and OpenTelemetry Protocol (OTLP) export |

TLS support requires a binary built with the `tls` feature. Tanod does not obtain or renew certificates. WebSocket upgrades use separate limits rather than render permits.

## Getting started

Start by enabling origin protection with response sharing disabled. The generated configuration treats all application routes as private, so you can introduce capacity limits before reviewing which responses are safe to share.

### Build and run locally

Use Linux x64 and a working Rust toolchain. The package declares Rust 1.88 as its minimum; [`rust-toolchain.toml`](./rust-toolchain.toml) pins the toolchain used in this checkout. Run your application on `127.0.0.1:3000` in another terminal before starting the proxy.

Clone and build Tanod from source:

```bash
git clone https://github.com/akosidencio/tanod.git
cd tanod
cargo build --release --locked
```

Generate a separate local configuration so the checked-in example remains available for reference:

```bash
./target/release/tanod init \
  --config tanod.local.yaml \
  --upstream 127.0.0.1:3000 \
  --listen 127.0.0.1:8080 \
  --concurrency 16
./target/release/tanod check --config tanod.local.yaml
./target/release/tanod run --config tanod.local.yaml
```

Send application requests through Tanod and check its admin listener from another terminal:

```bash
curl -i http://127.0.0.1:8080/
curl -fsS http://127.0.0.1:9091/health/live
curl -fsS http://127.0.0.1:9091/status
```

This example admits up to 16 concurrent origin work units and allows up to 16 queued requests, with a one-second queue deadline. A queue as deep as the ceiling costs about one render time when it is full; see [sizing the queue](./docs/OPERATIONS.md#sizing-the-queue). Those values are starting points, not measured capacity for your application. Caching and coalescing are disabled by the private catch-all route.

### Installation

Choose the form that matches your deployment:

| Deployment | Setup |
| --- | --- |
| One Linux server | Follow the [standalone server guide](./docs/STANDALONE.md) for release archives, checksums, systemd, and a domain |
| Separate proxy container | Use the [container build](./Dockerfile) and mount a reviewed configuration |
| Proxy inside your app container | Add [`origin.command`](#run-tanod-inside-your-app) and include the binary alongside the app runtime |
| JavaScript application | Use [`@tanod/linux-x64`](./packages/tanod-linux-x64) for the binary and [`@tanod/next`](./packages/tanod-next) for the integration |
| Build from source with TLS | Run `cargo build --release --locked --features tls` |

The release workflow targets Linux x64 with GNU and static musl archives, a `linux/amd64` image at `ghcr.io/akosidencio/tanod`, and npm packages. Select an available version from [GitHub releases](https://github.com/akosidencio/tanod/releases). There are no macOS, Windows, or Linux ARM64 release artifacts in the current workflow.

The container runs on `scratch` and has no shell or application runtime. Use your application's image when supervising Node, Bun, or another server inside the same container.

## Deploying Tanod

Place Tanod behind the public edge and keep your origin reachable only through the intended proxy path.

```text
Client → CDN / Caddy / NGINX / load balancer → Tanod → application
```

The edge can terminate TLS, manage certificates, and apply client rate limits. Keep Tanod's admin and metrics listeners on loopback or a private network. Configure `server.trusted_proxies` for the actual edge addresses before relying on forwarded client or scheme headers.

### Choosing a topology

Both an app-local proxy and a separate proxy tier are supported deployment patterns, with different capacity and cache boundaries.

| Pattern | Capacity boundary | Cache and invalidation |
| --- | --- | --- |
| One Tanod per app instance | Each Tanod protects that instance's measured capacity | Reuse stays local; invalidate every instance that can hold the response |
| One Tanod in front of several origins | Its global limit covers the origin pool it protects | One process owns the Tanod cache and purge endpoint |
| Several Tanod replicas sharing an origin pool | Statically partition the total budget across the declared replicas | Cache and coalescing stay local; purges must reach every replica |

Copying the full origin concurrency budget to every proxy replica multiplies the allowed work. Use `capacity` and reviewed replica allocations, or the Next.js generator's `--global-concurrency` and `--replicas` options. The [Next.js production reference](./docs/NEXTJS-PRODUCTION-REFERENCE.md) includes path-stable ingress and purge fan-out examples. The [deployment guide](./docs/DEPLOYMENT-GUIDE.md) recommends a setup for each scenario, including Kubernetes with autoscaling, and explains how to measure the ceiling.

### Run Tanod inside your app

Tanod 0.3.0 can supervise your application directly. In a configuration generated by `tanod init`, update the existing `origin` block with your server command while retaining its concurrency settings:

```yaml
origin:
  upstreams: ["127.0.0.1:3000"]
  command:
    args: ["node", "server.js"]
    ready_timeout: 60s
    stop_timeout: 10s
```

This example uses Next.js standalone output. The command runs directly, without a shell, and receives `PORT=3000` and `HOSTNAME=127.0.0.1` from the upstream address. Other HTTP servers can use their own command and binding settings.

Start the process tree with `tanod run`. Tanod waits for the origin to accept connections before serving traffic. On `SIGTERM`, it drains its own requests before stopping the application. If the application exits unexpectedly, Tanod exits with its status so the platform can restart the deployment.

A supervised application requires one loopback upstream. Command changes require a restart, and this mode refuses `--daemon` and `--upgrade`. In containers, use a small init such as `tini` or `dumb-init` to reap descendants the app may leave behind.

For setups that use a Node or Bun runner, [`tanod-next start`](./packages/tanod-next/README.md#running-the-app-and-tanod-together) offers another process-tree launcher.

## Using Tanod with Next.js

The `@tanod/next` package reads your production build to generate route policy and coordinates Tanod invalidation with Next.js invalidation. It supports Node and Bun; its declared Next.js peer range starts at version 14.

Install the build integration in your Next.js project and generate policy after building:

```bash
npm install --save-dev @tanod/next
npm run build
npx tanod-next generate \
  --upstream 127.0.0.1:3000 \
  --concurrency 16 \
  --out tanod.yaml \
  --check
```

The `--check` flag requires a Tanod binary discoverable by the adapter. It validates the generated configuration before replacing the previous output.

The generator uses build manifests rather than guessing whether a dynamic page is public. Prerendered routes can be marked shareable; dynamic pages and Route Handlers remain private unless you approve them in a checked-in policy file. Regenerate after each build so route policy and deployment identity match your application.

Review the [adapter documentation](./packages/tanod-next/README.md) for public-route approvals, staged rollout generation, `inspect`, `explain`, `doctor`, and capacity calibration. Use a regular dependency instead of a dev dependency when the deployed app needs the package's runner or invalidation helpers at runtime.

Calling Next.js `revalidateTag()` or `revalidatePath()` alone does not invalidate Tanod's cache. Use the adapter's invalidation integration and send purges to every Tanod instance that can hold a copy. Path purges match exact paths; dynamic route-pattern invalidation is not implemented.

## Configuration and cache safety

Tanod uses versioned YAML configuration. The [annotated configuration example](./tanod.yaml) documents the available settings, while the [schema guide](./docs/CONFIG-SCHEMA.md) explains compatibility and validation rules.

Validate changes before starting or reloading the proxy:

```bash
tanod check --config tanod.yaml
```

When no file is passed, `tanod check` and `tanod run` use `TANOD_CONFIG`, then search for `tanod.yaml`, `tanod.yml`, and their `/etc/tanod/` equivalents. Unknown keys and unsupported options are errors rather than ignored settings. `${NAME}` reads an environment value, and `${NAME:-default}` supplies a fallback.

### Enable reuse only for reviewed public routes

Response sharing requires both a suitable route policy and a safe origin response. Cache hits and coalescing waiters avoid origin admission; private or unshareable requests still pass through the capacity controls.

For an audited public page, insert a route like this **above** the generated `default-private` catch-all:

```yaml
  - id: public-products
    match: "/products/**"
    class: public_ssr
    cache:
      enabled: true
      override_origin: true
      ttl:
        max: 2s
    coalesce:
      enabled: true
```

This is a `routes` list item, not a complete configuration. The override allows short-lived caching even when the origin sends `private`, `no-cache`, or `no-store`. Use it only when those pages are the same for every visitor, including visitors with cookies. For Next.js, prefer checked-in route approvals through the adapter.

The sharing rules include:

- Requests with `Authorization`, unsafe methods, Next.js Server Actions, and draft-mode content bypass sharing
- Cookie-bearing requests remain private by default; an explicit public-route override requires a privacy review
- Responses with `Set-Cookie`, unsupported `Vary` headers, or unsafe statuses cannot be shared through an override
- Origin privacy and cache directives remain authoritative unless a validated public route explicitly overrides them
- Cache keys separate scheme, host, method, path, query, configured variants, Next.js variants, and deployment identity

The [threat model](./docs/THREAT-MODEL.md) documents these boundaries. They do not remove the need to review your own application policy.

### Slow readers and render capacity

A slow client can delay the proxy's observation that the origin response has finished, keeping a capacity permit occupied. Optional response spooling buffers the response within per-body and global memory limits so capacity can be released earlier.

Spooling delays delivery until buffering completes and sacrifices progressive rendering. It is disabled by default, is rejected on routes declared `streaming`, and falls back to streaming when its budget is exceeded. Review [`spool` settings](./tanod.yaml) and the [spool benchmark](./bench/spool.sh) before enabling it.

## Operating Tanod

The [operations guide](./docs/OPERATIONS.md) covers readiness, drain periods, reloads, restarts, systemd, Kubernetes, purging, and alerting.

The generated local setup exposes these admin endpoints on `127.0.0.1:9091`:

| Endpoint | Purpose |
| --- | --- |
| `GET /health/live` | Check whether the Tanod process is alive |
| `GET /health/ready` | Decide whether this instance should receive traffic; returns `503` during draining |
| `GET /status` | Inspect configuration identity, backend health, cache occupancy, and admission state |

Readiness can also require a healthy origin when configured. Keep liveness independent of origin health so an upstream outage does not trigger proxy restart loops.

`SIGHUP` applies reloadable policy changes and retains the running policy if validation fails. Listener and supervised-command changes need a restart. Allow the supervisor's stop timeout to cover the drain period, request shutdown window, and any supervised-origin stop timeout.

### Metrics and dashboards

Prometheus and OTLP exports report how the proxy affects origin work and visitor responses. The repository includes [alerting and recording rules](./ops/prometheus), a [Grafana dashboard](./ops/grafana/dashboard.json), and [import instructions](./ops/README.md).

Watch these signals when tuning capacity:

- Origin in-flight work compared with its configured concurrency limit
- Queue wait and total request duration
- Shed requests and response status by source
- Origin latency and backend health
- Cache reuse, memory occupancy, and drain state

For a local demonstration with three Next.js origins, generated traffic, Prometheus, and Grafana, run:

```bash
docker compose -f compose.observability.yaml up --build
```

Open the [local Grafana dashboard](http://127.0.0.1:13000/d/tanod-overview/tanod). Application traffic enters at `http://127.0.0.1:18080`, and Prometheus is available at `http://127.0.0.1:19000`.

![Tanod Grafana dashboard showing origin load, admission, latency, queues, and cache reuse](./assets/tanod-dashboard.png)

Stop the demonstration with:

```bash
docker compose -f compose.observability.yaml down
```

## Tests and benchmark evidence

The repository checks admission, sharing safety, streaming, protocols, lifecycle behavior, and framework integration. The [CI workflow](./.github/workflows/ci.yml) and [release gates](./docs/RELEASE-GATES.md) define the checks and their limits.

Run the workspace tests from the repository:

```bash
cargo test --workspace --all-targets --locked
cargo test --workspace --doc --locked
```

The following scripts start fixtures, make assertions, and tear down their processes. Next.js integration scripts require Docker; the browser checks also use Chromium through Playwright.

| Check | Command | What it verifies |
| --- | --- | --- |
| Distinct, uncacheable URLs | `./bench/demo.sh 60 1000` | Origin peak stays within the configured ceiling even without reuse |
| Duplicate public requests | `./bench/coalesce.sh 100` | Concurrent requests collapse into a bounded number of origin renders |
| Streaming reuse | `./bench/stream.sh 20 5 400` | Eligible waiters receive data while one origin response is produced |
| Private-response isolation | `./bench/safety.sh 50` | Responses setting cookies remain separate |
| Supervised application and shutdown | `./bench/supervise.sh` | Application lifecycle and in-flight request handling |
| Next.js HTTP behavior | `./bench/nextjs.sh` | Real framework responses across multiple origins |
| Next.js browser behavior | `./bench/nextjs-browser.sh` | Client-generated navigation, prefetch, and Server Action requests |
| Replica behavior | `./bench/nextjs-replicas.sh` | Combined budgets, affinity, purge fan-out, failure, and recovery |

The local slow-origin fixture sleeps instead of performing a real render. Its results demonstrate mechanisms, not universal throughput or latency. `BENCH_REPORT_DIR` collects supported benchmark results with their workload parameters.

The repository's recorded DigitalOcean staging A/B test used one repeated public SSR product URL. A spike at 20 requests per second lasted 20 seconds, with baseline and recovery periods. Across each complete run, it reported 501 successful requests through Tanod versus 60 directly, with successful-response p95 latency of 166 ms versus 9.10 seconds.

The direct run also recorded timeouts and dropped load-generator iterations, so the latency figures describe successful responses rather than the entire offered workload. Tanod ran on a separate instance; these results do not establish lower aggregate resource use.

That result is a workload-specific observation, not a general speed claim. Separate uncached staging traffic overloaded an origin when its configured ceiling was too high. Measure representative rendering, memory use, and latency before relying on your own limits.

## Project status and roadmap

Tanod 0.3.1 implements the core proxy, admission controls, local cache, request coalescing, telemetry, and application supervision. The [changelog](./CHANGELOG.md) records release history; the [roadmap](./docs/ROADMAP.md) tracks remaining work.

Current boundaries and validation gaps include:

- Sustained production validation and independent cache-key/shareability review remain open
- Cache and coalescing are local to each process; there is no distributed Tanod cache
- Replica capacity uses static partitioning rather than adaptive limits or shared capacity leases
- Every cache-holding instance must receive invalidation; path purges do not expand dynamic route patterns
- Overload shedding does not serve stale content, even when an eligible older response exists
- Slow readers can retain capacity unless spooling succeeds within its limits
- Routes classified as static or long-lived streaming are exempt from render permits; WebSocket upgrades have separate limits
- Additional framework integrations, Linux ARM64 artifacts, and package-manager installation remain roadmap work
- Clean-server distribution and published-artifact rollback exercises still need validation

Tanod was previously named Harmost through version 0.1.4. The 0.2.0 rename changed binary names, config paths, environment variables, headers, metrics, and npm package names. Read the [upgrade notes](./CHANGELOG.md#020--2026-10-04) before migrating an older deployment.

## Documentation

Use these guides for the next step:

| Guide | What it covers |
| --- | --- |
| [Standalone server](./docs/STANDALONE.md) | Release binary, systemd, local origin, and public domain |
| [Deployment guide](./docs/DEPLOYMENT-GUIDE.md) | Choosing a setup, finding the origin ceiling, autoscaling, and recommended configs |
| [Operations](./docs/OPERATIONS.md) | Health, deployment, reload, restart, purge, and monitoring |
| [Annotated configuration](./tanod.yaml) | Listeners, origin budgets, routes, cache, resilience, and telemetry |
| [Configuration schema](./docs/CONFIG-SCHEMA.md) | Version compatibility, migration rules, and rejected combinations |
| [Next.js adapter](./packages/tanod-next/README.md) | Build policy, runtime launcher, route approvals, and invalidation |
| [Next.js production reference](./docs/NEXTJS-PRODUCTION-REFERENCE.md) | Deployment identity, shared Next.js state, replica limits, and verification |
| [Threat model](./docs/THREAT-MODEL.md) | Protected assets, security assumptions, controls, and exclusions |
| [Cache-key review brief](./docs/CACHE-KEY-REVIEW.md) | Claims and attack cases for an independent cache-safety review |
| [Release gates](./docs/RELEASE-GATES.md) | Required checks and validation beyond CI |
| [Roadmap](./docs/ROADMAP.md) | Remaining features, limitations, and validation work |
| [Changelog](./CHANGELOG.md) | Release history and upgrade notes |

## Contributing and security

Report reproducible bugs or propose improvements through [GitHub issues](https://github.com/akosidencio/tanod/issues). Include the Tanod version, a redacted configuration, the request pattern, and the observed behavior. Review the relevant [release gates](./docs/RELEASE-GATES.md) when contributing code.

An independent review of cache keys and response shareability is a priority. The [review brief](./docs/CACHE-KEY-REVIEW.md) describes falsifiable claims and known review gaps. Tanod's threat model has not been independently audited; keep credentials and private application data out of public reports.

## License

Tanod is licensed under the [Apache License 2.0](./LICENSE).
