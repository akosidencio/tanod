import { execFile } from 'node:child_process';
import { performance } from 'node:perf_hooks';
import { promisify } from 'node:util';

import { tanodBin } from './generate-to-file.js';
import { TanodNextError } from './manifests.js';
import { inspectRoutes } from './routes.js';

const run = promisify(execFile);
const REQUIRED_METRICS = [
  'tanod_admission_total',
  'tanod_queue_depth',
  'tanod_concurrency_limit',
  'tanod_origin_requests_total',
  'tanod_cache_total',
  'tanod_cache_bypass_reason_total',
  'tanod_config_generation',
  'tanod_upstream_healthy',
];
const SENSITIVE = /^(?:authorization|cookie|proxy-authorization|x-api-key)$/i;

export function formatInspection(build, policy, { json = false } = {}) {
  const rows = inspectRoutes(build, policy);
  if (json) return `${JSON.stringify({ buildId: build.buildId, routes: rows }, null, 2)}\n`;
  const widths = [
    Math.max(5, ...rows.map((row) => row.route.length)),
    Math.max(6, ...rows.map((row) => row.render.length)),
    7,
    Math.max(5, ...rows.map((row) => row.reuse.length)),
  ];
  const line = (values) => values.map((value, index) => String(value).padEnd(widths[index] ?? 0)).join('  ').trimEnd();
  return [
    `Build ${build.identity ?? build.buildId}`,
    line(['ROUTE', 'RENDER', 'PRIVACY', 'REUSE', 'WEIGHT', 'PRIORITY', 'EVIDENCE']),
    ...rows.map((row) => line([
      row.route,
      row.render,
      row.privacy,
      row.reuse,
      row.weight,
      row.priority,
      row.publicOverride ? `${row.evidence} (PUBLIC OVERRIDE)` : row.evidence,
    ])),
    '',
  ].join('\n');
}

function routeRegex(route) {
  if (route === '/') return /^\/$/;
  const tokens = route.split('/').slice(1);
  let expression = '^';
  for (const token of tokens) {
    if (/^\[\[\.\.\..+\]\]$/.test(token)) expression += '(?:/.*)?';
    else if (/^\[\.\.\..+\]$/.test(token)) expression += '/.+';
    else if (/^\[.+\]$/.test(token)) expression += '/[^/]+';
    else expression += `/${token.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}`;
  }
  return new RegExp(`${expression}/?$`);
}

function normalizeHeaders(headers = []) {
  const output = {};
  for (const item of headers) {
    const index = item.indexOf(':');
    if (index < 1) throw new TanodNextError(`header ${JSON.stringify(item)} must use \`Name: value\``);
    const name = item.slice(0, index).trim().toLowerCase();
    const value = item.slice(index + 1).trim();
    output[name] = value;
  }
  return output;
}

/** Explain a local synthetic request without contacting Tanod or an origin. */
export function explainRequest(build, policy, options = {}) {
  const method = String(options.method ?? 'GET').toUpperCase();
  const url = new URL(options.url ?? options.path ?? '/', 'http://tanod.local');
  const headers = normalizeHeaders(options.headers);
  const rows = inspectRoutes(build, policy);
  const row = rows.find((candidate) => routeRegex(candidate.route).test(url.pathname)) ?? {
    route: '<default>',
    match: '/**',
    render: 'unknown',
    privacy: 'private',
    reuse: 'bypass',
    weight: 1,
    priority: 'normal',
    evidence: 'safe catch-all',
    assertion: null,
  };
  const barriers = [];
  if (!['GET', 'HEAD'].includes(method)) barriers.push(`unsafe method ${method}`);
  if (headers.authorization !== undefined) barriers.push('Authorization header');
  if (headers.cookie !== undefined) barriers.push('Cookie header');
  if (headers['next-action'] !== undefined) barriers.push('Server Action');
  if (headers['next-router-prefetch'] !== undefined) barriers.push('Next.js prefetch variant');
  if (row.privacy === 'private') barriers.push('private route policy');
  const queryKeys = row.assertion?.cache?.query;
  const query = [...url.searchParams.entries()]
    .filter(([key]) => !queryKeys || queryKeys.includes(key))
    .sort(([a], [b]) => a.localeCompare(b));
  const vary = row.assertion?.cache?.vary ?? [];
  return {
    request: {
      method,
      path: url.pathname,
      headers: Object.fromEntries(Object.entries(headers).map(([name, value]) => [name, SENSITIVE.test(name) ? '<redacted>' : value])),
    },
    route: row.route,
    match: row.match,
    render: row.render,
    classification: row.privacy === 'private' || barriers.length > 0 ? 'private_dynamic' : row.render === 'prerendered' ? 'public_ssr' : 'public_ssr',
    reuse: barriers.length > 0 ? 'bypass' : row.reuse,
    evidence: row.evidence,
    barriers,
    priority: row.priority,
    weight: row.weight,
    limiter: `tier:${row.priority}`,
    cacheKeyInputs: {
      scheme: url.protocol.slice(0, -1),
      host: url.host,
      method,
      path: url.pathname,
      query,
      vary: Object.fromEntries(vary.map((name) => [name, headers[name.toLowerCase()] === undefined ? '<absent>' : '<present>'])),
      deployment: build.identity ?? build.buildId,
    },
  };
}

