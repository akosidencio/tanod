import { performance } from 'node:perf_hooks';
import { TanodNextError } from './manifests.js';

function integer(value, name, min, max) {
  if (!Number.isSafeInteger(value) || value < min || value > max) {
    throw new TanodNextError(`${name} must be an integer from ${min} to ${max}`);
  }
  return value;
}

function percentile(values, fraction) {
  if (!values.length) return null;
  const sorted = [...values].sort((a, b) => a - b);
  return Number(sorted[Math.ceil(sorted.length * fraction) - 1].toFixed(2));
}

/**
 * Measure a declared workload without modifying configuration. A failed or
 * undersampled baseline cannot establish a capacity recommendation. Stop at
 * the first unhealthy step rather than increasing load on an overloaded app.
 * @param {import('./index.js').CalibrationOptions} options
 * @returns {Promise<import('./index.js').CalibrationReport>}
 */
export async function calibrate(options = {}) {
  if (!options.allowLoad) throw new TanodNextError('calibrate requires --allow-load');
  if (!options.target) throw new TanodNextError('calibrate requires --target');
  if (!Array.isArray(options.routes) || !options.routes.length) {
    throw new TanodNextError('calibrate requires at least one --route allowlist entry');
  }
  const target = new URL(options.target);
  if (!['http:', 'https:'].includes(target.protocol) || target.username || target.password) {
    throw new TanodNextError('calibration target must be an HTTP(S) URL without credentials');
  }
  if (!['127.0.0.1', 'localhost', '[::1]'].includes(target.hostname) && !options.allowProduction) {
    throw new TanodNextError('refusing a non-local target without --allow-production');
  }
  const routes = options.routes.map((route) => {
    if (typeof route !== 'string' || !route.startsWith('/') || route.startsWith('//') || route.includes('\\') ||
        new URL(route, target).origin !== target.origin || new URL(route, target).hash) {
      throw new TanodNextError('calibration routes must be absolute paths on the target without fragments');
    }
    return route;
  });
  const steps = options.steps ?? [1, 2, 4, 8, 16];
  if (!Array.isArray(steps) || !steps.length || steps.length > 16 || steps[0] !== 1 ||
      steps.some((step, index) => !Number.isInteger(step) || step < 1 || step > 1024 ||
        (index > 0 && step <= steps[index - 1]))) {
    throw new TanodNextError('calibration steps must start at 1 and strictly increase, with up to 16 integers from 1 to 1024');
  }
  if (options.durationMs !== undefined && options.requestsPerWorker !== undefined) {
    throw new TanodNextError('use either durationMs or requestsPerWorker for calibration');
  }
  const requestsPerWorker = options.requestsPerWorker === undefined ? null :
    integer(options.requestsPerWorker, 'requestsPerWorker', 1, 1000);
  const durationMs = requestsPerWorker === null ?
    integer(options.durationMs ?? 10000, 'durationMs', 1, 60000) : null;
  const warmupMs = integer(options.warmupMs ?? (durationMs === null ? 0 : 2000), 'warmupMs', 0, 30000);
  const timeoutMs = integer(options.timeoutMs ?? 10000, 'timeoutMs', 1, 60000);
  const minSamples = integer(options.minSamples ?? 20, 'minSamples', 20, 100000);
  const maxP95Ms = options.maxP95Ms ?? null;
  if (maxP95Ms !== null && (!Number.isFinite(maxP95Ms) || maxP95Ms <= 0)) {
    throw new TanodNextError('maxP95Ms must be a positive finite number');
  }
  const fetchImpl = options.fetch ?? globalThis.fetch;
  if (typeof fetchImpl !== 'function') throw new TanodNextError('calibration needs a fetch implementation');
  const runId = `${Date.now()}-${Math.random().toString(36).slice(2)}`;
  /** @type {import('./index.js').CalibrationStep[]} */
  const results = [];
  let baselineP95Ms = null;
  let latencyLimitMs = maxP95Ms;
  let highestHealthyConcurrency = null;
  let kneeObserved = false;

  async function measure(concurrency, phase, windowMs) {
    const started = performance.now();
    const deadline = windowMs === null ? null : started + windowMs;
    const latencies = [];
    let successes = 0;
    let failures = 0;
    let issued = 0;
    // Bound retained samples and total requests even for a very fast origin.
    const requestLimit = 100000;
    await Promise.all(Array.from({ length: concurrency }, async (_, worker) => {
      let iteration = 0;
      while (issued < requestLimit && (deadline === null ? iteration < requestsPerWorker : performance.now() < deadline)) {
        const sample = issued++;
        const url = new URL(routes[sample % routes.length], target);
        url.searchParams.set('__tanod_calibration', `${runId}-${phase}-${concurrency}-${worker}-${iteration++}`);
        const requestStarted = performance.now();
        try {
          const response = await fetchImpl(url, {
            headers: { 'cache-control': 'no-cache' },
            redirect: 'manual',
            signal: AbortSignal.timeout(timeoutMs),
          });
          // Read the full response without retaining its body in the load
          // generator. Header latency alone misses streaming render work.
          if (response.body) {
            const reader = response.body.getReader();
            try {
              while (!(await reader.read()).done) { /* discard each chunk */ }
            } finally {
              reader.releaseLock();
            }
          }
          if (response.ok) successes += 1;
          else failures += 1;
        } catch {
          failures += 1;
        }
        latencies.push(performance.now() - requestStarted);
      }
    }));
    return {
      requests: successes + failures,
      successes,
      failures,
      duration_ms: Number((performance.now() - started).toFixed(2)),
      sample_limit_reached: issued === requestLimit,
      latency_p50_ms: percentile(latencies, 0.5),
      latency_p95_ms: percentile(latencies, 0.95),
    };
  }

  for (const concurrency of steps) {
    const warmup = warmupMs > 0 ? await measure(concurrency, 'warmup', warmupMs) : null;
    const measured = await measure(concurrency, 'measure', durationMs);
    if (results.length === 0 && measured.failures === 0 && measured.requests >= minSamples) {
      baselineP95Ms = measured.latency_p95_ms;
      latencyLimitMs = Math.min(maxP95Ms ?? Infinity, Math.max(baselineP95Ms, 1) * 2);
    }
    const reasons = [];
    if (warmup?.failures) reasons.push('warmup_failures');
    if (measured.failures) reasons.push('request_failures');
    if (measured.requests < minSamples) reasons.push('insufficient_samples');
    if (latencyLimitMs !== null && measured.latency_p95_ms > latencyLimitMs) {
      reasons.push('latency_limit_exceeded');
    }
    const healthy = reasons.length === 0;
    results.push({
      concurrency,
      ...measured,
      throughput: Number((measured.successes * 1000 / Math.max(measured.duration_ms, 0.01)).toFixed(2)),
      warmup_requests: warmup?.requests ?? 0,
      warmup_failures: warmup?.failures ?? 0,
      healthy,
      reasons,
    });
    if (!healthy) {
      kneeObserved = highestHealthyConcurrency !== null && reasons.some((reason) => reason !== 'insufficient_samples');
      break;
    }
    highestHealthyConcurrency = concurrency;
  }
  const ok = highestHealthyConcurrency !== null;
  return {
    ok,
    reason: ok ? null : 'No healthy baseline was established; no capacity recommendation is available.',
    target: target.origin,
    routes,
    steps,
    durationMs,
    warmupMs,
    requestsPerWorker,
    timeoutMs,
    minSamples,
    maxP95Ms,
    baselineP95Ms,
    latencyLimitMs,
    safetyMargin: 0.7,
    highestHealthyConcurrency,
    kneeObserved,
    observedKnee: kneeObserved ? highestHealthyConcurrency : null,
    recommendedCeiling: ok ? Math.max(1, Math.floor(highestHealthyConcurrency * 0.7)) : null,
    applied: false,
    warnings: [
      ...(ok && !kneeObserved ? ['Saturation was not established; the recommendation is limited to the healthy steps measured.'] : []),
      ...(results.some((result) => result.sample_limit_reached) ? ['A step reached the 100000-request cap; inspect its actual measurement duration.'] : []),
    ],
    results,
  };
}
