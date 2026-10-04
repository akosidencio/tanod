import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { mkdtemp } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const SharedCacheHandler = require(
  fileURLToPath(new URL('../../../fixtures/next-storefront/cache-handler.cjs', import.meta.url)),
);

test('shared cache tag invalidations merge across concurrent origins', async () => {
  const root = await mkdtemp(path.join(tmpdir(), 'tanod-next-cache-'));
  const handlers = Array.from({ length: 8 }, () => {
    const handler = new SharedCacheHandler();
    handler.root = root;
    handler.entries = path.join(root, 'entries');
    handler.tagsFile = path.join(root, 'tags.json');
    handler.tagsLock = path.join(root, 'tags.lock');
    return handler;
  });

  await Promise.all(handlers.map((handler, index) => handler.revalidateTag(`tag-${index}`)));
  const tags = await handlers[0].tags();
  assert.deepEqual(Object.keys(tags).sort(), handlers.map((_, index) => `tag-${index}`).sort());
});