async function boundedFetch(url, options = {}, timeoutMs = 2000) {
  return fetch(url, { ...options, redirect: 'manual', signal: AbortSignal.timeout(timeoutMs) });
}

function checkResult(name, status, detail) {
  return { name, status, detail };
}

/** Run bounded, read-only checks plus an optional scoped no-match purge. */
export async function doctor(build, options = {}) {
  const checks = [];
  if (options.config) {
    try {
      await run(tanodBin(options.tanodBin), ['check', '--config', options.config], { timeout: options.timeoutMs ?? 5000 });
      checks.push(checkResult('configuration', 'pass', 'tanod check accepted the generated config'));
    } catch (cause) {
      checks.push(checkResult('configuration', 'fail', `${cause?.stderr || cause?.message || cause}`.trim()));
    }
  } else {
    checks.push(checkResult('configuration', 'skip', 'pass --config to validate the generated config'));
  }

  const origins = options.origins ?? [];
  if (origins.length > 0) {
    const identities = await Promise.all(origins.map(async (origin) => {
      const url = new URL('/.well-known/tanod/deployment', origin);
      const response = await boundedFetch(url, {}, options.timeoutMs);
      if (!response.ok) throw new Error(`${url.origin} answered ${response.status}`);
      return { origin: url.origin, body: await response.json() };
    })).catch((cause) => cause);
    if (identities instanceof Error) {
      checks.push(checkResult('origin identity', 'fail', identities.message));
    } else {
      const expectedIdentity = build.identity ?? build.buildId;
      const wrong = identities.filter(({ body }) => body.build_id !== expectedIdentity);
      checks.push(checkResult(
        'origin identity',
        wrong.length ? 'fail' : 'pass',
        wrong.length ? `${wrong.map(({ origin, body }) => `${origin}=${body.build_id ?? 'missing'}`).join(', ')}; expected ${expectedIdentity}` : `${identities.length} origin(s) agree on ${expectedIdentity}`,
      ));
    }
  } else {
    checks.push(checkResult('origin identity', 'skip', 'pass at least one --origin'));
  }

  if (origins.length > 0 && options.doctorToken) {
    const headers = { 'x-tanod-doctor-token': options.doctorToken };
    const readProbe = async (origin) => {
      const response = await boundedFetch(new URL('/.well-known/tanod/cache', origin), { headers }, options.timeoutMs);
      if (!response.ok) throw new Error(`${new URL(origin).origin} cache probe answered ${response.status}`);
      return response.json();
    };
    const waitForConvergence = async (previousGeneration) => {
      const deadline = performance.now() + (options.timeoutMs ?? 2000);
      let generation;
      let converged = false;
      let changed = previousGeneration === undefined;
      do {
        const first = await readProbe(origins[0]);
        const values = await Promise.all(origins.map(readProbe));
        generation = first.generation;
        converged = values.every((value) => value.generation === generation);
        changed = previousGeneration === undefined || generation !== previousGeneration;
        if (changed && converged) break;
        await new Promise((resolve) => setTimeout(resolve, 25));
      } while (performance.now() < deadline);
      return { generation, converged, changed };
    };
    try {
      const initial = await waitForConvergence();
      const invalidate = await boundedFetch(new URL('/.well-known/tanod/cache', origins[0]), { method: 'POST', headers }, options.timeoutMs);
      if (!invalidate.ok) throw new Error(`cache invalidation probe answered ${invalidate.status}`);
      const final = await waitForConvergence(initial.generation);
      checks.push(checkResult(
        'Next.js cache coordination',
        initial.converged && final.changed && final.converged ? 'pass' : 'fail',
        `initial=${initial.converged}, invalidated=${final.changed}, converged=${final.converged}`,
      ));
    } catch (cause) {
      checks.push(checkResult('Next.js cache coordination', 'fail', cause.message));
    }
  } else {
    checks.push(checkResult('Next.js cache coordination', 'skip', 'pass --origin and --doctor-token to run the dedicated cache probe'));
  }

  const listenerValues = [options.traffic, options.metrics, options.admin].filter(Boolean).map((value) => new URL(value).origin);
  checks.push(checkResult(
    'listener separation',
    listenerValues.length < 2 ? 'skip' : new Set(listenerValues).size === listenerValues.length ? 'pass' : 'fail',
    listenerValues.length < 2 ? 'pass traffic, metrics, and admin URLs to compare them' : listenerValues.join(', '),
  ));

  if (options.traffic) {
    try {
      const response = await boundedFetch(new URL('/healthz', options.traffic), {}, options.timeoutMs);
      checks.push(checkResult('traffic path', response.ok ? 'pass' : 'fail', `GET /healthz answered ${response.status}`));
    } catch (cause) {
      checks.push(checkResult('traffic path', 'fail', cause.message));
    }
  }

  if (options.traffic && options.publicUrl && options.doctorToken) {
    try {
      const expected = new URL(options.publicUrl);
      const response = await boundedFetch(new URL('/.well-known/tanod/request', options.traffic), {
        headers: { 'x-tanod-doctor-token': options.doctorToken, host: expected.host },
      }, options.timeoutMs);
      const observed = await response.json();
      const pass = response.ok && observed.host === expected.host && observed.scheme === expected.protocol.slice(0, -1);
      checks.push(checkResult('forwarded public URL', pass ? 'pass' : 'fail', `origin saw ${observed.scheme}://${observed.host}; expected ${expected.origin}`));
    } catch (cause) {
      checks.push(checkResult('forwarded public URL', 'fail', cause.message));
    }
  } else {
    checks.push(checkResult('forwarded public URL', 'skip', 'pass --traffic, --public-url, and --doctor-token'));
  }

  if (options.traffic && options.streamPath) {
    try {
      const started = performance.now();
      const response = await boundedFetch(new URL(options.streamPath, options.traffic), {}, options.timeoutMs ?? 15000);
      const reader = response.body?.getReader();
      if (!reader) throw new Error('stream response had no readable body');
      const first = await reader.read();
      const firstMs = performance.now() - started;
      while (!(await reader.read()).done) {}
      const totalMs = performance.now() - started;
      const pass = response.ok && !first.done && totalMs - firstMs >= 100;
      checks.push(checkResult('progressive streaming', pass ? 'pass' : 'fail', `first byte ${firstMs.toFixed(0)}ms, complete ${totalMs.toFixed(0)}ms`));
    } catch (cause) {
      checks.push(checkResult('progressive streaming', 'fail', cause.message));
    }
  } else {
    checks.push(checkResult('progressive streaming', 'skip', 'pass --traffic and --stream-path'));
  }

  if (options.metrics) {
    try {
      const response = await boundedFetch(new URL('/metrics', options.metrics), {}, options.timeoutMs);
      const body = await response.text();
      const missing = REQUIRED_METRICS.filter((name) => !body.includes(name));
      checks.push(checkResult('metrics contract', response.ok && missing.length === 0 ? 'pass' : 'fail', missing.length ? `missing: ${missing.join(', ')}` : `${REQUIRED_METRICS.length} required metrics found`));
    } catch (cause) {
      checks.push(checkResult('metrics contract', 'fail', cause.message));
    }
  }

  if (options.admin && options.purgeToken) {
    try {
      const url = new URL(`/purge?tag=tanod-doctor-${encodeURIComponent(build.identity ?? build.buildId)}`, options.admin);
      const unauthenticated = await boundedFetch(url, { method: 'POST' }, options.timeoutMs);
      const authenticated = await boundedFetch(url, {
        method: 'POST',
        headers: { authorization: `Bearer ${options.purgeToken}`, accept: 'application/json' },
      }, options.timeoutMs);
      const body = await authenticated.json();
      const pass = unauthenticated.status === 401 && authenticated.ok && body?.purged === true;
      checks.push(checkResult('scoped purge', pass ? 'pass' : 'fail', `unauthenticated=${unauthenticated.status}, authenticated=${authenticated.status}, entries=${body?.entries ?? '?'}`));
    } catch (cause) {
      checks.push(checkResult('scoped purge', 'fail', cause.message));
    }
  } else {
    checks.push(checkResult('scoped purge', 'skip', 'pass --admin and --purge-token to verify authentication with an unused test tag'));
  }

  const failed = checks.filter((check) => check.status === 'fail').length;
  const passed = checks.filter((check) => check.status === 'pass').length;
  return {
    ok: failed === 0 && passed > 0,
    buildId: build.identity ?? build.buildId,
    checks,
  };
}

