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

Product responses also emit the public `X-Tanod-Cache-Tags` contract. The
fixture therefore exercises tag invalidation without Next.js private minimal
mode, which disables the image optimiser covered by the same scenario.

Run the complete scenario from the repository root:

```bash
./bench/nextjs.sh          # HTTP assertions
./bench/nextjs-browser.sh  # the same stack, driven by Chromium
```

The browser script covers the two requests curl cannot construct: a router
prefetch carrying a real `Next-Router-State-Tree`, and a Server Action POST
carrying an action id this build assigned.

Or inspect it manually:

```bash
docker compose -f compose.nextjs.yaml up --build
curl -i http://127.0.0.1:18080/products/atlas-runner
```

Only Tanod publishes a host port. The three Next.js services are reachable
only inside the Compose network, matching the intended production topology.
