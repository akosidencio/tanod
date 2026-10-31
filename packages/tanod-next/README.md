# @tanod/next

This package does not make every Next.js application faster. Tanod is an
overload governor for expensive SSR and dynamic origin workloads; this package
only derives safer route policy from a Next.js build and coordinates cache
invalidation. Static or mostly ISR applications may not need Tanod at all.

Two things Tanod cannot work out on its own, taken from the place that
already knows them — your Next.js build.

1. **Generate route configuration.** `next build` writes manifests saying which
   routes exist, which are prerendered, which are Route Handlers, and what the
   build id is. This turns that into a Tanod config, so route policy is
   derived from the build rather than guessed at by hand.
2. **Route invalidation to Tanod.** `revalidateTag()` and `revalidatePath()`
   invalidate Next's own cache. They do nothing to the copy Tanod is serving
   in front of it. These helpers do both.

Zero runtime dependencies. Runs on Node and Bun.

Written in JavaScript with no build step — the published files are the files
that run — and type-checked as if it were TypeScript: `npm run typecheck` runs
`tsc` over the source with `checkJs`, and consumers get full types from
`index.d.ts`.

```bash
npm install --save-dev @tanod/next   # or: bun add -d @tanod/next
```

---


## Running the app and Tanod together

The lightest way is to let Tanod start your server itself with
`origin.command` in `tanod.yaml`, and make `tanod run` the container command:
no Node or Bun process sits between them. See the main README's
[Run Tanod inside your app](https://github.com/tanod-sh/tanod#run-tanod-inside-your-app).

Where you cannot change the container command, `tanod-next start` runs your
server and Tanod in front of it from Node or Bun, at the cost of that extra
runtime process.
Install `@tanod/next` as a regular dependency for this (it brings the
`@tanod/linux-x64` binary as an optional dependency), then:

```json
"scripts": { "start": "tanod-next start" }
```

The origin starts on `127.0.0.1:3000` (`--origin-port`), Tanod starts once the
origin accepts connections, `SIGTERM` drains Tanod before stopping the origin,
and either process exiting stops both. The origin can be any HTTP server:

```bash
tanod-next start -- node server.js                  # Next standalone output
tanod-next start -- node .output/server/index.mjs   # Nuxt
```

See `tanod-next start --help` and the main README's
[Run Tanod inside your app](https://github.com/tanod-sh/tanod#run-tanod-inside-your-app).

## Generating configuration

```bash
next build
npx tanod-next generate --upstream next-1:3000 --concurrency 40 \
  --out tanod.yaml --check
```

Approve public dynamic routes in a checked-in policy:

```yaml
version: 1
routes:
  /products/[slug]:
    privacy: public
    weight: 3
    cache:
      ttl: 2s
      stale_if_error: 1m
```

```bash
npx tanod-next generate --policy tanod.next.yaml \
  --upstream next-1:3000 --concurrency 40 --out tanod.yaml --check
```

Unknown fields, unmatched routes, unsafe methods, credential variants, and
cache settings on private routes are rejected. Routes without an assertion
remain private. Generation also writes `tanod.deployment.json` beside the
configuration for origin identity checks.

`--check` runs `tanod check` on a temporary sibling file and **fails if it is
rejected**. The previous config is replaced atomically only after validation,
so a failed generation cannot destroy the last known-good output.

Regenerate after every build. `deployment.id` is the Next build id and it is
part of every cache key, so a new build is never served the previous build's
entries — and the `SIGHUP` that applies the new id purges them.

### What it generates, and what it refuses to

**Anything the build does not prove is shareable is generated private.**

A prerendered route is proof: Next produced one response for everybody, so it
can be given to everybody. That is the only evidence in the build, and it is
the only thing that earns a public class — with `override_origin`, which is
correct here precisely because the build is the evidence.

A dynamically rendered route is not proof of anything. It may read cookies, a
session, or a header. So it is generated as `private_dynamic` with a comment
saying exactly how to opt in. A generator that guessed `public_ssr` would be
one bad guess away from serving one user's page to another, and it would do it
silently, in a file nobody reads closely because a tool wrote it.

It also generates the two routes that are easy to get wrong by hand:

- `/_next/static/**` as `static` — serving a chunk is not rendering.
- `/_next/image` with `vary: [Accept]`, which is load-bearing. Next negotiates
  the output format on `Accept` and answers `Vary: Accept`, so without it in
  the key Tanod refuses to store the response and the route gets a 0% hit
  rate. It also gets `priority: low` and `weight: 4`, because an image
  transform is several hundred milliseconds of origin CPU and must not starve
  page renders.

### Running it automatically

Two ways, and they are for different jobs.

**A `postbuild` script — the one to use in CI.** `npm run build` and
`bun run build` both run it, its exit code is the build's exit code, and it
appears in the log as its own step:

```json
{
  "scripts": {
    "build": "next build",
    "postbuild": "tanod-next generate --upstream next-1:3000 --concurrency 40 --out tanod.yaml --check"
  }
}
```

**A `next.config` wrapper — for local development**, so nobody has to remember
a second command:

```js
// next.config.mjs
import { withTanod } from '@tanod/next/config';

export default withTanod(
  { output: 'standalone' },
  { out: 'tanod.yaml', upstreams: ['next-1:3000'], check: true },
);
```

Next has no post-build callback, so the wrapper works from a
`process.on('exit')` handler. That is worth knowing rather than hiding:

- **It only runs on a successful build.** A non-zero exit means the build
  failed, and generating a route policy from a half-finished build would leave
  a stale file that looks current.
- **It can still fail the build.** Setting the exit code from an exit handler
  works, so a config Tanod rejects fails the build that produced it — not a
  warning nobody reads.
- **It fires once**, guarded by an environment variable, because Next loads
  `next.config` in its compilation workers as well as in the build itself.
- **It does nothing in `next dev`**, because it is gated on the
  production-build phase.

Both paths call the same code, so a config that is checked in CI and unchecked
locally is not a thing that can happen.

Where the `tanod` binary is not available — a build container that does not
ship it — omit `--check` rather than letting it fail; the generate step needs
nothing but Node.

### Options

```
--dist-dir <DIR>     Next build output. Default: .next
--policy <FILE>      Checked-in dynamic-route assertions.
--upstream <ADDR>    Repeatable. With at least one, the output is a complete
                     config; with none, it is routes only.
--concurrency <N>    Required with --upstream. Measure this origin ceiling.
--global-concurrency <N> One budget for a Tanod replica group.
--replicas <N>       Maximum replicas sharing the global budget.
--rollout <STAGE>    observe, protect, coalesce, or cache.
--out <FILE>         Write here instead of stdout.
--identity-out <FILE> Deployment identity artifact path.
--routes-only        Omit deployment.id as well as the origin block.
--check              Run `tanod check` on the result and fail if it is
                     rejected. Needs --out and at least one --upstream.
--tanod-bin <PATH> The tanod binary. Default: $TANOD_BIN, else PATH.
```

`concurrency` is the one number that has to come from your own measurement: it
is the ceiling on how much work the origin does at once, and the right value is
a property of your renders and your hardware, not of your framework.

For multiple Tanod processes, use `--global-concurrency` with `--replicas`
instead. Generation divides down conservatively, records the allocation in
`capacity` and the identity artifact, and refuses incomplete or conflicting
inputs.

### Inspect, explain, doctor, and calibrate

```bash
tanod-next inspect --policy tanod.next.yaml
tanod-next explain --policy tanod.next.yaml \
  --url 'https://shop.example/products/one' --header 'Cookie: session=secret'
tanod-next doctor --config tanod.yaml --origin http://next-1:3000 \
  --traffic http://tanod:8080 --metrics http://tanod:9090
tanod-next calibrate --target http://127.0.0.1:3000 \
  --route /products/one --route '/search?q=example' \
  --steps 1,2,4,8 --duration 10 --warmup 2 --max-p95-ms 500 --allow-load
```

`inspect` highlights every public override and its evidence. `explain` is
local and redacts credential values. `doctor` performs bounded deployment
checks. `calibrate` refuses non-local targets unless `--allow-production` is
also present, never changes configuration, and prints the load shape before
starting.

Point calibration at the origin directly, or at Tanod in `observe` mode.
An enforcing proxy or a cached response can hide the application's capacity.
Each concurrency step has a separate warm-up window (2 seconds by default)
and measurement window (10 seconds). Repeat `--route` for a representative
mix; routes are selected round-robin, and repeating a path increases its share.
Existing query parameters are preserved. A unique calibration query parameter
and `Cache-Control: no-cache` discourage response reuse, but do not guarantee
that framework or data caches are bypassed.

Calibration starts at concurrency 1 and stops at the first unhealthy step. A
healthy step has no failed requests in warm-up or measurement, at least 20
measured samples, and full-response p95 latency within twice the baseline
(with a 1ms baseline floor for timer resolution). `--max-p95-ms` adds a stricter
application latency target when supplied. Warm-up is excluded from measured
latency and throughput; streaming responses are measured through their final
byte. This is a workload-specific starting point, not a production guarantee.

The JSON report includes `ok`, `highestHealthyConcurrency`,
`recommendedCeiling`, and the measurements and rejection reasons for each
step. The recommendation is 70% of the highest healthy concurrency, rounded
down with a minimum of 1. Without a healthy baseline, the recommendation is
`null` and the CLI exits with status 1. If no saturation was established,
`observedKnee` is `null` and the report includes a warning; the suggested
ceiling is limited to the steps actually measured. Configuration is never edited.

Load windows are bounded: `--duration` allows up to 60 seconds per step,
`--warmup` up to 30 seconds, and `--timeout-ms` up to 60000ms per request.
In-flight requests can finish after a window ends. Each phase also stops at
100000 requests. For reproducible request-count experiments, use
`--requests-per-worker N` instead of `--duration`; warm-up defaults to zero in
that mode. `--min-samples N` can raise the sample requirement above 20.

Use `--rollout observe`, then `protect`, `coalesce`, and `cache` in order.
Observe mode records policy decisions but disables protection and reuse in the
proxy. See the [production reference](../../docs/NEXTJS-PRODUCTION-REFERENCE.md).

---

## Invalidation

Set `cache.purge.token` and a `telemetry.admin` listener in Tanod, then:

```js
import { revalidateTag, revalidatePath } from '@tanod/next';

// Invalidates Next's incremental cache AND Tanod's copy.
await revalidateTag('product-42');
await revalidatePath('/products/iphone');
```

Tag invalidation expires Next immediately with `{ expire: 0 }` by default.
This prevents the first Tanod miss from receiving stale-while-revalidate
HTML from Next and caching it again. To select another Next cache-life profile,
pass `nextProfile` alongside the Tanod connection options:

```js
await revalidateTag('product-42', { nextProfile: 'max' });
```

Or without Next in the loop — from a deploy hook, a CLI, a webhook:

```js
import { createPurger } from '@tanod/next';

const tanod = createPurger({
  endpoints: process.env.TANOD_PURGE_URLS.split(','), // every ADMIN listener
  token: process.env.TANOD_PURGE_TOKEN,
});

await tanod.purge({ tags: ['sale'], paths: ['/products/iphone'] });
```

Four behaviours worth knowing, all of them deliberate:

- **A failed purge throws.** That includes a `2xx` response whose JSON does not
  prove the purge succeeded, which catches traffic listeners and proxies that
  answer the purge path with an HTML success page.
- **Values are encoded exactly once.** Tanod percent-decodes purge parameters
  once, so spaces, `%`, `&`, `?`, and `#` remain part of the tag or path rather
  than becoming query syntax. A comma in a tag is still refused because the
  origin tag header is comma-separated and could never have stored that name.
- **The token travels in an `Authorization` header, never in the URL**, and
  redirects are refused rather than followed, so it cannot be bounced to
  another host.
- **An empty list is a no-op, not a purge of everything.** `purgeAll()` is
  spelled out because it makes every cached page re-render.

---

## Compatibility

The maintained integration targets are:

| Next.js | Node.js | Routers | Required CI coverage |
|---|---|---|---|
| 16.4.3 | 22.x, 24.x | App + Pages | Full integration suite on both runtimes |
| 16.3.8 | 22.x, 24.x | App + Pages | Full integration suite on both runtimes |

Tested is not the same as safe to run. Next.js 16.0.0 through 16.4.0, 16.3.8
included, are affected by GHSA-673q-mjm6-r5xr and GHSA-2464-fhr4-w9xw
(unauthenticated remote code execution) and GHSA-p3m5-59mp-jv5r (denial of
service), fixed only in 16.4.1. Run 16.4.1 or later; Tanod cannot mitigate
them. 16.3.8 stays in the matrix for applications still moving off 16.3.

The [compatibility declaration](./src/compatibility.json) drives CI and CLI
help. Every combination builds a real standalone application, confirms its
actual framework and Node versions, generates Tanod policy from that build,
and validates it with `tanod check`. HTTP tests cover response isolation,
coalescing, streaming, and App/Pages payload variants; Chromium drives real
prefetch, navigation, and Server Actions. Reference and replica tests exercise
invalidation, shared Next.js cache state, purge fan-out, and failure recovery.
All combinations must pass before a release can publish. CI attaches logs for
each pair so coverage can be checked against a particular commit or tag.

Run one pair locally from the repository root (requires Docker, Chromium, and
a built `target/debug/tanod` for policy validation):

```bash
TANOD_NEXT_VERSION=16.3.8 TANOD_NEXT_NODE=24 bash bench/nextjs-compatibility.sh
```

Node 22 and 24 are the supported Node release lines; CI uses their current
patches and records the actual runtime. Exact Next patches are intentional:
adding a patch to the matrix requires running the integration suite, rather
than assuming unchanged manifest versions prove unchanged runtime behavior.

The npm peer range (`next >=14`) and engine minimum (`node >=18.17`) permit
installation on older applications; they do **not** promise integration
coverage for those versions. Next.js 14/15, other Next patches, and other Node
release lines are outside the maintained matrix. They may work with understood
manifest versions, but require your own deployment validation.

Next's build manifests have their own version numbers. The generator accepts
`routes-manifest` v3 and `prerender-manifest` v4 and refuses unknown versions.
This protects policy generation but does not establish browser or invalidation
compatibility. Historical manifest-only checks for Next 16.3.3 remain recorded
in `VERIFIED_NEXT_RELEASES`; that history is separate from the maintained matrix.

### Runtimes

| Runtime | Status |
|---|---|
| Node 22.x, 24.x | Adapter tests and real Next.js integration in CI |
| Bun 1.4.0 | Adapter tests in CI; no Bun-hosted Next.js integration claim |

`npm run test:all` runs both.

One suite, both runtimes: the package uses only `node:` builtins and web
standards (`fetch`, `AbortSignal`), and the tests are `node:test`, which Bun
runs natively.

---

## What this does not do

- **No `revalidatePath()` route patterns.** Tanod purges by exact path, so
  `revalidatePath('/products/[slug]', 'page')` has no equivalent; matching a
  dynamic route pattern needs route metadata Tanod does not carry.
- **Replica fan-out is explicit.** Tanod's cache is per process. Pass every
  admin listener through `endpoints` or `TANOD_PURGE_URLS`; the purge fails
  if any replica cannot be invalidated.
- **No inferred route cost.** Nothing in a Next build says what a page costs
  to render. Set `weight` explicitly in the assertion file.
