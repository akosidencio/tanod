import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { fileURLToPath } from 'node:url';
import { calibrate } from '../src/workflows.js';

const base = {
  target: 'http://127.0.0.1:3000',
  routes: ['/products/one', '/search?q=example'],
  allowLoad: true,
  steps: [1, 2, 4],
  requestsPerWorker: 20,
};
const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const successfulFetch = async () => {
  await pause(3);
  return new Response('rendered');
};

test('all failed requests produce no recommendation and stop after the baseline', async () => {
  let calls = 0;
  const report = await calibrate({ ...base, fetch: async () => {
    calls += 1;
    return new Response('', { status: 503 });
  } });
  assert.equal(report.ok, false);
  assert.equal(report.observedKnee, null);
  assert.equal(report.recommendedCeiling, null);
  assert.equal(report.highestHealthyConcurrency, null);
  assert.match(report.reason, /No healthy baseline/);
  assert.deepEqual(report.results[0].reasons, ['request_failures']);
  assert.equal(calls, 20);
  assert.equal(report.results.length, 1);
});

test('connection failures cannot establish a capacity recommendation', async () => {
  const report = await calibrate({ ...base, fetch: async () => { throw new Error('connection refused'); } });
  assert.equal(report.ok, false);
  assert.equal(report.recommendedCeiling, null);
  assert.equal(report.results[0].failures, 20);
});

test('the first failing step bounds the recommendation and stops increasing load', async () => {
  const report = await calibrate({ ...base, fetch: async (url) => {
    await pause(3);
    const atBaseline = url.searchParams.get('__tanod_calibration').includes('-measure-1-');
    return new Response('', { status: atBaseline ? 200 : 503 });
  } });
  assert.equal(report.ok, true);
  assert.equal(report.kneeObserved, true);
  assert.equal(report.observedKnee, 1);
  assert.equal(report.recommendedCeiling, 1);
  assert.deepEqual(report.results.map((row) => row.concurrency), [1, 2]);
  assert.equal(report.applied, false);
});

test('healthy samples apply the margin without claiming unobserved saturation', async () => {
  const report = await calibrate({ ...base, fetch: successfulFetch });
  assert.equal(report.ok, true);
  assert.equal(report.highestHealthyConcurrency, 4);
  assert.equal(report.recommendedCeiling, 2);
  assert.equal(report.kneeObserved, false);
  assert.equal(report.observedKnee, null);
  assert.match(report.warnings[0], /Saturation was not established/);
  assert.deepEqual(report.results.map((row) => row.requests), [20, 40, 80]);
});

test('an explicit latency target can reject even a successful baseline', async () => {
  const report = await calibrate({ ...base, maxP95Ms: 0.001, fetch: successfulFetch });
  assert.equal(report.ok, false);
  assert.equal(report.recommendedCeiling, null);
  assert.deepEqual(report.results[0].reasons, ['latency_limit_exceeded']);
  assert.equal(report.latencyLimitMs, 0.001);
});

test('rising full-response latency stops the load ramp at the prior healthy step', async () => {
  const report = await calibrate({ ...base, fetch: async (url) => {
    await pause(url.searchParams.get('__tanod_calibration').includes('-measure-1-') ? 3 : 30);
    return new Response('rendered');
  } });
  assert.equal(report.ok, true);
  assert.equal(report.highestHealthyConcurrency, 1);
  assert.deepEqual(report.results[1].reasons, ['latency_limit_exceeded']);
  assert.equal(report.results.length, 2);
});

test('too few samples return an inconclusive result', async () => {
  const report = await calibrate({ ...base, requestsPerWorker: 4, fetch: successfulFetch });
  assert.equal(report.ok, false);
  assert.equal(report.baselineP95Ms, null);
  assert.equal(report.recommendedCeiling, null);
  assert.deepEqual(report.results[0].reasons, ['insufficient_samples']);
});

test('warm-up is separate from the sustained measurement and workload routes are preserved', async () => {
  const calls = { warmup: 0, measure: 0 };
  const routes = new Set();
  const report = await calibrate({
    ...base, requestsPerWorker: undefined, steps: [1], durationMs: 120, warmupMs: 20,
    fetch: async (url, options) => {
      const phase = url.searchParams.get('__tanod_calibration').includes('-warmup-') ? 'warmup' : 'measure';
      calls[phase] += 1;
      routes.add(url.pathname);
      if (url.pathname === '/search') assert.equal(url.searchParams.get('q'), 'example');
      assert.equal(options.redirect, 'manual');
      assert.equal(options.headers['cache-control'], 'no-cache');
      await pause(3);
      return new Response('rendered');
    },
  });
  assert.ok(calls.warmup > 0);
  assert.equal(report.results[0].requests, calls.measure);
  assert.equal(report.results[0].warmup_requests, calls.warmup);
  assert.ok(report.results[0].duration_ms >= 120);
  assert.deepEqual([...routes].sort(), ['/products/one', '/search']);
});

