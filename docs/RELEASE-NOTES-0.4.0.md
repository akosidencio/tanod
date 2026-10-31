# Tanod v0.4.0

**Draft for an unpublished release.** Nothing here is published until the
maintainer approves publication; see the [release gates](RELEASE-GATES.md) for
what is still open. Read the [upgrade guide](UPGRADING-0.4.md) before
deploying: 0.4.0 changes defaults and refuses some configurations 0.3.2
accepted.

## 1. Overview

Tanod is a reverse proxy, built on Cloudflare Pingora, that protects a
server-rendered origin: it limits how much render work reaches the origin at
once, collapses identical concurrent requests, and serves short-lived cached
copies where the route and the origin allow it.

0.4.0 is a security, correctness, performance and production-readiness
release. **It fixes four vulnerabilities present in every earlier release,
0.3.2 included.** There is no 0.3.3: every fix ships here. If you run 0.3.2,
read [§2](#2-security-fixes) and [§12](#12-upgrading-from-v032) first.

What Tanod guarantees, and what it does not:

- **It bounds proxy-managed origin exchanges.** A request holds its origin
  permit from admission until the origin's response ends, the origin fails,
  or `timeouts.origin` passes. A client disconnecting does not end it early.
- **The origin may keep working after its HTTP response.** Work an
  application does after its response ended (`after()` callbacks, background
  revalidation), and work past the drain deadline, are invisible to any
  proxy. If your application must never exceed a number of concurrent
  renders, enforce that in the origin too, with a limit sized to Tanod's.
- **Cache overrides are an operator contract.** A route you declare public
  is shared between visitors when an override is set. Tanod cannot detect an
  origin that personalises a response without saying so.
- **Replica capacity is a static partition.** Each process enforces its own
  ceiling; replicas do not coordinate.
- **Cache memory limits are not RSS limits.** Budget container memory with
  margin above `cache.max_memory`.

## 2. Security fixes

Four vulnerabilities, each with its own root cause. **All four affect every
published release from 0.2.0 to 0.3.2** (GitHub release binaries, the
`ghcr.io/akosidencio/tanod` image, and `@tanod/linux-x64` / `@tanod/next` on
npm). Security advisories will be published with this release; see
[§14](#14-security-disclosure).

| | Issue | Affected configurations | Impact |
|---|---|---|---|
| A | `coalesce.override_origin` could persist a response the origin refused to share | Routes with `coalesce.override_origin` and the route cache enabled (the default), whose origin sends `private` or `no-store` with a positive `max-age`, or a cacheable response to a cookie-bearing request | One visitor's page served to others for the origin's `max-age` |
| B | `cache.query` include/exclude lists matched parameter names before decoding | Cached routes with a `cache.query` list (including `@tanod/next` policies with declared query keys) | Attacker-chosen content stored under a URL other visitors request |
| C | Paths with `.`/`..` segments were routed and classed as sent while the origin resolved them | Protect mode with route classes or cache rules that differ by path prefix (every `@tanod/next` policy classes `/_next/static/**` as static) | Origin concurrency ceiling bypassed by a non-browser client; route-level cache rules applied to the wrong page |
| D | Client-supplied forwarding headers (`X-Forwarded-Host` and others) reached the origin unchanged and outside the cache key | Any deployment where clients can send these headers to Tanod; impact needs an origin that reads them (absolute URLs, redirects, client address) | A cached page carrying an attacker-chosen host served to other visitors |

What changed:

- **A.** A response that only `coalesce.override_origin` let past the origin's
  refusal is shared within its one flight and never stored, and a
  cookie-bearing request admitted by that option may coalesce but not store.
  The option now requires an explicit route `class:`.
- **B.** Query policies compare parameter names the way origins decode them
  (`%71` is `q`, `+` is a space). The cache key keeps the bytes as sent, so
  different spellings stay separate entries.
- **C.** In protect mode, a request target with a `.` or `..` path segment,
  literal or `%2e`-encoded, is answered `400` before routing and never reaches
  the origin, over HTTP/1.1 and HTTP/2.
- **D.** Tanod restates or removes forwarding headers on every request; see
  [§8](#8-forwarded-header-security).

### Mitigations for 0.3.2 until you upgrade

These reduce exposure. **None is equivalent to running 0.4.0.**

- **A:** add `cache: {enabled: false}` to every route that sets
  `coalesce.override_origin` (in-flight coalescing keeps working; nothing is
  stored), or remove the option from routes whose origin can send `private`
  or `no-store` with a positive `max-age`.
- **B:** remove `cache.query` include/exclude lists from cached routes (every
  parameter then enters the key, which is safe), or reject parameter names
  containing `%` or `+` at the edge.
- **C:** have the edge reject or normalise request targets with `.`/`..`
  segments, `%2e` forms included. Many proxies forward the original target
  unchanged, so check yours with `curl --path-as-is`. Keep an origin-side
  concurrency limit.
- **D:** have the edge overwrite `X-Forwarded-Host`, `X-Forwarded-Port` and
  `X-Real-IP`, and strip `X-Forwarded-Prefix`, `X-Original-Host`,
  `X-Original-URL` and `X-Rewrite-URL`; or make the origin build absolute URLs
  from a fixed public origin.

The cache and sharing review behind A–C is a maintainer-side review,
[published for an independent reviewer](reviews/CACHE-SECURITY-REVIEW-v0.4.0.md).
**It is not an independent audit**, and that review is still outstanding.

## 3. Cache correctness

- Keys separate scheme, host, method, path, query, configured and Next.js
  variants (HTML, RSC, prefetch), content coding and deployment identity.
- Responses that set cookies, vary on headers outside the key, or answer
  `Authorization`, draft-mode or Server Action requests are never shared.
- A purge closes in-flight fills (including the short transient handoff) to
  new readers; existing readers finish.
- `SIGHUP` reloads that change a sharing rule retire the cache namespace, so a
  response admitted under the old rules cannot reach a request under the new
  ones. Operational reloads keep the cache.
- An oversized streamed cacheable response fails once, then that URL bypasses
  the cache.

## 4. Request coalescing

Concurrent identical misses share one render, and followers receive the
leader's chunks as they are produced. Followers attach only after the origin's
response headers were judged shareable. `coalesce.override_origin` now
behaves as documented: in-flight sharing on routes you declare public, never
storage (§2 A).

## 5. Origin concurrency guarantees

- An admitted request whose client disconnects keeps its permit while Tanod
  reads the origin's response to its end and discards it, bounded by
  `timeouts.origin`. Before 0.4.0 a disconnect freed the permit mid-render: a
  burst of disconnects against a ceiling of 4 reached 13 concurrent renders.
- A client cut off at `timeouts.downstream_write` is drained the same way.
- An origin that fails while its client is still connected reaches that
  client as a 502 or a truncated body and counts against the backend's
  breaker; a drained exchange is never retried.
- The drain requires a reviewed one-hook patch to Pingora's proxy crate,
  vendored in `vendor/pingora-proxy` and checked against the published
  crate's checksum in CI (`scripts/check-vendored-pingora.sh`).
- `/status` and `tanod_origin_in_flight` report the work actually held, in
  weighted units, which can exceed a newly lowered limit until it drains.

**Behaviour change:** the origin is no longer signalled to stop by a closed
connection when a client leaves an admitted request; it finishes the work.

## 6. Performance improvements

- The proxy uses one worker thread per available CPU, respecting affinity and
  cgroup quotas, instead of a single thread (`server.threads` overrides it).
- Cache hits share stored bodies without copying (1 MiB hits went from about
  14,000 to 220,000 per second on one thread, in-process).
- Completing a fill no longer blocks every lookup; refilling a stored key no
  longer scans the eviction queue; large route tables resolve through one
  `GlobSet`.

Throughput and latency figures are measurements on one machine, published with
their parameters, not guarantees.

## 7. Memory-pressure behaviour

Registration falls back to serving uncached; an in-progress fill finishes
unstored within one `cache.max_body_size` of headroom; only past that is a
response aborted, visibly. Logical cache bytes are not an RSS ceiling:
detached readers, indexes, stacks and buffers also use memory. Size memory
limits for at least `cache.max_memory + cache.max_body_size` plus margin.

Measured by the memory gate (`MEMORY_ROUNDS=40 MEMORY_SLOW_READERS=64
bench/memory.sh`, debug build, 8 MiB cache budget, 4 MiB spool budget, 2 MiB
maximum body, 64 slow readers): peak RSS 68,816 KiB with CLOCK and 64,508 KiB
with TinyUFO. Use these with their budgets as a starting point for sizing,
not as a limit for other configurations.

## 8. Forwarded-header security

- `X-Forwarded-For` and `X-Real-IP` carry Tanod's conclusion about the
  client; `X-Forwarded-Proto` the effective scheme; `X-Forwarded-Host` the
  request authority (default port dropped); `X-Forwarded-Port` its port.
- `Forwarded`, `X-Forwarded-Prefix`, `X-Original-Host`, `X-Original-URL`,
  `X-Rewrite-URL` and similar override headers are removed. CDN client-IP
  headers pass only from trusted proxies.
- Scheme and host come from the hop the trusted-proxy walk chose, never the
  leftmost entry of a list a client can prefix.
- New `server.trusted_proxies.host` (default `none`) lets a trusted proxy
  supply the public host; a believed host is validated and enters the key.

## 9. Graceful shutdown

`SIGTERM` withdraws readiness while still serving for
`server.graceful.drain_period`, then closes listeners and lets in-flight work
finish within `shutdown_timeout`. `SIGINT` is a fast shutdown. Set your
supervisor's stop deadline above drain + shutdown timeout + teardown; the
production examples use 50 s for 5 + 35 s.

## 10. Observability

New: `tanod_origin_disconnect_drains_active`,
`tanod_origin_disconnect_drains_total{outcome}`,
`tanod_cache_memory_pressure_total{outcome}`,
`tanod_config_reloads_total{outcome}`, `tanod_process_start_time_seconds`;
access-log `permit_released` reasons; alert examples
`TanodOriginWorkOutlivedDeadline` and `TanodCacheResponsesAborted`. OTLP
traces and JSON access logs as before.

## 11. Deployment and compatibility changes

- Hardened container: `scratch` base, non-root (uid 10001), read-only root,
  no capabilities.
- Validated VPS, Compose and fixed-tier Kubernetes examples, and a
  [Kubernetes staging procedure](../ops/production/KUBERNETES-STAGING.md).
  **The Kubernetes example has been checked statically only; it has not been
  run on a cluster for this release.**
- Release artifacts: Linux x86_64 only (GNU and static musl archives, a
  `linux/amd64` image, `@tanod/linux-x64`). No macOS, Windows or ARM64
  artifacts.
- Next.js: verified with Next 16.3.8 and 16.4.0 on Node 22 and 24.
- Release pipeline: the container image is smoke-tested before it is pushed
  and published under the digest that was tested; every GitHub Action is
  pinned to a commit SHA.

## 12. Upgrading from v0.3.2

Follow the [upgrade guide](UPGRADING-0.4.md). In short:

1. Validate your configuration with the 0.4.0 binary
   (`tanod check --config …`). Add `class:` to every route that sets
   `coalesce.override_origin`; the configuration is otherwise refused.
2. If an edge rewrites `Host` and your origin relied on `X-Forwarded-Host`
   passing through, set `server.trusted_proxies.host` and the edge's CIDRs.
3. Check non-browser clients and health checkers that send raw `/./` or
   `/../` paths: they now get `400`.
4. Decide `server.threads` if Tanod shares CPUs with the app.
5. Expect renders for departed clients to complete and count against the
   ceiling; keep `timeouts.origin` near your longest acceptable render.
6. Pin the image by digest, roll one instance, watch readiness, origin work,
   queue depth and errors. Keep the 0.3.2 binary and configuration for
   rollback; a rollback re-exposes the vulnerabilities above.

## 13. Known limitations

- The independent cache-security review is outstanding. The fixes above come
  from a maintainer-side review.
- The Kubernetes example has not been run on a cluster for this release.
- Undeclared personalisation on a route declared public cannot be detected.
- Cache and coalescing are per process; purges must reach every replica.
- Replicas partition capacity statically and do not coordinate; surge or
  force-deleted pods can overlap.
- Origin work after its own response or past `timeouts.origin` is not bounded
  by Tanod.
- The image's BuildKit provenance is not a signed GitHub artifact attestation.
  The image digest is not reproducible; the Linux binary is.
- `pingora-proxy` is vendored with a patch; registry-based audit tools do not
  see it (CI verifies it separately).

## 14. Security disclosure

Advisories for A–D will be published on the repository's
[security advisories page](https://github.com/tanod-sh/tanod/security/advisories)
once 0.4.0 is available. Report vulnerabilities privately as described in
[SECURITY.md](../SECURITY.md), not in public issues.

## 15. Validation summary

All on Linux x86_64. Local results are from one desktop (i5-9400F) on the
candidate's build inputs (`d770dd64…`, identical for `9d1f9d9` through the
release branch), and are not CI evidence.

| Area | Result |
|---|---|
| Formatting, Clippy (default and TLS), unit/integration/doc tests | pass: 1,238 tests |
| `cargo audit` (main and fuzz lockfiles), vendored Pingora check | pass; reviewed exceptions unchanged |
| Cross-user isolation harness (CLOCK; TinyUFO with the Next.js fixture) | 286/286; 301/301 |
| Origin lifecycle scenarios, HTTP/1.1 and HTTP/2 origins, both cache policies | 34/34 each; 12/12 each inside the hardened image |
| Hour-long soaks with the uncached-disconnect arm | CLOCK 706,767 requests, TinyUFO 432,662; origin peak 8/8; every disconnect drain completed (about 160,000), none at the deadline |
| Next.js 16.4.0 and 16.3.8 on Node 22 and 24 | 4/4 pairs, 28 checks each |
| Release image: smoke test of the exact image to be pushed | pass (non-root, read-only, version, TLS, config checks, health, proxying, admission 1/1, shutdown) |
| End-to-end suite (`bench/all.sh`) | 46 runs, all held, none skipped |
| Memory gate, 40 rounds × 64 slow readers | pass; peak RSS 68,816 KiB (CLOCK), 64,508 KiB (TinyUFO) |
| Restart and chaos (`upgrade.sh 20`, 10 chaos rounds) | pass; 0 failed requests across the socket handover |
| GNU binary reproducibility | byte-identical from two clean checkouts |
| Independent cache-security review | **not obtained** |
| Kubernetes staging | **not run** |
| CI and the release workflow on the tag | **pending** (runs when the tag is pushed) |
