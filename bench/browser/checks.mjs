// Browser-driven checks against the Next.js fixture behind Tanod.
//
// Everything else in bench/ speaks to the proxy with curl. That is enough for
// the claims that are about HTTP, but two of Next.js's behaviours are not
// really about HTTP at all — they are about what the framework's own client
// decides to send:
//
//   * A prefetch is issued by the router, with a `Next-Router-State-Tree` that
//     encodes the entire client route state. Hand-writing one proves that a
//     header Tanod invented is handled; it does not prove the real thing is.
//   * A Server Action submission is a POST the React runtime builds, with an
//     action id the build assigns. It cannot be written down in advance,
//     because it changes every build.
//
// So these two are driven by a real browser, and the requests asserted on are
// the ones Chromium actually sent.

import { chromium } from "playwright";

const PROXY = process.env.PROXY_URL || "http://127.0.0.1:18080";
const METRICS = process.env.METRICS_URL || "http://127.0.0.1:19090";

let failures = 0;
let failuresInCheck = 0;

function beginCheck() {
  failuresInCheck = 0;
}

/// Only reports success if nothing failed inside the current check. Printing
/// PASS unconditionally at the end of a function that has already logged FAIL
/// is exactly the kind of report this whole phase exists to remove.
function pass(message) {
  if (failuresInCheck > 0) return;
  console.log(`PASS: ${message}`);
}

function fail(message) {
  console.error(`FAIL: ${message}`);
  failures += 1;
  failuresInCheck += 1;
}

function assert(condition, message) {
  if (condition) return true;
  fail(message);
  return false;
}

/// Origin requests for one route, read from Tanod's own counter. The Next
/// fixture reports render ids in the body too, but a prefetch payload has no
/// body a test can read, so the counter is the observable that covers both.
async function originRequests(route) {
  const text = await (await fetch(`${METRICS}/metrics`)).text();
  let sum = 0;
  for (const line of text.split("\n")) {
    if (!line.startsWith("tanod_origin_requests_total{")) continue;
    if (!line.includes(`route="${route}"`)) continue;
    sum += Number(line.trim().split(/\s+/).pop());
  }
  return sum;
}

// The link the homepage renders, and the route whose origin counter must move
// when its prefetch is replayed. The two have to be named together: reading a
// different route's counter is how this check first reported "0 renders" while
// the router had in fact prefetched something else.
const PREFETCH_PATH = "/products/atlas-runner";
const PREFETCH_ROUTE = "products";