function percentile(values, fraction) {
  if (values.length === 0) return null;
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * fraction))];
}

function isLocalTarget(url) {
  return ['127.0.0.1', 'localhost', '::1'].includes(url.hostname);
}

/** Run an explicitly authorized, bounded calibration without changing config. */
export async function calibrate(options = {}) {
  if (!options.allowLoad) throw new TanodNextError('calibrate requires --allow-load');
  if (!options.target) throw new TanodNextError('calibrate requires --target');
  if (!options.routes?.length) throw new TanodNextError('calibrate requires at least one --route allowlist entry');
  const target = new URL(options.target);
  if (!isLocalTarget(target) && !options.allowProduction) {
    throw new TanodNextError('refusing a non-local target without --allow-production');
  }
  const steps = options.steps ?? [1, 2, 4, 8, 16];
  if (!steps.length || steps.some((step) => !Number.isInteger(step) || step < 1 || step > 1024)) {
    throw new TanodNextError('calibration steps must be integers from 1 to 1024');
  }
  const requestsPerWorker = options.requestsPerWorker ?? 4;
  const timeoutMs = options.timeoutMs ?? 10000;
  const results = [];
  for (const concurrency of steps) {
    const latencies = [];
    let successes = 0;
    let failures = 0;
    const started = performance.now();
    await Promise.all(Array.from({ length: concurrency }, async (_, worker) => {
      for (let iteration = 0; iteration < requestsPerWorker; iteration += 1) {
        const route = options.routes[(worker + iteration) % options.routes.length];
        const url = new URL(route, target);
        url.searchParams.set('__tanod_calibration', `${concurrency}-${worker}-${iteration}`);
        const requestStarted = performance.now();
        try {
          const response = await boundedFetch(url, { headers: { 'cache-control': 'no-cache' } }, timeoutMs);
          await response.arrayBuffer();
          if (response.ok) successes += 1;
          else failures += 1;
        } catch {
          failures += 1;
        }
        latencies.push(performance.now() - requestStarted);
      }
    }));
    const elapsed = performance.now() - started;
    results.push({
      concurrency,
      requests: successes + failures,
      successes,
      failures,
      throughput: Number(((successes * 1000) / elapsed).toFixed(2)),
      latency_p50_ms: Number(percentile(latencies, 0.5)?.toFixed(2)),
      latency_p95_ms: Number(percentile(latencies, 0.95)?.toFixed(2)),
    });
  }
  const baseline = results[0].latency_p95_ms || 1;
  const healthy = results.filter((result) => result.failures === 0 && result.latency_p95_ms <= baseline * 2);
  const knee = (healthy.at(-1) ?? results[0]).concurrency;
  return {
    target: target.origin,
    routes: options.routes,
    safetyMargin: 0.7,
    observedKnee: knee,
    recommendedCeiling: Math.max(1, Math.floor(knee * 0.7)),
    applied: false,
    results,
  };
}
