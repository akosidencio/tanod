# Pingora reuse in Tanod

Tanod delegates HTTP proxying, connection management, protocol parsing and
HTTP cache coordination to Pingora. This change also reuses
`pingora-load-balancing` 0.9 for round-robin sequencing, Ketama selection and
HTTP health probes. Tanod retains the policies that govern origin work.

## What the upstream layer reuses

| Responsibility | Implementation |
|---|---|
| Round-robin sequence | Pingora `selection::algorithms::RoundRobin`, at configured-upstream and DNS-instance levels |
| Consistent path affinity | Pingora `selection::Consistent`, backed by `pingora-ketama` |
| HTTP health request, parsing and body drain | Pingora `health_check::HttpHealthCheck`, with an all-2xx validator |
| DNS refresh | Tanod resolves configured names on the blocking pool and keeps the last good answer |
| Health streaks and startup readiness | Tanod tracks the configured upstream, initially unhealthy |
| Passive ejection and recovery | Tanod's breaker, including probe identity and the ejection cap |
| Load-aware selection | Tanod's in-flight count and latency EWMA |
| Admission | Tanod's exact weighted permits, queue deadlines and priority reservations |

The full Pingora `LoadBalancer`/health registry is not installed as a second
routing service. Tanod groups DNS instances under a configured upstream's
breaker, metrics and capacity state. Pingora's built-in health state starts
healthy; Tanod's strict readiness requires an actual successful probe streak.
Using the selection and probe APIs keeps those semantics explicit.

DNS membership and each local Ketama ring are published together. After a
DNS refresh, the resolver rebuilds the pool ring on the blocking pool.
Request workers read selector snapshots and never build rings. Probes rotate
through the current DNS addresses using a sequence independent of live
requests. This remains health per configured name, not per resolved instance.

