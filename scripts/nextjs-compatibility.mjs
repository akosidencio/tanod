#!/usr/bin/env node
import assert from 'node:assert/strict';
import { readFile, writeFile } from 'node:fs/promises';
import { execFileSync } from 'node:child_process';
import {
  NEXT_COMPATIBILITY_MATRIX, SUPPORTED_NODE_RELEASES, TESTED_BUN_RELEASE,
  readBuild, readPolicy, generateConfig,
} from '../packages/tanod-next/src/index.js';

const [command, ...args] = process.argv.slice(2);
if (command === 'matrix') {
  console.log(JSON.stringify({ include: NEXT_COMPATIBILITY_MATRIX }));
} else if (command === 'node') {
  console.log(JSON.stringify(SUPPORTED_NODE_RELEASES));
} else if (command === 'bun') {
  console.log(TESTED_BUN_RELEASE);
} else if (command === 'assert') {
  assert.ok(NEXT_COMPATIBILITY_MATRIX.some((row) =>
    row.next === process.env.TANOD_NEXT_VERSION && row.node === process.env.TANOD_NEXT_NODE,
  ), 'TANOD_NEXT_VERSION / TANOD_NEXT_NODE must name a declared compatibility pair');
} else if (command === 'verify') {
  const [distDir, versionFile, output] = args;
  const actual = JSON.parse(await readFile(versionFile, 'utf8'));
  if (process.env.TANOD_NEXT_VERSION) assert.equal(actual.next, process.env.TANOD_NEXT_VERSION);
  if (process.env.TANOD_NEXT_NODE) assert.equal(actual.node.split('.')[0], process.env.TANOD_NEXT_NODE);
  const build = await readBuild(distDir);
  assert.ok(build.pagePaths.includes('/legacy/[slug]'), 'Pages Router output is missing');
  assert.ok(Object.values(build.appPaths).some((route) => route === '/products/[slug]'), 'App Router output is missing');
  const policy = await readPolicy('fixtures/next-storefront/tanod.next.yaml', build);
  const yaml = generateConfig(build, { upstreams: ['127.0.0.1:3000'], concurrency: 8, policy });
  await writeFile(output, yaml);
  execFileSync(process.env.TANOD_BIN || 'target/debug/tanod', ['check', '--config', output], { stdio: 'inherit' });
  console.log(`PASS: generated and validated policy from Next ${actual.next} on Node ${actual.node}`);
} else {
  throw new Error('usage: nextjs-compatibility.mjs matrix|node|bun|assert|verify <build> <versions.json> <config>');
}
