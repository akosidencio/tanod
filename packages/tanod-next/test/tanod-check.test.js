import { test } from 'node:test';
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { mkdtemp, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { existsSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';

import { BINARY, FIXTURE, ROOT } from './fixture.js';

import { readBuild } from '../src/manifests.js';
import { generateConfig } from '../src/routes.js';

const run = promisify(execFile);

/**
 * The claim this whole package rests on: what it generates is a configuration
 * Tanod actually accepts.
 *
 * Skipped rather than failed when the binary is not built, because a
 * JavaScript package's test suite should not require a Rust toolchain to run —
 * but CI builds it, so the check is not optional there.
 */

test(
  'the generated configuration passes `tanod check`',
  { skip: BINARY ? false : 'tanod binary not built; run cargo build' },
  async () => {
    const build = await readBuild(FIXTURE);
    const yaml = generateConfig(build, { upstreams: ['127.0.0.1:3000', '127.0.0.2:3000'] });

    const dir = await mkdtemp(path.join(tmpdir(), 'tanod-next-'));
    const file = path.join(dir, 'generated.yaml');
    await writeFile(file, yaml);

    const { stdout } = await run(BINARY, ['check', '--config', file]);
    assert.match(stdout, /^ok:/m, stdout);
    // The generated deployment id has to survive into what Tanod read.
    assert.ok(yaml.includes(build.buildId));
  },
);

test(
  'a routes-only fragment is deliberately not a complete config',
  { skip: BINARY ? false : 'tanod binary not built' },
  async () => {
    const build = await readBuild(FIXTURE);
    const dir = await mkdtemp(path.join(tmpdir(), 'tanod-next-'));
    const file = path.join(dir, 'fragment.yaml');
    await writeFile(file, generateConfig(build, { includeDeployment: false }));

    // It has no `origin:`, so Tanod must refuse it rather than start with no
    // upstream. A fragment that validated would be a fragment somebody
    // deployed.
    await assert.rejects(() => run(BINARY, ['check', '--config', file]));
  },
);

test(
  'a small statically partitioned group passes `tanod check`',
  { skip: BINARY ? false : 'tanod binary not built; run cargo build' },
  async () => {
    const build = await readBuild(FIXTURE);
    const yaml = generateConfig(build, {
      upstreams: ['127.0.0.1:3000'],
      globalConcurrency: 9,
      replicas: 2,
    });
    const dir = await mkdtemp(path.join(tmpdir(), 'tanod-next-group-'));
    const file = path.join(dir, 'generated.yaml');
    await writeFile(file, yaml);
    const { stdout } = await run(BINARY, ['check', '--config', file]);
    assert.match(stdout, /replica-group capacity: 8\/9 allocated/);
  },
);
