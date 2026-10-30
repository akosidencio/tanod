# Tanod Next.js storefront fixture

This is a deliberately small but real Next.js application, serving an App
Router and a Pages Router surface from one origin. It uses standalone output
and exercises public SSR, query-keyed dynamic pages, React Server Component
navigation, Suspense streaming, cookies, Draft Mode, a Server Action, and an
absolute `Set-Cookie` privacy barrier.

It verifies Tanod's integration with expensive SSR and dynamic Next.js
request shapes. It is not evidence that every Next.js application benefits
from Tanod, and it is not a general site-performance benchmark.

The `pages/` half is not there for variety. A Pages Router page answers the
same page in two shapes — the document at `/legacy/x` and a JSON props payload
at `/_next/data/<buildId>/legacy/x.json` — which is the same class of hazard as
the App Router's RSC variant reached by a different mechanism. `/legacy/session`
sets a cookie from `getServerSideProps` under a deliberately permissive route
policy, so the `Set-Cookie` barrier has to hold on the legacy code path too.

`public/` carries two images on purpose. `product.svg` is what the pages
actually render; `bands.png` exists only so `bench/nextjs.sh` can exercise
`/_next/image`, and it is a raster file because Next's image optimiser refuses
SVG outright (`dangerouslyAllowSVG` defaults to false) and answers `400` with
no `Vary` header — an SVG-only fixture cannot reach the optimiser's success
path at all. It is eight flat colour bands at 1280x960 so that `w=640` is a
genuine downscale while the file still compresses to about 5KB, which is the
most a binary committed to a git repository should cost.

It is an integration origin, not a benchmark simulator. Each expensive route
emits JSON `render_start` and `render_end` records carrying an instance id and a
unique render id. Tanod's Prometheus origin counter is the machine-readable
witness used by `bench/nextjs.sh` across all three origins.

Two surfaces exist only for the overload showcase
([`bench/showcase`](../../bench/showcase), [method](../../docs/demo/README.md))
and are off unless their environment variable is set, so no other suite sees
them. `TANOD_FIXTURE_STATS=1` enables `/api/fixture-stats`, which reports the
renders in progress, the total count and, for each named `reader`, the peak
since that reader last asked (JSON, or Prometheus text with
`?format=prometheus`). `TANOD_FIXTURE_CPU_ROUTE=1` enables `/catalog/[slug]`,
which does a fixed amount of CPU work per request (`CATALOG_WORK_UNITS`,
default 40,000 synthetic items built, faceted and ranked) instead of the
artificial delay used by every other route. The delay is a mechanism test: it
occupies no CPU, so it never saturates the origin.

Product responses also emit the public `X-Tanod-Cache-Tags` contract. The
fixture therefore exercises tag invalidation without Next.js private minimal
mode, which disables the image optimiser covered by the same scenario.

Run the complete scenario from the repository root:

```bash
./bench/nextjs.sh          # HTTP assertions
./bench/nextjs-browser.sh  # the same stack, driven by Chromium
```

The compatibility runner repeats all HTTP, browser, reference, and replica
checks for a declared Next.js/Node pair. Build arguments select the framework
and Node release without rewriting the checked-in fixture or lockfile:

```bash
TANOD_NEXT_VERSION=16.3.8 TANOD_NEXT_NODE=24 bash bench/nextjs-compatibility.sh
```

CI takes the required pairs from
[`compatibility.json`](../../packages/tanod-next/src/compatibility.json).
The runner uses a separate Compose project and origin image for each pair,
and writes version targets and suite logs under `compatibility-results/`.
Runs on one host are sequential because the fixture uses fixed loopback ports.
On Linux hosts outside Playwright's supported distributions, set
`TANOD_BROWSER_DOCKER=1` to run Chromium and the same harness in the official
Playwright image matching the pinned harness dependency. This uses host
networking and does not install system packages.
Linux hosts without `apt-get` use this mode automatically; set
`TANOD_BROWSER_DOCKER=0` to explicitly use the native browser installer.
If the native browser's system dependencies are already installed, set
`TANOD_BROWSER_INSTALL_DEPS=0` to download Chromium without changing system
packages. Browser assertions still run and fail normally in either mode.

The browser script covers the two requests curl cannot construct: a router
prefetch carrying a real `Next-Router-State-Tree`, and a Server Action POST
carrying an action id this build assigned.

Or inspect it manually:

```bash
docker compose -f compose.nextjs.yaml up --build
curl -i http://127.0.0.1:18080/products/atlas-runner
```

The edge publishes the traffic port; origin and admin ports bind only to
loopback for local diagnostic checks. Keep those diagnostic ports private in
production.
