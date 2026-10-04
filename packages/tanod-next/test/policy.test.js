import { test } from 'node:test';
import assert from 'node:assert/strict';

import { FIXTURE } from './fixture.js';
import { readBuild } from '../src/manifests.js';
import { parsePolicy, validatePolicy } from '../src/policy.js';
import { generateConfig, inspectRoutes } from '../src/routes.js';
import { calibrate, doctor, explainRequest } from '../src/workflows.js';

const build = await readBuild(FIXTURE);

const source = `
version: 1
routes:
  /products/[slug]:
    privacy: public
    weight: 3
    priority: high
    methods: [GET, HEAD]
    cache:
      ttl: 2s
      stale_if_error: 1m
      query: [ref, page]
      vary: [Accept-Language]
  /account:
    privacy: private
    weight: 2
`;

test('a checked-in policy makes an explicitly asserted dynamic page public', () => {
  const policy = validatePolicy(parsePolicy(source), build);
  const yaml = generateConfig(build, { upstreams: ['next:3000'], policy });
  const product = yaml.split(/\n  - id: /).find((entry) => entry.includes('match:') && entry.includes('/products/*'));
  assert.match(product, /class: public_ssr/);
  assert.match(product, /weight: 3/);
  assert.match(product, /priority: high/);
  assert.match(product, /methods: \["GET", "HEAD"\]/);
  assert.match(product, /max: 2s/);
  assert.match(product, /keys: \["ref", "page"\]/);
  assert.match(product, /headers: \["Accept-Language"\]/);
});

test('inspect names operator evidence and highlights a public override', () => {
  const rows = inspectRoutes(build, validatePolicy(parsePolicy(source), build));
  const product = rows.find((row) => row.route === '/products/[slug]');
  assert.equal(product.privacy, 'public');
  assert.equal(product.reuse, 'cache 2s');
  assert.equal(product.evidence, 'operator assertion');
  assert.equal(product.publicOverride, true);
});

test('Pages Router data payloads inherit the document policy', () => {
  const policy = validatePolicy(parsePolicy(`
version: 1
routes:
  /legacy/[slug]:
    privacy: public
    cache:
      ttl: 2s
`), build);
  const yaml = generateConfig(build, { upstreams: ['next:3000'], policy });
  assert.match(yaml, /match: "\/_next\/data\/[^"]+\/legacy\/\*\.json"[\s\S]*?class: public_ssr/);
  const exact = yaml.indexOf('match: "/legacy/session"');
  const dynamic = yaml.indexOf('match: "/legacy/*"');
  assert.ok(exact >= 0 && dynamic > exact, 'exact private route must precede a public dynamic glob');
});

test('rollout stages preserve privacy while changing reuse behavior', () => {
  const policy = validatePolicy(parsePolicy(source), build);
  const observe = generateConfig(build, { upstreams: ['next:3000'], policy, rollout: 'observe' });
  const protect = generateConfig(build, { upstreams: ['next:3000'], policy, rollout: 'protect' });
  const coalesce = generateConfig(build, { upstreams: ['next:3000'], policy, rollout: 'coalesce' });
  assert.match(observe, /^mode: observe$/m);
  assert.match(protect, /^cache:\n  enabled: false\ncoalesce:\n  enabled: false$/m);
  assert.match(coalesce, /^cache:\n  enabled: false\ncoalesce:\n  enabled: true$/m);
  assert.match(coalesce, /class: public_ssr[\s\S]*?override_origin: true[\s\S]*?coalesce:[\s\S]*?override_origin: true/);
});

test('unknown fields and unmatched routes are refused', () => {
  assert.throws(
    () => parsePolicy('version: 1\nroutes:\n  /search:\n    privacy: public\n    surprise: yes\n'),
    /unknown route field/,
  );
  assert.throws(
    () => validatePolicy(parsePolicy('version: 1\nroutes:\n  /missing:\n    privacy: private\n'), build),
    /found 0/,
  );
});

test('unsafe public assertions fail closed', () => {
  assert.throws(
    () => validatePolicy(parsePolicy('version: 1\nroutes:\n  /search:\n    privacy: public\n    methods: [POST]\n    cache:\n      ttl: 1s\n'), build),
    /unsafe cacheable method/,
  );
  assert.throws(
    () => validatePolicy(parsePolicy('version: 1\nroutes:\n  /search:\n    privacy: public\n    cache:\n      ttl: 1s\n      vary: [Cookie]\n'), build),
    /credential, or unbounded/,
  );
  assert.throws(
    () => validatePolicy(parsePolicy('version: 1\nroutes:\n  /search:\n    privacy: private\n    cache:\n      ttl: 1s\n'), build),
    /private and cannot enable/,
  );
});

test('redundant prerender and public route-handler assertions are refused', () => {
  assert.throws(
    () => validatePolicy(parsePolicy('version: 1\nroutes:\n  /:\n    privacy: public\n    cache:\n      ttl: 1s\n'), build),
    /already public/,
  );
  assert.throws(
    () => validatePolicy(parsePolicy('version: 1\nroutes:\n  /api/draft:\n    privacy: public\n    cache:\n      ttl: 1s\n'), build),
    /route handler.*cannot be declared public/,
  );
});

test('explain is local, shows barriers, and redacts credentials', () => {
  const policy = validatePolicy(parsePolicy(source), build);
  const explanation = explainRequest(build, policy, {
    url: 'https://shop.example/products/one?ref=email&ignored=random',
    method: 'GET',
    headers: ['Authorization: bearer secret', 'Accept-Language: en'],
  });
  assert.equal(explanation.route, '/products/[slug]');
  assert.equal(explanation.reuse, 'bypass');
  assert.deepEqual(explanation.cacheKeyInputs.query, [['ref', 'email']]);
  assert.equal(explanation.request.headers.authorization, '<redacted>');
  assert.ok(explanation.barriers.includes('Authorization header'));
});

test('calibration requires explicit load authorization and route scope', async () => {
  await assert.rejects(() => calibrate({ target: 'http://127.0.0.1:3000', routes: ['/'] }), /--allow-load/);
  await assert.rejects(() => calibrate({ allowLoad: true, target: 'http://127.0.0.1:3000' }), /--route/);
  await assert.rejects(
    () => calibrate({ allowLoad: true, target: 'https://example.com', routes: ['/'] }),
    /--allow-production/,
  );
});

test('doctor cannot report success when every deployment check was skipped', async () => {
  const result = await doctor(build);
  assert.equal(result.ok, false);
  assert.ok(result.checks.every((check) => check.status === 'skip'));
});
