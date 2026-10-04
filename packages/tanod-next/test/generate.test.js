import { test } from 'node:test';
import assert from 'node:assert/strict';
import { FIXTURE, REAL_BUILD } from './fixture.js';
import { readBuild } from '../src/manifests.js';
import { generateConfig, routeId, toGlob } from '../src/routes.js';
import { quote } from '../src/yaml.js';
import { SUPPORTED_MANIFESTS, VERIFIED_NEXT_RELEASES } from '../src/compat.js';

const build = await readBuild(FIXTURE);

test('reads a real Next build', () => {
  assert.match(build.buildId, /^[\w-]+$/);
  assert.ok(build.staticRoutes.length > 0);
});

test('the compatibility matrix matches the build it claims to support', () => {
  // The matrix is only worth having if it is checked against a real build
  // rather than transcribed by hand.
  assert.ok(
    SUPPORTED_MANIFESTS['routes-manifest.json'].includes(build.manifestVersions.routes),
    `routes-manifest v${build.manifestVersions.routes} is not in the supported list`,
  );
  assert.ok(
    SUPPORTED_MANIFESTS['prerender-manifest.json'].includes(build.manifestVersions.prerender),
    `prerender-manifest v${build.manifestVersions.prerender} is not in the supported list`,
  );
  const claimed = VERIFIED_NEXT_RELEASES[0];
  assert.equal(claimed.routesManifest, build.manifestVersions.routes);
  assert.equal(claimed.prerenderManifest, build.manifestVersions.prerender);
});

test('a missing build is refused with an explanation, not guessed at', async () => {
  await assert.rejects(() => readBuild('./does-not-exist'), { name: 'TanodNextError' });
});

test(
  'the committed fixture still matches the storefront it was copied from',
  { skip: REAL_BUILD ? false : 'no local next build; the committed fixture is the source of truth' },
  async () => {
    // Guards the drift the committed copy makes possible: a Next upgrade that
    // changes the manifests would leave this suite testing a format nothing
    // emits any more.
    const live = await readBuild(REAL_BUILD);
    assert.deepEqual(
      live.manifestVersions,
      build.manifestVersions,
      'the storefront now builds different manifest versions; refresh test/fixtures/next-build',
    );
  },
);

// --------------------------------------------------------------- route globs

test('dynamic segments become the glob Tanod actually compiles', () => {
  assert.equal(toGlob('/products/[slug]'), '/products/*');
  assert.equal(toGlob('/blog/[...slug]'), '/blog/**');
  assert.equal(toGlob('/a/[x]/b/[y]'), '/a/*/b/*');
  assert.equal(toGlob('/static'), '/static');
});

test('an optional catch-all also matches its bare parent, as it does in Next', () => {
  // `/shop{,/**}` would NOT match `/shop`; the alternate form does. Verified
  // against globset, which is what Tanod compiles with.
  assert.equal(toGlob('/shop/[[...slug]]'), '{/shop,/shop/**}');
});

test('route ids are readable, safe and unique', () => {
  const taken = new Set();
  assert.equal(routeId('/products/[slug]', taken), 'products-slug');
  assert.equal(routeId('/products/[slug]', taken), 'products-slug-2');
  assert.equal(routeId('/', taken), 'root');
  assert.match(routeId('/weird/!!path', taken), /^[a-z0-9-]+$/);
});

// ------------------------------------------------------------ generated YAML

const yaml = generateConfig(build, { upstreams: ['next-1:3000'] });

/**
 * The generated document with its comments removed.
 *
 * The comments deliberately contain sample configuration — "change it to
 * class: public_ssr" — so any assertion that greps the raw output reads those
 * samples as real settings. Stripping first is the difference between testing
 * the generator and testing its prose.
 */
const config = yaml
  .split('\n')
  .filter((line) => !/^\s*#/.test(line))
  .join('\n');

const entries = () => config.split(/\n  - id: /).slice(1);

test('the build id becomes the deployment id', () => {
  assert.ok(yaml.includes(`id: "${build.buildId}"`), yaml.slice(0, 400));
});

test('a stable Next deployment id takes precedence over the data build id', () => {
  const withDeployment = { ...build, deploymentId: 'storefront-production', identity: 'storefront-production' };
  const output = generateConfig(withDeployment, { upstreams: ['next-1:3000'] });
  assert.match(output, /deployment:\n  id: "storefront-production"/);
  assert.match(output, new RegExp(`/_next/data/${build.buildId.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}/`));
});

test('the image route carries the Accept vary that makes it cache at all', () => {
  // Without Accept in the key Tanod refuses to store the response and the
  // route gets a 0% hit rate. Generating it wrong would be worse than not
  // generating it at all.
  assert.match(yaml, /match: "\/_next\/image"[\s\S]*?headers: \["Accept"\]/);
});

test('prerendered routes are the only ones granted a public class', () => {
  // The safety property of the whole generator: the build proves a prerendered
  // route is one response for everybody. Nothing else is proof of anything, so
  // nothing else may be public.
  const prerendered = new Set(
    Object.keys(build.prerendered).filter((route) => !route.startsWith('/_')),
  );
  for (const entry of entries()) {
    if (!/class: public_ssr/.test(entry)) continue;
    const match = entry.match(/match: "([^"]+)"/)?.[1];
    assert.ok(
      prerendered.has(match),
      `${match} was generated public_ssr but the build does not say it is prerendered`,
    );
  }
});

test('every dynamically rendered page is private', () => {
  for (const route of build.dynamicRoutes) {
    const glob = toGlob(route.page);
    const entry = entries().find((e) => e.includes(`match: "${glob}"`));
    assert.ok(entry, `no route generated for ${glob}`);
    assert.match(entry, /class: private_dynamic/);
  }
});

test('the catch-all is last, because first match wins', () => {
  const ids = [...config.matchAll(/^  - id: "([^"]+)"/gm)].map((m) => m[1]);
  assert.equal(ids.at(-1), 'default');
});

test('route ids in the output are unique', () => {
  const ids = [...config.matchAll(/^  - id: "([^"]+)"/gm)].map((m) => m[1]);
  assert.equal(new Set(ids).size, ids.length);
});

test('routes-only output omits what a fragment must not carry', () => {
  // Matched at the start of a line: `override_origin:` is a real setting
  // inside a route and must not read as a top-level `origin:` block.
  const fragment = generateConfig(build, { includeDeployment: false })
    .split('\n')
    .filter((line) => !/^\s*#/.test(line));
  assert.ok(!fragment.some((line) => line === 'deployment:'));
  assert.ok(!fragment.some((line) => line === 'origin:'));
  assert.ok(fragment.some((line) => line === 'routes:'));
});

// ------------------------------------------------------------------- quoting

test('a value from a manifest cannot break out of its YAML position', () => {
  assert.equal(quote('a"b'), '"a\\"b"');
  assert.equal(quote('a\nb'), '"a\\nb"');
  assert.equal(quote(`x\u0007y`), '"x\\x07y"');
  // The shape that would matter: a route id trying to close its string and
  // open a new key.
  assert.equal(quote('a"\n    class: static'), '"a\\"\\n    class: static"');
});
