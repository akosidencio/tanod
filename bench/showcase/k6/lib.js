// Pure helpers for overload.js. No k6 imports, so bench/showcase/tests can run
// them under Node and hold the classification rules to their tests.

// k6 error code 1050 is a request timeout. Anything else with status 0 is a
// connection-level failure (refused, reset, EOF).
export const TIMEOUT_CODE = 1050;

export const OUTCOMES = [
  'ok_origin',
  'ok_cache',
  'shed',
  'rate_limited',
  'upstream_error',
  'timeout',
  'transport_error',
  'other_status',
];

// Exactly one outcome per response. A 200 that Tanod answered from its cache is
// `ok_cache`, never `ok_origin`: reuse is not admission control. A 503 is only
// `shed` when Tanod says it shed the request; an NGINX or origin 5xx is an
// `upstream_error` in either arm.
export function classify(status, errorCode, tanodHeader) {
  const cacheStatus = (tanodHeader || '').toUpperCase();
  if (status === 200) {
    return cacheStatus === 'HIT' || cacheStatus === 'STALE' ? 'ok_cache' : 'ok_origin';
  }
  if (status === 503 && cacheStatus === 'SHED') return 'shed';
  if (status === 429) return 'rate_limited';
  if (status >= 500) return 'upstream_error';
  if (status === 0) return errorCode === TIMEOUT_CODE ? 'timeout' : 'transport_error';
  return 'other_status';
}

export function pathFor(workload, route, runId, iteration, hotPaths) {
  if (workload === 'repeated') {
    return `${route}/hot-${iteration % Math.max(1, hotPaths)}`;
  }
  // The run id is per trial, not per arm, so both arms of a trial request
  // exactly the same URL sequence.
  const prefix = workload === 'warmup' ? 'warmup' : 'u';
  return `${route}/${prefix}-${runId}-${iteration}`;
}
