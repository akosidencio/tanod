# Next.js production reference

This reference runs an edge proxy in front of one Tanod governor and three
identical standalone Next.js origins. The origins share Next.js cache and tag
state through a mounted cache directory. Only the edge listener is public; the
Tanod admin and metrics listeners and direct origin ports bind to loopback.

A second topology runs two Tanod replicas behind URI-stable ingress. Each
replica keeps local cache and admission state while a static partition keeps
their combined origin-work allocation inside one declared budget.

## Build and start

```bash
cargo build --locked
docker compose -f compose.nextjs.yaml up --build -d
```

Run the replicated topology with:

```bash
docker compose -f compose.nextjs.yaml -f compose.nextjs-replicated.yaml up --build -d
```

All origins use one image, one `deploymentId`, and one Server Action encryption
key. The runtime image contains `public`, `.next/static`, and a deployment
identity artifact. Do not rebuild origins separately during a rollout.

The shared-filesystem cache handler is a reference implementation for a
filesystem with atomic rename and coherent reads. Replace the mounted volume
with durable shared storage before running the origins on different hosts.

## Approve routes

Public dynamic routes live in
[`fixtures/next-storefront/tanod.next.yaml`](../fixtures/next-storefront/tanod.next.yaml).
Anything absent stays private.

```bash
node packages/tanod-next/src/cli.js inspect \
  --dist-dir fixtures/next-storefront/.next \
  --policy fixtures/next-storefront/tanod.next.yaml

node packages/tanod-next/src/cli.js generate \
  --dist-dir fixtures/next-storefront/.next \
  --policy fixtures/next-storefront/tanod.next.yaml \
  --upstream next-1:3000 \
  --upstream next-2:3000 \
  --upstream next-3:3000 \
  --concurrency 8 \
  --out /tmp/tanod.yaml \
  --check
```

Generation writes `tanod.deployment.json` beside the configuration. Commit
the assertion file; treat generated configuration and identity files as build
artifacts.

For several Tanod replicas, declare the group budget instead of copying a
local ceiling to every process:

```bash
node packages/tanod-next/src/cli.js generate \
  --dist-dir fixtures/next-storefront/.next \
  --policy fixtures/next-storefront/tanod.next.yaml \
  --upstream next-1:3000 --upstream next-2:3000 --upstream next-3:3000 \
  --global-concurrency 8 --replicas 2 \
  --out /tmp/tanod-replicated.yaml --check
```

This allocates four units to each process. Tanod rejects a configuration
whose declared replica sum exceeds `capacity.global_max`; the orchestrator
must keep its active replica count at or below `capacity.replicas`.

## Roll out in stages

Generate each stage with `--rollout <stage>`, validate it, then reload Tanod.
Move to the next stage only when the dashboard matches the expected behavior.

| Stage | Behavior | Expected signal | Rollback |
| --- | --- | --- | --- |
| `observe` | Classification and telemetry only | Origin traffic unchanged; admission decision is `observe` | Route ingress around Tanod |
| `protect` | Fixed admission and bounded queues | In-flight work stays below the ceiling; no cache hits | Reload `observe` configuration |
| `coalesce` | Public requests may share one in-flight render | Origin requests fall during identical bursts; cache stays empty | Reload `protect` configuration |
| `cache` | Short caching for approved routes | Hits rise and origin work falls; invalidation remains scoped | Reload `coalesce` configuration |
| replicate | Add statically partitioned Tanod replicas | Group capacity remains within its declared budget | Return ingress to one Tanod replica |

Every generated stage keeps the same route privacy decisions. The `observe`
mode disables admission, cache, coalescing, and spooling in the proxy itself,
so route-level settings cannot accidentally turn protection on.

## Diagnose and calibrate

Explain a request locally without contacting an origin:

```bash
node packages/tanod-next/src/cli.js explain \
  --dist-dir fixtures/next-storefront/.next \
  --policy fixtures/next-storefront/tanod.next.yaml \
  --url 'https://shop.example/products/one?ref=email' \
  --header 'Cookie: session=redacted'
```

Run all bounded reference checks:

```bash
./bench/nextjs-reference.sh
```

Calibration requires an explicit load acknowledgement and route allowlist. It
prints the target, routes, and concurrency steps before sending requests and
never edits the configuration.

```bash
node packages/tanod-next/src/cli.js calibrate \
  --target http://127.0.0.1:18080 \
  --route /products/calibration \
  --steps 1,2,4,8 \
  --allow-load
```

Use the recommendation as a starting point for review. Repeat the measurement
on production hardware with representative renders and monitoring before
raising the fixed ceiling.

## Replica behavior

The replicated edge hashes the URI consistently, preserving local cache and
coalescing value without making correctness depend on affinity. Configure
`@tanod/next` with every admin listener through `endpoints` or
`TANOD_PURGE_URLS`; a fan-out purge fails if any local cache cannot be
invalidated.

Running Tanod inside each app instance with `tanod-next start` is the other
end of the same trade: no partitioning and no fan-out list, because each Tanod
guards exactly one instance, at the cost of per-instance caches and purges that
reach only the instance that received them. The README's
[Choosing a topology](../README.md#choosing-a-topology) compares the two.

The reference keeps static partitioning authoritative. Capacity leases and
adaptive limits add coordination and failure modes without evidence that the
fixed allocation is insufficient. Distributed Tanod caching and coalescing
remain deferred for the same reason.

## Verify

The HTTP and browser suites cover public/private isolation, RSC variants,
Server Actions, Draft Mode, streaming, image negotiation, and coalescing. The
reference suite additionally checks build identity across origins, shared
Next.js cache invalidation, forwarded public URL, scoped Tanod purging,
metrics, and progressive streaming.

```bash
./bench/nextjs.sh
./bench/nextjs-browser.sh
./bench/nextjs-reference.sh
./bench/nextjs-replicas.sh
```

The replica suite checks the combined budget, URI affinity, purge fan-out,
abrupt loss of one governor, continued service, and recovery into the pool.

Production sign-off still requires an independent cache-safety review, an
artifact/rollback exercise, staging validation, and a sustained deployment
report. Those are evidence from real operation rather than repository features.
