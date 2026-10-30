// Open-model load for the Tanod overload showcase.
//
// Arrivals follow a schedule (constant- or ramping-arrival-rate) that does not
// slow down when the server does: a slow server makes k6 run out of VUs and
// *drop* iterations rather than quietly offering less load. Dropped iterations
// are reported, never hidden.
//
// Every response is classified once, by status and by the X-Tanod debug header,
// into exactly one outcome counter. Cache hits are counted separately so that
// no analysis can mistake reuse for admission control.
//
// Configuration comes from environment variables (k6 -e NAME=value); see
// docs/demo/README.md. bench/showcase/run.sh sets all of them.
import http from 'k6/http';
import exec from 'k6/execution';
import { Counter, Trend } from 'k6/metrics';
import { OUTCOMES, classify, pathFor } from './lib.js';

const env = (name, fallback) => (__ENV[name] === undefined || __ENV[name] === '' ? fallback : __ENV[name]);
const int = (name, fallback) => {
  const value = Number(env(name, fallback));
  if (!Number.isInteger(value) || value < 0) {
    throw new Error(`${name} must be a non-negative integer, got ${env(name, fallback)}`);
  }
  return value;
};

export const config = {
  target: env('TARGET', 'http://127.0.0.1:18480'),
  // unique: every request a never-seen URL, so nothing can be cached or
  // coalesced. repeated: a small set of hot URLs. warmup: low-rate, distinct
  // URLs that the measured phase never requests.
  workload: env('WORKLOAD', 'unique'),
  route: env('ROUTE', '/products'),
  profile: env('PROFILE', 'constant'),
  rate: int('RATE', 60),
  baseRate: int('BASE_RATE', 10),
  duration: int('DURATION_S', 60),
  rampS: int('RAMP_S', 5),
  baseS: int('BASE_S', 15),
  recoveryS: int('RECOVERY_S', 20),
  hotPaths: int('HOT_PATHS', 4),
  clients: int('CLIENTS', 200),
  preVUs: int('PRE_VUS', 100),
  maxVUs: int('MAX_VUS', 2000),
  timeout: env('REQUEST_TIMEOUT', '10s'),
  runId: env('RUN_ID', 'local'),
  arm: env('ARM', 'unknown'),
  summaryPath: env('SUMMARY_PATH', 'summary.json'),
};

if (!['unique', 'repeated', 'warmup'].includes(config.workload)) {
  throw new Error(`WORKLOAD must be unique, repeated or warmup, got ${config.workload}`);
}
if (!['constant', 'spike'].includes(config.profile)) {
  throw new Error(`PROFILE must be constant or spike, got ${config.profile}`);
}
if (!/^\/[a-z0-9-]+$/.test(config.route)) {
  throw new Error(`ROUTE must be a single path segment like /products, got ${config.route}`);
}

// Requests still in flight when the schedule ends are allowed to finish, so an
// iteration is never cut off and silently miscounted.
const gracefulStop = `${Math.ceil(parseFloat(config.timeout)) + 5}s`;

function scenario() {
  if (config.profile === 'constant') {
    return {
      executor: 'constant-arrival-rate',
      rate: config.rate,
      timeUnit: '1s',
      duration: `${config.duration}s`,
      preAllocatedVUs: config.preVUs,
      maxVUs: config.maxVUs,
      gracefulStop,
    };
  }
  // A spike: steady base traffic, a fast ramp to the peak, the peak held, and a
  // recovery period at the base rate to show whether the system comes back.
  return {
    executor: 'ramping-arrival-rate',
    startRate: config.baseRate,
    timeUnit: '1s',
    preAllocatedVUs: config.preVUs,
    maxVUs: config.maxVUs,
    gracefulStop,
    stages: [
      { target: config.baseRate, duration: `${config.baseS}s` },
      { target: config.rate, duration: `${config.rampS}s` },
      { target: config.rate, duration: `${config.duration}s` },
      { target: config.baseRate, duration: `${config.rampS}s` },
      { target: config.baseRate, duration: `${config.recoveryS}s` },
    ],
  };
}

export const options = {
  scenarios: { load: scenario() },
  discardResponseBodies: true,
  // The outcome counters below are authoritative; k6's own http_req_failed
  // would count Tanod's intentional 503 as a failure like any other.
  summaryTrendStats: ['min', 'med', 'avg', 'p(90)', 'p(95)', 'p(99)', 'max', 'count'],
  // k6 tags every sample with these, so a shared Prometheus can tell runs apart.
  tags: { showcase_run: config.runId, arm: config.arm, workload: config.workload },
  noConnectionReuse: false,
};

// One counter per outcome. Exactly one is incremented per iteration.
const outcomes = Object.fromEntries(OUTCOMES.map((name) => [name, new Counter(`showcase_${name}`)]));
const okDuration = new Trend('showcase_ok_ms', true);
const okOriginDuration = new Trend('showcase_ok_origin_ms', true);
const shedDuration = new Trend('showcase_shed_ms', true);

export default function () {
  const iteration = exec.scenario.iterationInTest;
  const path = pathFor(config.workload, config.route, config.runId, iteration, config.hotPaths);
  const response = http.get(`${config.target}${path}`, {
    timeout: config.timeout,
    headers: { 'X-Showcase-Client': `visitor-${iteration % Math.max(1, config.clients)}` },
    // One name for all URLs keeps k6's per-URL tag cardinality bounded.
    tags: { name: `${config.route}/:slug` },
    responseCallback: http.expectedStatuses(200),
  });
  const outcome = classify(response.status, response.error_code, response.headers['X-Tanod']);
  outcomes[outcome].add(1);
  const elapsed = response.timings.duration;
  if (outcome === 'ok_origin' || outcome === 'ok_cache') okDuration.add(elapsed);
  if (outcome === 'ok_origin') okOriginDuration.add(elapsed);
  if (outcome === 'shed') shedDuration.add(elapsed);
}

export function handleSummary(data) {
  data.showcase_config = config;
  const value = (name) => (data.metrics[name] ? data.metrics[name].values.count : 0);
  const line = Object.keys(outcomes).map((name) => `${name}=${value(`showcase_${name}`)}`).join(' ');
  return {
    [config.summaryPath]: JSON.stringify(data, null, 2),
    stdout: `  ${config.arm}/${config.workload}: iterations=${value('iterations')} dropped=${value('dropped_iterations')} ${line}\n`,
  };
}
