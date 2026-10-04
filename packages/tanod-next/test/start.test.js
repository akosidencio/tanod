import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { chmodSync, mkdtempSync, readFileSync, writeFileSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import net from 'node:net';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { parseStartArgs, resolveTanodBinary } from '../src/start.js';

const CLI = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', 'src', 'cli.js');

function freePort() {
  return new Promise((resolve) => {
    const server = net.createServer();
    server.listen(0, '127.0.0.1', () => {
      const { port } = server.address();
      server.close(() => resolve(port));
    });
  });
}

/** A scratch directory holding a fake tanod and a fake origin that log to one file. */
function fixture({ tanodExit = null, originExit = null } = {}) {
  const dir = mkdtempSync(path.join(tmpdir(), 'tanod-start-'));
  const events = path.join(dir, 'events.log');
  writeFileSync(events, '');
  const tanod = path.join(dir, 'tanod');
  writeFileSync(
    tanod,
    `#!${process.execPath}
const fs = require('node:fs');
fs.appendFileSync(${JSON.stringify(events)}, 'tanod-start ' + process.argv.slice(2).join(' ') + '\\n');
process.on('SIGHUP', () => fs.appendFileSync(${JSON.stringify(events)}, 'tanod-hup\\n'));
process.on('SIGTERM', () => {
  fs.appendFileSync(${JSON.stringify(events)}, 'tanod-stop\\n');
  process.exit(0);
});
${tanodExit === null ? 'setInterval(() => {}, 1000);' : `setTimeout(() => process.exit(${tanodExit}), 100);`}
`,
  );
  chmodSync(tanod, 0o755);
  const origin = path.join(dir, 'origin.cjs');
  writeFileSync(
    origin,
    `const fs = require('node:fs');
const http = require('node:http');
fs.appendFileSync(${JSON.stringify(events)}, 'origin-start ' + process.env.HOSTNAME + ':' + process.env.PORT + '\\n');
${originExit === null ? '' : `process.exit(${originExit});`}
const server = http.createServer((q, r) => r.end('ok')).listen(Number(process.env.PORT), process.env.HOSTNAME);
process.on('SIGTERM', () => {
  fs.appendFileSync(${JSON.stringify(events)}, 'origin-stop\\n');
  server.close(() => process.exit(0));
});
`,
  );
  return { dir, events, tanod, origin, log: () => readFileSync(events, 'utf8').trim().split('\n').filter(Boolean) };
}

function run(args, env = {}) {
  const child = spawn(process.execPath, [CLI, 'start', ...args], {
    env: { ...process.env, TANOD_BIN: '', ...env },
    stdio: ['ignore', 'ignore', 'pipe'],
  });
  let stderr = '';
  child.stderr.on('data', (d) => {
    stderr += d;
  });
  const exited = new Promise((resolve) => child.on('exit', (code) => resolve({ code, stderr })));
  return { child, exited };
}

async function until(predicate, ms = 10_000) {
  const deadline = Date.now() + ms;
  while (!predicate()) {
    if (Date.now() > deadline) throw new Error('timed out waiting');
    await new Promise((r) => setTimeout(r, 50));
  }
}

test('the origin starts first, tanod once it is listening, and SIGTERM stops tanod before the origin', async () => {
  const f = fixture();
  const port = await freePort();
  const { child, exited } = run(['--tanod-bin', f.tanod, '--config', 'x.yaml', '--origin-port', String(port), '--', process.execPath, f.origin]);
  await until(() => f.log().some((l) => l.startsWith('tanod-start')));
  child.kill('SIGHUP');
  await until(() => f.log().includes('tanod-hup'));
  child.kill('SIGTERM');
  const { code, stderr } = await exited;
  assert.equal(code, 0, stderr);
  assert.deepEqual(f.log(), [
    `origin-start 127.0.0.1:${port}`,
    'tanod-start run --config x.yaml',
    'tanod-hup',
    'tanod-stop',
    'origin-stop',
  ]);
});

test('tanod dying stops the origin and passes on its exit code', async () => {
  const f = fixture({ tanodExit: 3 });
  const port = await freePort();
  const { exited } = run(['--tanod-bin', f.tanod, '--origin-port', String(port), '--', process.execPath, f.origin]);
  const { code } = await exited;
  assert.equal(code, 3);
  assert.equal(f.log().at(-1), 'origin-stop');
});

test('an origin that exits before listening never gets a tanod in front of it', async () => {
  const f = fixture({ originExit: 5 });
  const port = await freePort();
  const { exited } = run(['--tanod-bin', f.tanod, '--origin-port', String(port), '--', process.execPath, f.origin]);
  const { code } = await exited;
  assert.equal(code, 5);
  assert.ok(!f.log().some((l) => l.startsWith('tanod-start')));
});

test('a missing tanod binary is reported and the origin is stopped', async () => {
  const f = fixture();
  const port = await freePort();
  const { exited } = run(['--tanod-bin', 'tanod-does-not-exist-anywhere', '--origin-port', String(port), '--', process.execPath, f.origin]);
  const { code, stderr } = await exited;
  assert.equal(code, 127);
  assert.match(stderr, /not found/);
  assert.equal(f.log().at(-1), 'origin-stop');
});

test('the binary is found explicitly, then via TANOD_BIN, then the platform package, then PATH', () => {
  const resolveNothing = () => {
    throw new Error('not installed');
  };
  assert.equal(resolveTanodBinary('/x/tanod', { TANOD_BIN: '/y' }, resolveNothing), '/x/tanod');
  assert.equal(resolveTanodBinary(null, { TANOD_BIN: '/y' }, resolveNothing), '/y');
  assert.equal(resolveTanodBinary(null, {}, resolveNothing), 'tanod');
  if (process.platform === 'linux' && process.arch === 'x64') {
    assert.equal(resolveTanodBinary(null, {}, (id) => `/pkg/${id}`), '/pkg/@tanod/linux-x64/bin/tanod');
  }
});

test('start options are checked', () => {
  assert.equal(parseStartArgs([], {}).originPort, 3000);
  assert.equal(parseStartArgs([], { TANOD_ORIGIN_PORT: '4000' }).originPort, 4000);
  assert.deepEqual(parseStartArgs(['--', 'node', 'server.js'], {}).origin, ['node', 'server.js']);
  assert.throws(() => parseStartArgs(['--origin-port', '0'], {}), /TCP port/);
  assert.throws(() => parseStartArgs(['--'], {}), /nothing after/);
  assert.throws(() => parseStartArgs(['--bogus'], {}), /unknown option/);
  assert.ok(!existsSync('/definitely/not/here'));
});
