# Tanod 0.4.0 release notes (draft)

**Draft for an unpublished release.** Nothing here is published until the
maintainer approves publication; see [release gates](RELEASE-GATES.md) for
what is still open. Read the [upgrade guide](UPGRADING-0.4.md) before
deploying: 0.4.0 changes defaults and refuses some configurations 0.3.2
accepted.

Tanod is a reverse proxy, built on Cloudflare Pingora, that protects a
server-rendered origin: it limits how much render work reaches the origin at
once, collapses identical concurrent requests, and serves short-lived cached
copies where the route and the origin allow it.

## What Tanod guarantees, and what it does not

Tanod strictly bounds its managed admitted upstream exchanges under its
defined lifecycle and deadline contract. It does not independently bound
arbitrary backend application work that continues after a response or beyond
the drain deadline.

In practice: a request holds its origin permit from admission until the
origin's response ends, the origin fails, or `timeouts.origin` passes, and a
client disconnecting does not end it early. Work the origin does after its
own response ended (`after()` callbacks, background revalidation) and work
past the deadline are invisible to any proxy. If your application must never
exceed a number of concurrent renders, enforce it in the origin as well, with
a semaphore sized to Tanod's ceiling. The ceiling is per process; replicas
partition it statically and do not coordinate.

## Highlights

**SSR origin protection.** Weighted admission with a global ceiling, per-route
limits, priority tiers and a bounded queue; excess is shed with `503` and
`Retry-After`, and browsers can get a self-refreshing busy page. Static assets,
streaming routes and upgrades have their own rules and do not take render
permits.

**Origin capacity through client disconnects.** An admitted request whose
client leaves keeps its permit while Tanod reads the origin's response to its
end, bounded by `timeouts.origin`. Before 0.4.0 a disconnect freed the permit
mid-render; a burst of disconnects against a ceiling of 4 reached 13
concurrent renders. Now verified at the ceiling over HTTP/1.1 and HTTP/2
origins, and inside the production container image. This needs a reviewed
patch to Pingora's proxy crate, vendored and checksum-verified in CI.

**Cache correctness and response sharing.** Keys separate scheme, host,
method, path, query, configured and Next.js variants, content coding and
deployment. Responses that set cookies, vary on unkeyed headers, or answer
`Authorization`, draft-mode or Server Action requests are never shared.
Incompatible policy reloads retire the cache namespace, and purge closes
in-flight fills to new readers.

**Request coalescing and streaming.** Concurrent identical misses share one
render, and followers receive the leader's chunks as they are produced.

**Policy reloads.** `SIGHUP` reloads atomically. Compatible changes keep the
cache; sharing changes start a new namespace.

**Multithreaded performance.** The proxy uses every available CPU (respecting
affinity and cgroup quotas; `server.threads` overrides it). Cache hits share
stored bodies without copying.

**Forwarded-header hardening.** Untrusted forwarding headers are overwritten
or removed; only configured proxies can supply client address, scheme or (opt
in) public host, and a believed host is part of the key.

**Memory pressure.** Registration falls back uncached, an in-progress fill
finishes unstored within one body of headroom, and only past that is a
response aborted visibly. Logical cache bytes are not an RSS ceiling: budget
container memory with margin.

**Observability.** Prometheus metrics for admission, queues, cache, pressure,
drains, reloads and process start; OTLP traces; JSON access logs with a
per-request `permit_released` reason.

**Deployment guidance.** Validated VPS, Compose and fixed-tier Kubernetes
examples, hardened container (non-root, read-only, no capabilities), and a
Kubernetes staging procedure. The Kubernetes example has been checked
statically only. It has not been run on a cluster for this release.

## Security fixes found during the 0.4.0 review

**All three are present in the released 0.3.2** (same code paths). Users of
0.3.2 who set `coalesce.override_origin`, use `cache.query` lists, or rely on
the origin ceiling against untrusted clients should upgrade or apply the
mitigations below.

- `coalesce.override_origin` could store a response the origin marked
  `private` (with a positive `max-age`) and serve it to other visitors. It now
  never stores, and requires an explicit route class.
- `cache.query` include/exclude lists matched parameter names as sent while
  origins decode them, letting an attacker store a page under the bare URL.
  Names are now matched as decoded.
- Paths with `.`/`..` segments were routed and classed as sent while the
  origin resolved them, which could cache a private page under a public route
  or take a dynamic page past the origin ceiling as `static`. They are now
  refused with `400` in protect mode.

Mitigations for 0.3.2 until upgrading: remove `coalesce.override_origin` from
routes whose origin can send `private` or `no-store` with a positive
`max-age`; have the edge reject or normalise request targets with dot
segments; and remove `cache.query` include/exclude lists, or reject
percent-encoded parameter names at the edge.

The cache and sharing review behind these fixes is a maintainer-side review,
[published for an independent reviewer](reviews/CACHE-SECURITY-REVIEW-v0.4.0.md).
**It is not an independent audit**, and that review is still outstanding.

## Compatibility changes

See the [upgrade guide](UPGRADING-0.4.md) for actions. In short:
worker threads now default to available CPUs; disconnects no longer stop the
origin's render; `coalesce.override_origin` requires `class:`; dot-segment
targets get `400`; query-policy names match decoded; untrusted forwarding
headers are rewritten; shutdown has a readiness drain window.

Next.js: verified with Next 16.3.8 and 16.4.0 on Node 22 and 24 (see release
gates for the run). Requires Linux; the release binary and image are x86_64.

## Measured, not promised

Numbers in the release evidence (soaks, RSS, query-flood costs) come from one
desktop and are published with their parameters. They are not throughput or
latency guarantees for other hardware.
