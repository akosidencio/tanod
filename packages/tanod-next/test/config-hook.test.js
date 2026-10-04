import { test } from 'node:test';
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { mkdtemp, readFile, readdir, writeFile } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';

import { BINARY, FIXTURE, ROOT } from './fixture.js';

const run = promisify(execFile);
const PKG = fileURLToPath(new URL('..', import.meta.url));

/**
 * Drive the hook the way Next does: load the config, ask it for the
 * production-build phase, then let the process exit.
 *
 * A subprocess rather than an in-process call, because the whole mechanism is
 * `process.on('exit')` and there is no way to observe that without a process
 * actually ending.
 */
async function runHook(hookOptions, { fail = false, initial } = {}) {
  const dir = await mkdtemp(path.join(tmpdir(), 'tanod-hook-'));
  const out = path.join(dir, 'tanod.yaml');
  const script = path.join(dir, 'run.mjs');
  if (initial !== undefined) await writeFile(out, initial);
  await writeFile(
    script,
    `import { withTanod } from ${JSON.stringify(path.join(PKG, 'src/config.js'))};\n` +
      `const config = withTanod({ output: 'standalone' }, ${JSON.stringify({ out, ...hookOptions })});\n` +
      `config('phase-production-build', {});\n` +
      (fail ? 'process.exitCode = 7;\n' : ''),
  );
  const result = await run(process.execPath, [script], { cwd: dir }).catch((e) => e);
  return { out, dir, result };
}

test('a production build writes the config on the way out', async () => {
  const { out, dir, result } = await runHook({
    distDir: FIXTURE,
    upstreams: ['next-1:3000'],
    concurrency: 8,
  });
  assert.equal(result.code ?? 0, 0, result.stderr);
  const yaml = await readFile(out, 'utf8');
  const identity = JSON.parse(await readFile(path.join(dir, 'tanod.deployment.json'), 'utf8'));
  assert.match(yaml, /^version: 1$/m);
  assert.match(yaml, /^deployment:$/m);
  assert.equal(identity.build_id, identity.next_build_id);
  assert.match(identity.build_fingerprint, /^[a-f0-9]{64}$/);
  assert.equal(identity.policy_fingerprint, 'none');
  assert.equal(identity.rollout, 'cache');
  assert.match(result.stdout, /@tanod\/next: wrote/);
});

test('silent means silent', async () => {
  const { result } = await runHook({
    distDir: FIXTURE,
    upstreams: ['x:3000'],
    concurrency: 8,
    silent: true,
  });
  assert.equal(result.code ?? 0, 0, result.stderr);
  assert.equal(result.stdout, '');
});

test('a failed build generates nothing', async () => {
  // Generating a route policy from a half-finished build would leave a stale
  // file that looks current.
  const { out, result } = await runHook(
    { distDir: FIXTURE, upstreams: ['next-1:3000'], concurrency: 8 },
    { fail: true },
  );
  assert.equal(result.code, 7, "the build's own exit code must survive");
  assert.ok(!existsSync(out), 'a failed build wrote a config anyway');
});

test('a build that cannot be read fails rather than warns', async () => {
  const { result } = await runHook({ distDir: path.join(ROOT, 'no-such-dir') });
  assert.equal(result.code, 1);
  assert.match(result.stderr, /could not read/);
});

test(
  'a configuration Tanod would reject fails the build that produced it',
  { skip: BINARY ? false : 'tanod binary not built' },
  async () => {
    const { result } = await runHook({
      distDir: FIXTURE,
      check: true,
      tanodBin: BINARY,
      upstreams: [],
    });
    assert.equal(result.code, 1);
    assert.match(result.stderr, /routes-only fragment/);
  },
);

test('a failed check preserves the last valid config and removes its temporary file', async () => {
  const initial = 'known-good: true\n';
  const { out, dir, result } = await runHook(
    {
      distDir: FIXTURE,
      check: true,
      // Node rejects Tanod's CLI arguments, after the temporary config has
      // been written. This exercises the validation-failure path without a
      // platform-specific shell script.
      tanodBin: process.execPath,
      upstreams: ['127.0.0.1:3000'],
      concurrency: 8,
    },
    { initial },
  );
  assert.equal(result.code, 1);
  assert.equal(await readFile(out, 'utf8'), initial);
  assert.deepEqual(
    (await readdir(dir)).filter((name) => name.endsWith('.tmp')),
    [],
  );
});

test(
  'a good configuration passes the check and the build succeeds',
  { skip: BINARY ? false : 'tanod binary not built' },
  async () => {
    const { result } = await runHook({
      distDir: FIXTURE,
      upstreams: ['127.0.0.1:3000'],
      concurrency: 8,
      check: true,
      tanodBin: BINARY,
    });
    assert.equal(result.code ?? 0, 0, result.stderr);
    assert.match(result.stdout, /tanod check passed/);
  },
);

test('a complete generated config requires an explicit measured ceiling', async () => {
  const { result } = await runHook({ distDir: FIXTURE, upstreams: ['next-1:3000'] });
  assert.equal(result.code, 1);
  assert.match(result.stderr, /requires `concurrency`/);
});

test('a group budget writes its conservative allocation into config and identity', async () => {
  const { out, dir, result } = await runHook({
    distDir: FIXTURE,
    upstreams: ['next-1:3000'],
    globalConcurrency: 9,
    replicas: 2,
  });
  assert.equal(result.code ?? 0, 0, result.stderr);
  const yaml = await readFile(out, 'utf8');
  const identity = JSON.parse(await readFile(path.join(dir, 'tanod.deployment.json'), 'utf8'));
  assert.match(yaml, /concurrency:\n    max: 4/);
  assert.match(yaml, /capacity:\n  global_max: 9\n  replicas: 2/);
  assert.deepEqual(identity.capacity, {
    global_max: 9,
    replicas: 2,
    allocated: 8,
    unallocated: 1,
  });
});