test('warm-up failures remain visible and prevent a healthy classification', async () => {
  const report = await calibrate({ ...base, steps: [1], warmupMs: 10, fetch: async (url) => {
    await pause(3);
    return new Response('', { status: url.searchParams.get('__tanod_calibration').includes('-warmup-') ? 503 : 200 });
  } });
  assert.equal(report.ok, false);
  assert.ok(report.results[0].warmup_failures > 0);
  assert.equal(report.results[0].failures, 0);
  assert.deepEqual(report.results[0].reasons, ['warmup_failures']);
});

test('latency includes consuming the streamed body', async () => {
  const report = await calibrate({ ...base, steps: [1], requestsPerWorker: 1, fetch: async () =>
    new Response(new ReadableStream({ async start(controller) {
      controller.enqueue(new Uint8Array([1]));
      await pause(15);
      controller.close();
    } })),
  });
  assert.ok(report.results[0].latency_p95_ms >= 10);
});

test('invalid measurement settings are rejected before any load is sent', async () => {
  let calls = 0;
  const fetch = async () => { calls += 1; return new Response(''); };
  for (const options of [
    { steps: [2, 4] }, { steps: [1, 1] }, { steps: [1, 4, 2] }, { steps: [1, 1025] },
    { requestsPerWorker: 0 }, { requestsPerWorker: -1 }, { requestsPerWorker: 1.5 },
    { durationMs: 10 }, { warmupMs: -1 }, { timeoutMs: 0 }, { minSamples: 19 },
    { maxP95Ms: NaN }, { maxP95Ms: 0 },
    { routes: ['https://other.example/page'] }, { routes: ['//127.0.0.1:3000/page'] },
    { routes: ['/\\other.example'] }, { routes: ['/page#fragment'] },
    { target: 'file:///tmp/page' }, { target: 'http://secret@127.0.0.1:3000' },
  ]) {
    await assert.rejects(() => calibrate({ ...base, ...options, fetch }), { name: 'TanodNextError' });
  }
  assert.equal(calls, 0);
});

test('IPv6 loopback is a local calibration target', async () => {
  const report = await calibrate({ ...base, target: 'http://[::1]:3000', steps: [1], fetch: successfulFetch });
  assert.equal(report.ok, true);
});

test('the CLI emits an inconclusive JSON report and returns nonzero', async () => {
  const server = createServer((request, response) => {
    response.writeHead(503);
    response.end('busy');
  });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  try {
    const { port } = server.address();
    const result = await promisify(execFile)(process.execPath, [
      fileURLToPath(new URL('../src/cli.js', import.meta.url)),
      'calibrate', '--target', `http://127.0.0.1:${port}`, '--route', '/', '--allow-load',
      '--steps', '1,2', '--requests-per-worker', '20', '--warmup', '0', '--min-samples', '20',
      '--max-p95-ms', '500', '--timeout-ms', '1000',
    ]).catch((error) => error);
    assert.equal(result.code, 1);
    const report = JSON.parse(result.stdout);
    assert.equal(report.ok, false);
    assert.equal(report.recommendedCeiling, null);
    assert.equal(report.maxP95Ms, 500);
    assert.equal(report.timeoutMs, 1000);
  } finally {
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
  }
});

test('the installed CLI calibrates a real local origin and returns usable JSON', async () => {
  const paths = new Set();
  const server = createServer((request, response) => {
    const url = new URL(request.url, 'http://localhost');
    paths.add(url.pathname);
    assert.ok(url.searchParams.has('__tanod_calibration'));
    response.end('rendered');
  });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  try {
    const { port } = server.address();
    const { stdout, stderr } = await promisify(execFile)(process.execPath, [
      fileURLToPath(new URL('../src/cli.js', import.meta.url)), 'calibrate',
      '--target', `http://127.0.0.1:${port}`, '--route', '/products/one', '--route', '/search?q=example',
      '--allow-load', '--steps', '1', '--duration', '0.2', '--warmup', '0.02', '--max-p95-ms', '1000',
    ]);
    const report = JSON.parse(stdout);
    assert.equal(report.ok, true);
    assert.equal(report.recommendedCeiling, 1);
    assert.equal(report.durationMs, 200);
    assert.equal(report.warmupMs, 20);
    assert.equal(report.maxP95Ms, 1000);
    assert.match(stderr, /measurement=200ms\/step warmup=20ms\/step/);
    assert.deepEqual([...paths].sort(), ['/products/one', '/search']);
  } finally {
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
  }
});
