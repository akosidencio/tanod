# Deployment security model

This is an internal source review and a guide to deployment assumptions. It is
not an independent security audit. Read the broader [threat model](THREAT-MODEL.md)
and the [cache review brief](CACHE-KEY-REVIEW.md) before approving a release.

## Boundaries

* Internet clients control paths, queries, cookies, headers, protocol framing
  and connection lifetimes. Pingora 0.9 handles HTTP parsing and framing;
  Tanod's protocol, HTTP/2 and forwarding regressions test the integrations.
  This review does not prove the parser immune to request smuggling.
* The immediate peer may supply forwarding claims only when its address
  matches `server.trusted_proxies.from`. The edge must overwrite or correctly
  append every configured list. Trust of public host is separately opt-in
  (`server.trusted_proxies.host`). A trusted peer forwarding an attacker's
  unchanged header has not authenticated that header.
* The origin declares sharing through response headers; route configuration
  may explicitly override some origin directives. Operators must establish
  that such routes have no undeclared personalization. Authentication,
  session/draft cookies, Server Actions, Set-Cookie, unsupported Vary and
  variant keys have separate guards. Never enable overrides merely to improve
  a benchmark hit ratio. RSC, prefetch and HTML are distinct variants.
* Local configuration, process signals, TLS files and environment secrets are
  trusted operator inputs. Limit their writers. SIGHUP reloads are serialized;
  invalid or restart-only changes retain the running policy. There is no
  HTTP reload endpoint.
* The admin and Prometheus listeners are separate from traffic. Liveness,
  readiness, status and metrics do **not** require authentication. Bind them
  to loopback/private networks and enforce access rules for container peers
  and cluster workloads as well as published host ports. Do not route them
  through public ingress. `/status` exposes topology and policy state.
* `POST /purge` is disabled without `cache.purge.token`. Otherwise it requires
  `Authorization: Bearer …`; credential comparison uses `subtle` rather than
  a plain compiler-optimizable loop. Token length is not concealed. Query
  parameter counts are bounded. Possessing the token authorizes expensive
  invalidation and new origin work: restrict its network and callers. There
  is no per-credential purge rate limiter; rate-limit a purge gateway if needed.

## Cache lifecycle

Lookup attaches under a shared lifecycle guard, excludes invalidated fills,
then checks the completed map again after a temporary-map miss. Completion
admits before removing its temporary entry while holding the same shared
lifecycle guard. Purge and namespace retirement take the exclusive side, so
completion cannot resurrect purged content. A late old-generation fill starts
ineligible for new subscriptions; only its pre-reserved leader reader can be
transferred. Already attached followers retain their progressive response.

Incompatible cache-policy changes retire the namespace before publishing the
new policy. Namespaces are encoded in storage/coalescing keys. Returning from
A to B to A allocates a new identity. These protections do not turn purge into
an origin transaction: work not registered at purge time may register later
in the same active namespace. See [operations](OPERATIONS.md).

## Resource limits and remaining assumptions