async function checkPrefetch(browser) {
  beginCheck();
  const context = await browser.newContext();
  const page = await context.newPage();

  // Record every request the router issues, not just the document.
  const requests = [];
  page.on("request", (request) => {
    requests.push({
      url: request.url(),
      method: request.method(),
      headers: request.headers(),
    });
  });

  await page.goto(`${PROXY}/`, { waitUntil: "networkidle" });
  // The homepage's product link is in the viewport, so the App Router
  // prefetches it. Hovering is belt and braces for a headless viewport.
  await page.hover(`a[href="${PREFETCH_PATH}"]`).catch(() => {});
  await page.waitForTimeout(1500);

  const prefetches = requests.filter(
    (request) =>
      request.headers["next-router-prefetch"] !== undefined ||
      request.headers["next-router-segment-prefetch"] !== undefined,
  );

  // The router prefetches more than the link — its own segments among them —
  // so the one this check is about has to be selected by path rather than
  // taken as whichever arrived first.
  const prefetch = prefetches.find(
    (request) => new URL(request.url).pathname === PREFETCH_PATH,
  );

  if (
    !assert(
      prefetch !== undefined,
      `the router issued no prefetch for ${PREFETCH_PATH}; it prefetched ` +
        (prefetches.length
          ? prefetches.map((r) => new URL(r.url).pathname).join(", ")
          : "nothing at all"),
    )
  ) {
    await context.close();
    return;
  }
  assert(
    prefetch.headers.rsc !== undefined,
    "a prefetch arrived without the RSC header, so it would not be keyed as a flight payload",
  );

  // Replay the browser's own prefetch, twice over, in the two arrangements
  // that pin down what "coalesced but never stored" means. A prefetch payload
  // is keyed on the router state tree — near-unbounded cardinality — so it is
  // worth collapsing a burst of them and never worth keeping one.
  const replayHeaders = { ...prefetch.headers };
  delete replayHeaders["content-length"];
  // The body has to be drained, not just awaited: `fetch` resolves at the
  // response headers, so an un-consumed reply is still an in-flight origin
  // render, and a "sequential" replay issued on top of it is really a
  // concurrent one. Getting this wrong makes the store look like it retained
  // something when it had only coalesced.
  const replay = async () => {
    const response = await fetch(prefetch.url, { headers: replayHeaders });
    await response.arrayBuffer();
    return response;
  };

  const beforeBurst = await originRequests(PREFETCH_ROUTE);
  await Promise.all([replay(), replay(), replay(), replay()]);
  const afterBurst = await originRequests(PREFETCH_ROUTE);
  assert(
    afterBurst - beforeBurst === 1,
    `four concurrent replays of the browser's own prefetch cost ${afterBurst - beforeBurst} origin renders, expected 1`,
  );

  // Past the store's handoff window, nothing may remain: the next request has
  // to render again. A retained prefetch payload would be an unbounded key
  // space living in a bounded cache.
  await page.waitForTimeout(500);
  const beforeSecond = await originRequests(PREFETCH_ROUTE);
  await replay();
  const afterSecond = await originRequests(PREFETCH_ROUTE);
  assert(
    afterSecond - beforeSecond === 1,
    `a replay issued after the flight had finished cost ${afterSecond - beforeSecond} origin renders, expected 1; ` +
      "a prefetch payload was retained",
  );

  pass(
    `the App Router prefetched ${new URL(prefetch.url).pathname}; four concurrent replays cost one render, and a later one rendered again`,
  );
  await context.close();
}

async function checkServerAction(browser) {
  beginCheck();
  const context = await browser.newContext();
  const page = await context.newPage();

  const posts = [];
  page.on("request", (request) => {
    if (request.method() === "POST") {
      posts.push({ url: request.url(), headers: request.headers() });
    }
  });

  const readCount = async () => {
    const text = await page.textContent("main");
    const match = /Items:\s*(\d+)/.exec(text || "");
    return match ? Number(match[1]) : null;
  };

  await page.goto(`${PROXY}/cart`, { waitUntil: "networkidle" });
  const start = await readCount();
  if (!assert(start !== null, "could not read the cart count from /cart")) {
    await context.close();
    return;
  }

  const before = await originRequests("private-routes");

  // Two real submissions of a real Server Action form. If either the response
  // or the mutation itself were reused, the count would stop advancing — which
  // is the failure this exists to catch, and the reason a mutation may never
  // be cached or coalesced.
  for (let index = 1; index <= 2; index += 1) {
    await page.click('button[type="submit"]');
    await page.waitForFunction(
      (expected) => {
        const text = document.querySelector("main")?.textContent || "";
        const match = /Items:\s*(\d+)/.exec(text);
        return match ? Number(match[1]) === expected : false;
      },
      start + index,
      { timeout: 10_000 },
    ).catch(() => {});
    const observed = await readCount();
    assert(
      observed === start + index,
      `after submission ${index} the cart read ${observed}, expected ${start + index}; ` +
        "a Server Action response was reused",
    );
  }

  const after = await originRequests("private-routes");
  assert(
    posts.length >= 2,
    `the browser sent ${posts.length} POST requests, expected at least two form submissions`,
  );
  assert(
    posts.every((post) => post.headers["next-action"] !== undefined),
    "a form submission arrived without a Next-Action header, so it would not classify as a mutation",
  );
  assert(
    after - before >= 2,
    `two Server Action submissions reached the origin ${after - before} times; each must render`,
  );

  pass(
    `two real Server Action submissions advanced the cart to ${start + 2}, each reaching the origin`,
  );
  await context.close();
}

const browser = await chromium.launch();
try {
  await checkPrefetch(browser);
  await checkServerAction(browser);
} finally {
  await browser.close();
}

if (failures > 0) {
  console.error(`\n${failures} browser check(s) failed`);
  process.exit(1);
}
console.log("\nPASS: browser-driven Next.js checks completed");
