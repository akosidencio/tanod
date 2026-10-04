import { test } from 'node:test';
import assert from 'node:assert/strict';
import { cp, mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

import { createPurger } from '../src/purge.js';

/** A purger whose requests are captured instead of sent. */
function stub({ status = 200, body = '{"purged":true,"entries":3}' } = {}) {
  const calls = [];
  const purger = createPurger({
    endpoint: 'http://127.0.0.1:9091',
    token: 'test-token-0123456789abcdef',
    fetch: async (url, init) => {
      calls.push({ url: new URL(url), init });
      return new Response(body, { status, headers: { 'content-type': 'application/json' } });
    },
  });
  return { purger, calls };
}

test('the token travels in a header, never in the URL', async () => {
  // A query string is logged by every proxy, load balancer and access log on
  // the path. A purge token in one is a purge token in a log aggregator.
  const { purger, calls } = stub();
  await purger.purgeTags(['product-42']);
  const [call] = calls;
  assert.equal(call.init.headers.authorization, 'Bearer test-token-0123456789abcdef');
  assert.ok(!call.url.search.includes('token'));
  assert.ok(!call.url.href.includes('test-token'));
});

test('redirects are refused rather than followed', async () => {
  // Following one would re-send the Authorization header to whatever host the
  // redirect names.
  const { purger, calls } = stub();
  await purger.purgeTags(['a']);
  assert.equal(calls[0].init.redirect, 'manual');

  const redirecting = stub({ status: 302 });
  await assert.rejects(() => redirecting.purger.purgeTags(['a']), /redirect/);
});

test('it POSTs, because a GET that invalidates a cache gets crawled', async () => {
  const { purger, calls } = stub();
  await purger.purgeTags(['a']);
  assert.equal(calls[0].init.method, 'POST');
  assert.equal(calls[0].url.pathname, '/purge');
});

test('values are encoded once and delimiters cannot alter the purge query', async () => {
  const { purger, calls } = stub();
  await purger.purge({
    tags: ['sale & 100%'],
    paths: ['/search?q=a&b#results'],
  });
  const [call] = calls;
  assert.deepEqual(call.url.searchParams.getAll('tag'), ['sale & 100%']);
  assert.deepEqual(call.url.searchParams.getAll('path'), ['/search?q=a&b#results']);
  assert.match(call.url.search, /tag=sale%20%26%20100%25/);
  assert.match(call.url.search, /path=%2Fsearch%3Fq%3Da%26b%23results/);
});

test('tags and paths travel in one request', async () => {
  const { purger, calls } = stub();
  await purger.purge({ tags: ['sale'], paths: ['/p/1'] });
  assert.equal(calls.length, 1);
  assert.equal(calls[0].url.search, '?tag=sale&path=%2Fp%2F1');
});

test('a replicated purge reaches every local Tanod cache', async () => {
  const calls = [];
  const purger = createPurger({
    endpoints: ['http://127.0.0.1:9091', 'http://127.0.0.1:9092'],
    token: 'test-token-0123456789abcdef',
    fetch: async (url) => {
      calls.push(new URL(url));
      return new Response('{"purged":true,"entries":2}', {
        headers: { 'content-type': 'application/json' },
      });
    },
  });
  const result = await purger.purgeTags(['product-42']);
  assert.deepEqual(calls.map((url) => url.origin).sort(), [
    'http://127.0.0.1:9091',
    'http://127.0.0.1:9092',
  ]);
  assert.equal(result.replicas, 2);
  assert.equal(result.entries, 4);
  assert.equal(result.results.length, 2);
});

test('a partial replicated purge is reported as a failure', async () => {
  const purger = createPurger({
    endpoints: ['http://127.0.0.1:9091', 'http://127.0.0.1:9092'],
    token: 'test-token-0123456789abcdef',
    fetch: async (url) => new Response(
      new URL(url).port === '9092' ? 'unavailable' : '{"purged":true,"entries":1}',
      { status: new URL(url).port === '9092' ? 503 : 200 },
    ),
  });
  await assert.rejects(() => purger.purgeTags(['product-42']), /127\.0\.0\.1:9092.*503/);
});

test('duplicates collapse', async () => {
  const { purger, calls } = stub();
  await purger.purgeTags(['a', 'a', 'b']);
  assert.equal(calls[0].url.search, '?tag=a&tag=b');
});

test('purging everything is spelled out', async () => {
  const { purger, calls } = stub();
  await purger.purgeAll();
  assert.equal(calls[0].url.search, '?all=1');
});

test('a value that could not have matched is refused', async () => {
  const { purger, calls } = stub();
  // A comma can never appear in a stored tag: the tag header is
  // comma-separated, so Tanod split it before indexing.
  await assert.rejects(() => purger.purgeTags(['a,b']), /comma/);
  // The stored path always begins with a slash.
  await assert.rejects(() => purger.purgePaths(['products']), /must be absolute/);
  assert.equal(calls.length, 0, 'a refused purge must not reach the network');
});

test('an empty list is a no-op rather than a purge of everything', async () => {
  // The dangerous shape: purgeTags([]) turning into "purge all".
  const { purger, calls } = stub();
  const result = await purger.purgeTags([]);
  assert.equal(result.entries, 0);
  assert.equal(calls.length, 0);
  await assert.rejects(() => purger.purge({}), /at least one/);
});

test('a failed purge throws rather than leaving stale content served quietly', async () => {
  const failing = stub({ status: 401, body: 'unauthorized' });
  await assert.rejects(() => failing.purger.purgeTags(['a']), /401/);
});

test('a malformed HTTP success response throws rather than pretending the purge worked', async () => {
  await assert.rejects(
    () => stub({ body: '<html>not the admin listener</html>' }).purger.purgeTags(['a']),
    /invalid JSON/,
  );
  await assert.rejects(
    () => stub({ body: '{}' }).purger.purgeTags(['a']),
    /invalid success body/,
  );
  await assert.rejects(
    () => stub({ body: '{"purged":false,"entries":0}' }).purger.purgeTags(['a']),
    /invalid success body/,
  );
  await assert.rejects(
    () => stub({ body: '{"purged":true,"entries":"3"}' }).purger.purgeTags(['a']),
    /invalid success body/,
  );
});

test('revalidateTag immediately expires Next before purging Tanod', async (t) => {
  const dir = await mkdtemp(path.join(tmpdir(), 'tanod-next-cache-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  await cp(fileURLToPath(new URL('../src', import.meta.url)), path.join(dir, 'src'), {
    recursive: true,
  });
  const nextDir = path.join(dir, 'node_modules/next');
  await mkdir(nextDir, { recursive: true });
  await writeFile(
    path.join(nextDir, 'package.json'),
    JSON.stringify({ name: 'next', type: 'module', exports: { './cache': './cache.js' } }),
  );
  await writeFile(
    path.join(nextDir, 'cache.js'),
    'export const revalidateTag = (...args) => globalThis.__tanodNextTagCalls.push(args);\n',
  );

  globalThis.__tanodNextTagCalls = [];
  t.after(() => delete globalThis.__tanodNextTagCalls);
  const copied = await import(`${pathToFileURL(path.join(dir, 'src/purge.js'))}?test=${Date.now()}`);
  const fetch = async () => new Response('{"purged":true,"entries":1}');
  const base = { endpoint: 'http://127.0.0.1:9091', token: 'token', fetch };

  await copied.revalidateTag('product-42', base);
  await copied.revalidateTag('product-43', { ...base, nextProfile: 'max' });
  assert.deepEqual(globalThis.__tanodNextTagCalls, [
    ['product-42', { expire: 0 }],
    ['product-43', 'max'],
  ]);
});

test('it refuses to start without an endpoint or a token', () => {
  assert.throws(() => createPurger({ endpoint: '', token: 't' }), /endpoint/);
  assert.throws(() => createPurger({ endpoint: 'http://x', token: '' }), /token/);
});