An admitted request holds its origin permit until the origin's response ends,
including after its client disconnects: Tanod reads the rest of the response
and discards it, bounded by `timeouts.origin`
([details](OPERATIONS.md#what-the-origin-ceiling-guarantees)). A client can
therefore no longer free origin capacity, and so admit more concurrent origin
work, by hanging up. What a proxy cannot bound is work an origin continues
after its own response ended, or past the drain deadline
(`tanod_origin_disconnect_drains_total{outcome="deadline"}`); a strict bound
on those needs an origin-side limit. The drain path is carried as a reviewed
patch to Pingora's proxy crate in `vendor/pingora-proxy`, which `cargo audit`
cannot see; `scripts/check-vendored-pingora.sh` ties it to the published crate's
checksum.

Admission bounds proxy-held origin work units, queue count and queue time. Upgrade slots
are separate and off by default. Cache bodies, cache logical bytes and spool
bytes have limits. Cache pressure falls back before registration, abandons a
fill into bounded headroom when possible, then aborts visibly past that
headroom. The first oversized chunked response may still be aborted.

Logical cache bytes are not an RSS ceiling: followers can retain detached
bodies, and indexes, stacks, allocator slack and network buffers cost memory.
A container needs tested RSS margin. Avoid unbounded connection counts with
file descriptor limits, ingress connection/rate/body limits and infrastructure
controls. Tanod does not provide per-client rate limiting or a configurable
request-upload size ceiling. Pingora's HTTP/1 downstream read timeout defaults
to 60 seconds; Tanod's origin/idle settings do not promise a total upload
wall-clock deadline. Enforce upload deadlines and HTTP/2 connection policy at
the public edge, including for static/exempt routes.

Logs omit credentials and raw request headers; access logs can still contain
request URLs. Treat queries as potentially sensitive and set retention/access
accordingly. Metric label values come from configuration or finite enums,
not URLs, hosts, query values or secrets. Retired limiter gauges are removed;
route counter/histogram series can persist across changing route IDs until
process restart. Keep IDs stable across reloads; arbitrary operator-driven
route-ID churn is not a bounded-cardinality promise.

## Dependency advisory review (2026-10-29)

The checked lockfile retains Pingora 0.9.0 and Tokio 1.53.3. Tokio's `~1.53.3`
constraint remains: [Pingora issue 1059](https://github.com/cloudflare/pingora/issues/1059)
tracks its zero-capacity BufStream incompatibility with Tokio 1.54.
[Pingora's release list](https://github.com/cloudflare/pingora/releases) was
checked; upgrading beyond its currently released dependency graph is not an
available local fix. The refreshed RustSec database identity and audit output
are recorded with validation evidence. An audit pass with exceptions means
reviewed exceptions remain, not absence of vulnerable dependencies.

The exception set in [audit.toml](../.cargo/audit.toml) is unchanged. Each row
below names the potentially vulnerable behavior, mitigation and removal
condition. Re-review all zstd exceptions if compression, streaming contexts,
seekable, experimental, multithreaded compression or dictionaries are enabled.

| Advisory | Locked package / dependency path | Reachability, mitigation, remaining risk / removal condition |
|---|---|---|
| RUSTSEC-2024-0388 | derivative 2.2.0 ← pingora-core | Unmaintained build-time proc macro; not runtime code. Build supply-chain maintenance remains. Remove when Pingora replaces it. |
| RUSTSEC-2024-0449 | unsafe-libyaml 0.2.11 ← serde_yaml ← pingora-core | Unmaintained parser; Tanod uses serde-saphyr and programmatic ServerConf, not Pingora YAML APIs. Remove on upstream replacement; re-review any configuration-loading change. |
| RUSTSEC-2025-0134 | rustls-pemfile 2.2.0 ← pingora-rustls (TLS feature) | Unmaintained PEM reader; operator-owned certificate input. Maintenance risk remains; remove on Pingora TLS dependency replacement. |
| RUSTSEC-2026-0391 | zstd 0.13.3 ← pingora-core / pingora-header-serde | Reused stream `with_context` leaks prior-stream data. Header metadata uses one-shot contexts; response compression is not enabled. Remove when upstream accepts zstd ≥0.14.3. |
| RUSTSEC-2026-0395 | zstd-safe 7.2.4 ← zstd / pingora-header-serde | Experimental StableInBuffer lifetime flaw; feature disabled. Remove on upstream zstd-safe ≥8.2.0. |
| RUSTSEC-2026-0396 | same zstd-safe path | Uninitialized Seekable table; seekable disabled. Same removal condition. |
| RUSTSEC-2026-0397 | same zstd-safe path | Streaming OutBuffer::around_pos; not called by one-shot header serialization. Same removal condition. |
| RUSTSEC-2026-0398 | same zstd-safe path | Seekable decompression length; seekable disabled. Same removal condition. |
| RUSTSEC-2026-0399 | same zstd-safe path | Experimental dictionary-context initialization; feature disabled. Same removal condition. |
| RUSTSEC-2026-0400 | same zstd-safe path | One-shot calls on an open stream; metadata contexts never enter streaming state. Same removal condition. |
| RUSTSEC-2026-0401 | same zstd-safe path | Dictionary-training size overflow; training not called. Same removal condition. |
| RUSTSEC-2026-0402 | same zstd-safe path | CCtx cloning with zstdmt; experimental/zstdmt disabled. Same removal condition. |
| RUSTSEC-2026-0403 | same zstd-safe path | Raw streaming decoder exposure on a bad legacy frame; metadata decode uses one-shot self-produced buffers. Same removal condition. |
| RUSTSEC-2026-0404 | same zstd-safe path | Experimental DCtx cloning; feature disabled. Same removal condition. |
| RUSTSEC-2026-0405 | same zstd-safe path | Seekable InBuffer position; seekable disabled. Same removal condition. |
| RUSTSEC-2026-0406 | same zstd-safe path | zstdmt context mutation mid-frame; feature disabled. Same removal condition. |

The zstd wrappers contain unsafe code outside Tanod. Reachability restrictions
are the mitigation; they are not a formal safety proof. The package-level
upstream upgrade remains necessary to remove these exceptions. New advisories
must fail CI until individually reviewed; none were added to make this run pass.
