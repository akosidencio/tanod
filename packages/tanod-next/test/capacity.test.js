import { test } from 'node:test';
import assert from 'node:assert/strict';

import { resolveCapacity } from '../src/capacity.js';
import { readBuild } from '../src/manifests.js';
import { generateConfig } from '../src/routes.js';
import { FIXTURE } from './fixture.js';

const build = await readBuild(FIXTURE);

test('a global budget is partitioned conservatively across replicas', () => {
  assert.deepEqual(resolveCapacity({ globalConcurrency: 9, replicas: 2 }), {
    concurrency: 4,
    group: {
      globalMax: 9,
      replicas: 2,
      allocated: 8,
      unallocated: 1,
    },
  });
  const yaml = generateConfig(build, {
    upstreams: ['next-1:3000'],
    globalConcurrency: 9,
    replicas: 2,
  });
  assert.match(yaml, /concurrency:\n    max: 4/);
  assert.match(yaml, /capacity:\n  global_max: 9\n  replicas: 2/);
  assert.match(yaml, /match: "\/_next\/image"[\s\S]*?priority: low[\s\S]*?weight: 3/);
});

test('static partition inputs are complete and mutually exclusive', () => {
  assert.throws(() => resolveCapacity({ globalConcurrency: 8 }), /both/);
  assert.throws(() => resolveCapacity({ replicas: 2 }), /both/);
  assert.throws(
    () => resolveCapacity({ concurrency: 4, globalConcurrency: 8, replicas: 2 }),
    /either/,
  );
  assert.throws(
    () => resolveCapacity({ globalConcurrency: 1, replicas: 2 }),
    /cannot be partitioned/,
  );
});

test('complete output still requires an explicit local or group ceiling', () => {
  assert.throws(() => resolveCapacity({}, { requireExplicit: true }), /requires/);
});