`consistent_hash` is optional. It assigns equal weight to each unique resolved
socket. Existing `hash_by_path` keeps its FNV modulo mapping; the default is
still `round_robin`. See [choosing an origin strategy](./OPERATIONS.md#choosing-an-origin-strategy)
for configuration and rollout details.

## Path movement measurement

The regression test routes 10,000 distinct `/products/{n}` paths to three
loopback endpoints, then adds a fourth. It checks that every Ketama path that
moves goes to the new endpoint. Separate tests cover removal, configuration
reordering, DNS snapshot replacement, health exclusions and breaker recovery.

| Strategy | Paths moved on 3 → 4 instances |
|---|---|
| Existing FNV modulo | 7,483 / 10,000 (74.83%) |
| Pingora Ketama | 2,567 / 10,000 (25.67%) |

Reproduce:

```sh
cargo test --lib consistent_hash --locked -- --nocapture
BALANCING_STRATEGY=consistent_hash bash bench/breaker.sh
```

The live breaker test checks actual origin render counts before failure,
after ejection and after recovery. The regular benchmark suite runs both
round-robin and Ketama, recording separate result artifacts.

## TinyUFO in the streaming cache

`TinyUFO` 0.9 is used by the optional `cache.eviction: tinyufo` policy inside
`BoundedStore`. CLOCK remains the default. See
[choosing a cache eviction policy](./OPERATIONS.md#choosing-a-cache-eviction-policy)
for enabling it, restarting and monitoring admission rejections.

Tanod retains HTTP storage, exact byte accounting, streaming writes, TTL
metadata and reverse indexes. The TinyUFO index carries entry identities and
weights rather than response bodies. Cache hits update its S3-FIFO use counts;
completed fills go through its admission policy.

The streaming budget can require eviction before a response is complete.
TinyUFO's public API evicts on insertion, so the adapter temporarily inserts
body-free pressure tokens to obtain S3-FIFO victims, then removes the tokens.
It frees the selected completed entries, their tags and their exact byte
charges. The reservation then accounts the next response chunk. Pressure
eviction deliberately proceeds before the completed-fill TinyLFU decision;
waiting until completion would exceed the streaming memory budget.

An admission-rejected completed fill stays visible only for the existing
25ms follower handoff, with its bytes still reserved. The handoff is published
atomically with respect to lookup and purge. Existing readers can drain it;
new readers cannot attach to a completed handoff invalidated by a purge.
`tanod_cache_admission_rejected_total` and `/status`'s
`cache.admission_rejected` report these rejected fills separately from eviction.

TinyUFO's entry weight is `u16`. Stores up to 16MiB use byte weights; larger
stores round upward in units that bound the policy to at most 2^24 units.
Entries exceeding 65,535 units saturate their policy weight. This affects
victim preference; Tanod's separate byte counter always charges the full
metadata, paths, tags and body, including in-progress fills. These counters
do not include allocator slack and map/policy bookkeeping, so container RSS
must still be sized using the memory gate.

Explicit purge, replacement and metadata updates advance the admission
revision and remove the previous identity. This avoids reusing TinyUFO's lazy
queue records for new content and bypasses its existing-key weight-update
path. Natural eviction retains admission history within that revision.
Removed queue records are compacted after more than `2 × entries + 64`
removals; otherwise repeated purge/refill of a small working set could grow
bookkeeping without ever filling the cache. Compaction restores retained
identities and visit credit and resets the admission sketch. Its cost is
amortized over removals.

`pingora-memory-cache` was not added: its generic TTL interface does not supply
Tanod's streaming HTTP storage and invalidation contracts.

## Retention measurement

Run in the release profile:

```sh
cargo bench --bench eviction --locked
```

Measured on 2026-10-15, Linux x86_64, six logical CPUs, Rust 1.98.0. Each
workload has 200 keys, 4,000 deterministic Zipf-like requests and a 16,000-byte
budget. Entries are charged for serialized Pingora metadata, purge path and
body, matching `Stored::size()`. TTL is ten minutes; no entries expire during
the experiment. Mixed-size bodies use `400 + (key % 8) * 600` bytes, so
popularity and size are not independent.

| Body sizes | FIFO | CLOCK | Integrated TinyUFO | Completed-only TinyUFO reference |
|---|---|---|---|---|
| 400 bytes | 51.42% | 58.55% | 62.3–62.6% | 66.40% |
| 400–4,600 bytes | 22.83% | 27.08% | 35.1–40.7% | 39.50% |

The production stores reserve metadata and body bytes during every fill;
the completed-only reference applies admission after an object is fully
available. The reference uses a 200-key estimator; the production policy
uses `budget / 1024`, clamped to 16–65,536, and periodically compacts retired
records. This compares complete implementations rather than isolating one
algorithmic difference. The reference does not describe Tanod's production
policy. The benchmark labels it separately and reports five integrated-policy
trials; the table shows their range rather than a selected trial.

Internal hash seeds are random, so subsequent trials can differ. Peak
accounted occupancy was 15,520 bytes for equal bodies and 15,995 bytes for
mixed bodies, within the shared budget. The complete
[CSV results](./data/pingora-eviction.csv) are checked in.

These are retention measurements on synthetic traffic. They do not compare
request latency, allocator memory, concurrent fills, expiry or purges.
Benchmark objects are preconstructed and shared; their backing allocations
are outside the measured logical byte budget. The live lifecycle gates test
the other contracts separately.

## Cache and response-spool compatibility

Pingora 0.9's streaming-cache reader emits a `Done` task without a final
`response_body_filter` call. Applying a second response spool to that path
could buffer a body without ever flushing it. This reproduced with CLOCK as
well as TinyUFO during the memory gate.

Tanod now creates a response spool only for requests bypassing HTTP cache
and coalescing. Cached/coalesced responses use their existing streaming
storage path. To apply the extra spool to a route, disable both cache and
coalescing on that route. Cache/coalescing responses can still experience
client backpressure; this change does not promise the spool's early capacity
release on those paths.

The strengthened memory gate verifies complete, matching bodies on a cached
miss and hit while global spooling is enabled. It separately drives an
uncached, uncoalesced route through the spool, requires an actual spool
outcome, checks sampled spool occupancy, and enforces the exact cache budget.
Missing counter series count as zero and cannot bypass assertions through an
arithmetic error.

## Validation

Unit regressions cover large fills and weight gaps, repeated purge/refill and
replacement, bounded lazy policy records, metadata growth and shrink, expiry
and stale windows, admission-rejected handoff and purge, abandonment, and
concurrent fill/read/purge under eviction pressure.

The normal CI suite also runs these scripts with `CACHE_EVICTION=tinyufo`:

```sh
CACHE_EVICTION=tinyufo bash bench/coalesce.sh
CACHE_EVICTION=tinyufo bash bench/stream.sh
CACHE_EVICTION=tinyufo bash bench/protocol.sh
CACHE_EVICTION=tinyufo bash bench/purge.sh
CACHE_EVICTION=tinyufo bash bench/memory.sh
CACHE_EVICTION=tinyufo bash bench/storage.sh
CACHE_EVICTION=tinyufo bash bench/soak.sh
```

The override changes only the generated root cache policy and records a
separate policy-labelled artifact. See [release gates](./RELEASE-GATES.md) for
longer soak and pressure runs before a release.

## Other crates

`pingora-limits` provides counters/rate estimators; it does not replace exact
weighted admission with resizing, deadlines and priority reservations.
`pingora-timeout` is already used transitively by Pingora; Tanod retains Tokio
for its policy deadlines. `pingora-foundations` is optional telemetry
integration, while Tanod already exposes its own metrics and OTLP export.
OpenSSL, BoringSSL and s2n are alternative TLS backends. The `tls` feature
continues to select Rustls for both Pingora core and load balancing.
