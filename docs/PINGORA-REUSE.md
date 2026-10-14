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

## TinyUFO retention evaluation

`TinyUFO` 0.9 is a development dependency for `bench/eviction.rs`. It is not
used by the production cache. The comparison exercises the real
`BoundedStore` CLOCK/FIFO implementations and TinyUFO's weighted completed
entry retention on the same requests and accounted-byte budget.

Run in the release profile:

```sh
cargo bench --bench eviction --locked
```

Measured on 2026-10-15, Linux x86_64, six logical CPUs, Rust 1.98.0. Each
workload has 200 keys, 4,000 deterministic Zipf-like requests and a 16,000-byte
budget. Each entry is charged for its serialized Pingora metadata, purge
path and body, matching `Stored::size()`. TTL is ten minutes; no entries
expire during the experiment. Mixed-size bodies use
`400 + (key % 8) * 600` bytes, so popularity and size are not independent.

| Body sizes | FIFO hit ratio | CLOCK hit ratio | TinyUFO hit ratio |
|---|---|---|---|
| 400 bytes | 51.42% | 58.55% | 66.40% |
| 400–4,600 bytes | 22.83% | 27.08% | 39.50% |

Five TinyUFO trials produced the same rounded ratios in this run. Internal
hash seeds are random, so subsequent trials need not match exactly. Peak
accounted occupancy was 15,520 bytes for equal bodies and 15,995 bytes for
mixed bodies for all policies, within the shared budget. The complete
[CSV results](./data/pingora-eviction.csv) are checked in.

These are retention measurements on synthetic traffic. They do not compare
request latency, allocator memory, concurrency, expiry, tags, invalidation,
or streaming fills. Benchmark objects are preconstructed and shared; their
backing allocations are outside the measured logical byte budget.

TinyUFO is a promising completed-entry policy, but it cannot replace Tanod's
HTTP `Storage` implementation directly. An integration must preserve:

- Byte reservations while fills are still streaming, abandoned-fill cleanup,
  and transient coalescing handoff.
- Path/tag invalidation, including fills invalidated before completion.
- Cleanup for every evicted or admission-rejected entry. TinyUFO returns a
  hashed key; the value must carry the original identity for reverse indexes.
- Accounting for responses larger than 65,535 bytes. TinyUFO's per-entry
  weight is `u16`; production needs conservative weight units plus Tanod's
  exact byte counter.

The production default remains CLOCK. A future TinyUFO integration should
use it as an eviction/admission component inside the bounded streaming store
and run the existing streaming, purge and memory gates before becoming a
configurable policy. `pingora-memory-cache` adds a generic TTL cache around
TinyUFO, rather than Tanod's HTTP storage and invalidation contracts, so it
was not added.

## Other crates

`pingora-limits` provides counters/rate estimators; it does not replace exact
weighted admission with resizing, deadlines and priority reservations.
`pingora-timeout` is already used transitively by Pingora; Tanod retains Tokio
for its policy deadlines. `pingora-foundations` is optional telemetry
integration, while Tanod already exposes its own metrics and OTLP export.
OpenSSL, BoringSSL and s2n are alternative TLS backends. The `tls` feature
continues to select Rustls for both Pingora core and load balancing.
