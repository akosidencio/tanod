"""Tests for the overload showcase's configuration and result parsing.

    python3 -m unittest discover -s bench/showcase/tests -v

Standard library only. Checks that need a tool (node, k6, docker, a tanod
binary, Matplotlib) skip when it is absent and say so.
"""
import csv
import gzip
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
SHOWCASE_DIR = HERE.parent
REPO = SHOWCASE_DIR.parent.parent
sys.path.insert(0, str(SHOWCASE_DIR))

import showcase  # noqa: E402


def tool(name, *candidates):
    for candidate in (os.environ.get(name.upper() + '_BIN'), *candidates, shutil.which(name)):
        if candidate and Path(candidate).is_file() and os.access(candidate, os.X_OK):
            return candidate
    return None


def k6_summary(outcomes, iterations=None, dropped=0, ok_ms=None, config=None):
    metrics = {f'showcase_{name}': {'type': 'counter', 'values': {'count': count, 'rate': 1}}
               for name, count in outcomes.items() if count}
    total = sum(outcomes.values()) if iterations is None else iterations
    metrics['iterations'] = {'type': 'counter', 'values': {'count': total, 'rate': 1}}
    if dropped:
        metrics['dropped_iterations'] = {'type': 'counter', 'values': {'count': dropped, 'rate': 1}}
    if ok_ms:
        metrics['showcase_ok_ms'] = {'type': 'trend', 'values': ok_ms}
        metrics['showcase_ok_origin_ms'] = {'type': 'trend', 'values': ok_ms}
    return {
        'metrics': metrics,
        'state': {'testRunDurationMs': 61000},
        'showcase_config': config or {'profile': 'constant', 'rate': 10, 'duration': 10, 'maxVUs': 50},
    }


def outcomes(**counts):
    return {name: counts.get(name, 0) for name in showcase.OUTCOMES}


def tanod_metrics(origin_requests=0, hits=0, misses=0, shed_full=0, admitted=0, waits=None, ceiling=6):
    samples = [
        ['tanod_origin_requests_total', {'route': 'products', 'upstream': 'next-1:3000'}, origin_requests],
        ['tanod_cache_total', {'route': 'products', 'status': 'hit'}, hits],
        ['tanod_cache_total', {'route': 'products', 'status': 'miss'}, misses],
        ['tanod_admission_total', {'route': 'products', 'decision': 'shed_queue_full'}, shed_full],
        ['tanod_admission_total', {'route': 'products', 'decision': 'admitted'}, admitted],
        ['tanod_concurrency_limit', {'limiter': 'global'}, ceiling],
        ['tanod_responses_total', {'route': 'products', 'code': '2xx', 'source': 'origin'}, admitted],
    ]
    for le, count in (waits or {'0.1': 0, '1': 0, '+Inf': 0}).items():
        samples.append(['tanod_queue_wait_seconds_bucket', {'route': 'products', 'le': le}, count])
    return samples


def write_case(root, *, arm, workload, summary, before, after, samples, tanod_max=6, timeline=None):
    case = Path(root) / 'cases' / f'delay-{workload}-{arm}-t1'
    case.mkdir(parents=True)
    meta = showcase.case_meta(showcase.load_params({}), 'delay', workload, arm, 1, 'test-run')
    meta['tanod_max'] = tanod_max
    (case / 'case-meta.json').write_text(json.dumps(meta))
    (case / 'k6-summary.json').write_text(json.dumps(summary))
    (case / 'snapshot-before.json').write_text(json.dumps(before))
    (case / 'snapshot-after.json').write_text(json.dumps(after))
    (case / 'drain.json').write_text(json.dumps({'drain_seconds': 1.5, 'timeout_s': 60}))
    with open(case / 'samples.csv', 'w', newline='') as stream:
        writer = csv.writer(stream)
        writer.writerow(['time', 'source', 'metric', 'value'])
        writer.writerows(samples)
    if timeline is not None:
        with gzip.open(case / 'k6-requests.csv.gz', 'wt', newline='') as stream:
            writer = csv.writer(stream)
            writer.writerow(['metric_name', 'timestamp', 'metric_value', 'extra_tags'])
            writer.writerows(timeline)
    return case


def origin_samples(peaks_by_tick, interval=0.5):
    """Rows for three origins: per tick, (active, window_peak) per origin."""
    rows = []
    for tick, per_origin in enumerate(peaks_by_tick):
        at = 1000 + tick * interval
        for index, (active, peak) in enumerate(per_origin, start=1):
            rows += [[f'{at:.3f}', f'next-{index}', 'active', active],
                     [f'{at:.3f}', f'next-{index}', 'window_peak', peak],
                     [f'{at:.3f}', f'next-{index}', 'total', 0]]
    return rows


def origin_snapshot(totals, tanod=None):
    return {'time': 0, 'origins': {f'next-{i}': {'active': 0, 'peak': 0, 'total': total, 'window_peak': 0}
                                   for i, total in enumerate(totals, start=1)}, 'tanod': tanod}


class Params(unittest.TestCase):
    def test_defaults_are_valid_and_complete(self):
        params = showcase.load_params({})
        self.assertEqual(params['modes'], 'delay', 'the mechanism test stays the default; cpu is opt-in')
        self.assertEqual(set(showcase.MODE_DEFAULTS), set(showcase.MODES))
        for mode in showcase.MODES:
            effective = showcase.mode_params(params, mode)
            self.assertGreater(effective['rate'], 0)
            self.assertGreater(effective['tanod_max'], 0)

    def test_environment_overrides_and_types(self):
        params = showcase.load_params({'SHOWCASE_RATE': '75', 'SHOWCASE_TANOD_CACHE': 'off',
                                       'SHOWCASE_MODES': 'delay,cpu', 'SHOWCASE_SAMPLE_INTERVAL_S': '0.25'})
        self.assertEqual(params['rate'], 75)
        self.assertIs(params['tanod_cache'], False)
        self.assertEqual(params['sample_interval_s'], 0.25)
        self.assertEqual(showcase.mode_params(params, 'cpu')['rate'], 75, 'an explicit rate applies to every mode')

    def test_invalid_values_are_rejected(self):
        for env in ({'SHOWCASE_MODES': 'gpu'}, {'SHOWCASE_WORKLOADS': ','}, {'SHOWCASE_ARMS': 'tanod,haproxy'},
                    {'SHOWCASE_PROFILE': 'closed'}, {'SHOWCASE_NGINX_RATE': '20'},
                    {'SHOWCASE_TANOD_QUEUE_TIMEOUT': '1 second'}, {'SHOWCASE_PRE_VUS': '5000'},
                    {'SHOWCASE_ORIGIN_PORTS': '1,2'}, {'SHOWCASE_TRIALS': '0'}):
            with self.subTest(env=env), self.assertRaises(ValueError):
                showcase.load_params(env)
        with self.assertRaises(ValueError):
            showcase.load_params({'SHOWCASE_DURATION_S': 'sixty'})

    def test_scheduled_iterations(self):
        self.assertEqual(showcase.scheduled_iterations('constant', 120, 60), 7200)
        # base 10*15 + ramp (10+100)/2*5 + 100*30 + ramp down 275 + recovery 10*20
        self.assertEqual(showcase.scheduled_iterations('spike', 100, 30, 10, 15, 5, 20), 150 + 275 + 3000 + 275 + 200)
        self.assertEqual(showcase.schedule_seconds('spike', 30, 15, 5, 20), 75)


class Configuration(unittest.TestCase):
    def setUp(self):
        self.params = showcase.load_params({})

    def test_arms_share_every_nginx_setting_but_the_upstream(self):
        baseline = showcase.render_nginx(self.params, 'baseline').splitlines()
        tanod = showcase.render_nginx(self.params, 'tanod').splitlines()
        differing = [line.strip() for line in set(baseline) ^ set(tanod)]
        self.assertEqual(sorted(differing), sorted([
            'server next-1:3000;', 'server next-2:3000;', 'server next-3:3000;', 'server tanod:8080;']))
        text = '\n'.join(baseline)
        self.assertIn('limit_req_status 429;', text, 'rate-limit rejections must be distinguishable from shed 503s')
        self.assertIn('limit_req_zone $http_x_showcase_client', text)
        self.assertIn('burst=40 nodelay', text)

    def test_nginx_timeouts_default_to_nginx_defaults(self):
        text = showcase.render_nginx(self.params, 'baseline')
        self.assertIn('proxy_connect_timeout 60s;', text)
        self.assertIn('proxy_read_timeout 60s;', text)
        params = showcase.load_params({'SHOWCASE_NGINX_CONNECT_TIMEOUT': '2s'})
        self.assertIn('proxy_connect_timeout 2s;', showcase.render_nginx(params, 'tanod'))
        with self.assertRaises(ValueError):
            showcase.load_params({'SHOWCASE_NGINX_CONNECT_TIMEOUT': 'soon'})

    def test_ip_keyed_limit(self):
        params = showcase.load_params({'SHOWCASE_NGINX_LIMIT_KEY': 'ip'})
        self.assertIn('limit_req_zone $binary_remote_addr', showcase.render_nginx(params, 'baseline'))

    def test_tanod_config_matches_parameters(self):
        text = showcase.render_tanod(self.params, 'cpu')
        self.assertIn('max: 6\n', text)
        self.assertIn('load_balancing: round_robin', text, 'must match NGINX round robin in the baseline arm')
        for upstream in ('next-1:3000', 'next-2:3000', 'next-3:3000'):
            self.assertIn(f'"{upstream}"', text)
        params = showcase.load_params({'SHOWCASE_TANOD_CACHE': '0', 'SHOWCASE_TANOD_MAX': '9'})
        text = showcase.render_tanod(params, 'delay')
        self.assertIn('max: 9\n', text)
        self.assertIn('enabled: false', text)

    def test_unrendered_placeholder_is_an_error(self):
        with self.assertRaises(ValueError):
            showcase.render_template('a @@MISSING@@ b', {})

    def test_rendered_tanod_config_passes_tanod_check(self):
        binary = tool('tanod', str(REPO / 'target/debug/tanod'), str(REPO / 'target/release/tanod'))
        if not binary:
            self.skipTest('no tanod binary (cargo build, or set TANOD_BIN)')
        with tempfile.TemporaryDirectory() as directory:
            for env in ({}, {'SHOWCASE_TANOD_CACHE': '0'}, {'SHOWCASE_TANOD_CACHE': '0', 'SHOWCASE_TANOD_COALESCE': '0'}):
                for mode in showcase.MODES:
                    path = Path(directory) / 'tanod.yaml'
                    path.write_text(showcase.render_tanod(showcase.load_params(env), mode))
                    result = subprocess.run([binary, 'check', '--config', str(path)], capture_output=True, text=True)
                    self.assertEqual(result.returncode, 0, f'{env} {mode}: {result.stderr}{result.stdout}')

    def test_compose_origins_are_identical(self):
        text = (SHOWCASE_DIR / 'compose.yaml').read_text()
        if shutil.which('docker') and subprocess.run(['docker', 'compose', 'version'], capture_output=True).returncode == 0:
            env = {**os.environ, 'SHOWCASE_CONFIG_DIR': '/tmp'}
            result = subprocess.run(['docker', 'compose', '-f', str(SHOWCASE_DIR / 'compose.yaml'), '--profile', 'tanod',
                                     '--profile', 'observe', 'config', '--format', 'json'],
                                    capture_output=True, text=True, env=env)
            self.assertEqual(result.returncode, 0, result.stderr)
            services = json.loads(result.stdout)['services']
        else:
            try:
                import yaml
            except ImportError:
                self.skipTest('neither docker compose nor PyYAML is available')
            services = yaml.safe_load(text)['services']
            if '<<' in str(services):
                self.skipTest('PyYAML did not resolve merge keys')
        origins = [services[f'next-{i}'] for i in (1, 2, 3)]
        for key in ('image', 'cpus', 'mem_limit', 'healthcheck'):
            values = [json.dumps(origin.get(key), sort_keys=True) for origin in origins]
            self.assertEqual(len(set(values)), 1, f'origins differ in {key}: {values}')
        environments = [{k: v for k, v in origin['environment'].items() if k != 'INSTANCE_ID'} for origin in origins]
        self.assertTrue(all(env == environments[0] for env in environments), environments)
        self.assertEqual(environments[0]['TANOD_FIXTURE_STATS'], '1')
        self.assertNotIn('depends_on', services['edge'], 'the edge must be identical in both arms')
        for name in ('next-1', 'edge'):
            self.assertNotIn('profiles', services[name])
        self.assertIn('tanod', services['tanod'].get('profiles', []))


class K6Script(unittest.TestCase):
    def test_classification_and_paths(self):
        node = tool('node', os.path.expanduser('~/.local/lib/node22/bin/node'))
        if not node:
            self.skipTest('node is not installed')
        program = """
const lib = await import(process.argv[1]);
const cases = [
  [200, 0, 'MISS'], [200, 0, 'HIT'], [200, 0, 'STALE'], [200, 0, undefined], [503, 0, 'SHED'],
  [503, 0, undefined], [502, 0, undefined], [429, 0, undefined], [0, 1050, undefined], [0, 1211, undefined],
  [404, 0, undefined], [504, 0, 'MISS']];
console.log(JSON.stringify({
  outcomes: lib.OUTCOMES,
  classified: cases.map(([s, e, h]) => lib.classify(s, e, h)),
  paths: [lib.pathFor('unique', '/catalog', 't1', 7, 4), lib.pathFor('repeated', '/products', 't1', 7, 4),
          lib.pathFor('warmup', '/products', 't2', 0, 4)],
}));
"""
        result = subprocess.run([node, '--input-type=module', '-e', program,
                                 (SHOWCASE_DIR / 'k6/lib.js').as_uri()], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        data = json.loads(result.stdout)
        self.assertEqual(data['outcomes'], showcase.OUTCOMES, 'k6 and the analysis must agree on outcome names')
        self.assertEqual(data['classified'], [
            'ok_origin', 'ok_cache', 'ok_cache', 'ok_origin', 'shed', 'upstream_error', 'upstream_error',
            'rate_limited', 'timeout', 'transport_error', 'other_status', 'upstream_error'])
        self.assertEqual(data['paths'], ['/catalog/u-t1-7', '/products/hot-3', '/products/warmup-t2-0'])

    def test_k6_loads_the_script_and_rejects_bad_input(self):
        k6 = tool('k6', os.path.expanduser('~/.local/bin/k6'))
        if not k6:
            self.skipTest('k6 is not installed')
        script = str(SHOWCASE_DIR / 'k6/overload.js')
        for env in (['-e', 'PROFILE=constant'], ['-e', 'PROFILE=spike', '-e', 'WORKLOAD=repeated']):
            result = subprocess.run([k6, 'inspect', *env, script], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            scenario = json.loads(result.stdout)['scenarios']['load']
            self.assertIn(scenario['executor'], ('constant-arrival-rate', 'ramping-arrival-rate'),
                          'the load must be an open model')
            self.assertEqual(scenario['gracefulStop'], '15s', 'in-flight requests must be allowed to finish')
        for bad in (['-e', 'WORKLOAD=bogus'], ['-e', 'RATE=-1'], ['-e', 'ROUTE=/a/b']):
            result = subprocess.run([k6, 'inspect', *bad, script], capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0, f'{bad} was accepted')


class Parsing(unittest.TestCase):
    def test_prometheus_text(self):
        text = '\n'.join([
            '# HELP tanod_cache_total Cache outcomes',
            'tanod_cache_total{route="products",status="hit"} 12',
            'tanod_cache_total{route="catalog",status="hit"} 3',
            'tanod_cache_total{route="products",status="miss"} 4 1700000000000',
            'tanod_origin_in_flight{limiter="global"} 6',
            'other_metric 1',
            'tanod_odd{label="quote \\" inside"} 2',
        ])
        samples = showcase.parse_prometheus(text, 'tanod_')
        self.assertEqual(len(samples), 5)
        self.assertEqual(showcase.metric_sum(samples, 'tanod_cache_total', status='hit'), 15)
        self.assertEqual(showcase.metric_by(samples, 'tanod_cache_total', 'status'), {'hit': 15, 'miss': 4})
        self.assertEqual(samples[-1][1]['label'], 'quote " inside')

    def test_bucket_bound(self):
        cumulative = [(0.1, 50), (0.5, 90), (1.0, 100), (float('inf'), 100)]
        self.assertEqual(showcase.bucket_bound(0.5, cumulative), 0.1)
        self.assertEqual(showcase.bucket_bound(0.95, cumulative), 1.0)
        self.assertIsNone(showcase.bucket_bound(0.95, [(float('inf'), 0)]))
        self.assertIsNone(showcase.bucket_bound(0.99, [(1.0, 10), (float('inf'), 100)]), 'beyond the last bound')

    def test_origin_concurrency_from_logs(self):
        lines = {
            'next-1': ['next-1-1  | 2026-10-30T14:00:00.100000000Z {"event":"render_start","active":1}',
                       'next-1-1  | 2026-10-30T14:00:00.300000000Z {"event":"render_start","active":2}',
                       'next-1-1  | 2026-10-30T14:00:00.900000000Z {"event":"render_end","active":1}',
                       'next-1-1  | 2026-10-30T14:00:01.500000000Z {"event":"render_end","active":0}',
                       'next-1-1  | 2026-10-30T14:00:00.200000000Z \u25b2 Next.js 16.4.0'],
            'next-2': ['next-2-1  | 2026-10-30T13:59:59.000000000Z {"event":"render_start","active":1}',
                       'next-2-1  | 2026-10-30T14:00:00.500000000Z {"event":"render_start","active":2}',
                       'next-2-1  | 2026-10-30T14:00:02.000000000Z {"event":"render_end","active":1}'],
            'next-3': [],
        }
        events = {name: showcase.origin_log_events(text) for name, text in lines.items()}
        self.assertEqual(len(events['next-1']), 4, 'non-JSON lines are ignored')
        start = showcase._parse_timestamp('2026-10-30T14:00:00Z')
        result = showcase.origin_concurrency_from_logs(events, start, start + 1.0, bin_s=0.5)
        # next-2 entered the window already rendering one; at 0.5s it is 2 + 2.
        self.assertEqual(result['peak_total'], 4)
        self.assertEqual(result['peak_per_origin'], {'next-1': 2, 'next-2': 2, 'next-3': 0})
        self.assertEqual(result['events'], 4, 'events outside the window are excluded')
        self.assertEqual(result['series'], [(0.0, 3), (0.5, 4), (1.0, 4)])
        self.assertIsNone(showcase.origin_concurrency_from_logs({'next-1': []}, start, start + 1))

    def test_edge_errors(self):
        with tempfile.TemporaryDirectory() as directory:
            (Path(directory) / 'logs').mkdir()
            with gzip.open(Path(directory) / 'logs/edge.log.gz', 'wt') as stream:
                stream.write('edge-1  | 2026/10/30 15:18:01 [error] 31#31: *353 no live upstreams while connecting '
                             'to upstream, client: 172.18.0.1, server: , request: "GET /catalog/u-t1-9 HTTP/1.1"\n')
                stream.write('edge-1  | 2026/10/30 15:18:01 [error] 31#31: *354 no live upstreams while connecting '
                             'to upstream, client: 172.18.0.1\n')
                stream.write('edge-1  | 2026/10/30 15:18:00 [error] 31#31: *300 upstream timed out (110: Operation '
                             'timed out) while connecting to upstream, client: 172.18.0.1\n')
                stream.write('edge-1  | /docker-entrypoint.sh: Configuration complete; ready for start up\n')
            self.assertEqual(showcase.edge_errors(directory), {
                'no live upstreams': 2,
                'upstream timed out (110: Operation timed out) while connecting to': 1})
            self.assertIsNone(showcase.edge_errors(Path(directory) / 'missing'))

    def test_container_checks(self):
        def container(service, created, status='running', oom=False):
            return {'Name': f'/x-{service}-1', 'Created': created, 'RestartCount': 0,
                    'Config': {'Labels': {'com.docker.compose.service': service}},
                    'State': {'Status': status, 'OOMKilled': oom}}
        started = showcase._parse_timestamp('2026-10-30T14:00:00Z')
        result = showcase.container_checks([container('next-1', '2026-10-30T14:00:05.123456789Z', 'exited', True),
                                            container('edge', '2026-10-30T14:00:09.5Z')], started)
        self.assertEqual(result['services'], ['edge', 'next-1'])
        self.assertTrue(result['created_after_case_start'])
        self.assertEqual(result['oom_killed'], ['next-1'])
        self.assertEqual(result['not_running'], ['next-1'])
        stale = showcase.container_checks([container('next-1', '2026-10-30T13:00:00Z')], started)
        self.assertFalse(stale['created_after_case_start'])

    def test_k6_summary(self):
        summary = k6_summary(outcomes(ok_origin=80, shed=15, timeout=5), dropped=20,
                             ok_ms={'med': 50, 'p(90)': 80, 'p(95)': 90, 'p(99)': 120, 'max': 300, 'count': 80})
        result = showcase.summarize_k6(summary)
        self.assertEqual(result['iterations'], 100)
        self.assertEqual(result['dropped_iterations'], 20)
        self.assertEqual(result['attempted_iterations'], 120)
        self.assertEqual(result['successful'], 80)
        self.assertAlmostEqual(result['success_share_of_attempted'], 80 / 120, places=4)
        self.assertEqual(result['successful_per_second'], 8.0)
        self.assertEqual(result['ok_ms']['p95'], 90)
        self.assertEqual(result['shed_ms']['count'], 0)
        self.assertIsNone(result['shed_ms']['p95'])

    def test_inconsistent_k6_summary_is_rejected(self):
        with self.assertRaises(ValueError):
            showcase.summarize_k6(k6_summary(outcomes(ok_origin=10), iterations=12))

    def test_samples_peaks_and_resources(self):
        rows = origin_samples([[(1, 2), (1, 1), (0, 1)], [(4, 5), (3, 3), (2, 4)], [(0, 1), (0, 0), (0, 0)]])
        rows += [['1000.000', 'container:next-1', 'cpu_ns', 0], ['1001.000', 'container:next-1', 'cpu_ns', 5e8],
                 ['1000.000', 'container:next-1', 'memory_bytes', 100 * 2**20],
                 ['1001.000', 'container:next-1', 'memory_bytes', 150 * 2**20],
                 ['1000.500', 'next-2', 'error', 'TimeoutError']]
        result = showcase.summarize_samples([dict(zip(['time', 'source', 'metric', 'value'], map(str, r))) for r in rows], 0.5)
        self.assertEqual(result['per_origin']['next-1']['peak_concurrent'], 5)
        self.assertEqual(result['peak_any_single_origin'], 5)
        self.assertEqual(result['peak_total_active_sampled'], 9)
        self.assertEqual(result['peak_total_window_upper_bound'], 12)
        self.assertEqual(result['resources']['next-1']['cpu_seconds'], 0.5)
        self.assertEqual(result['resources']['next-1']['mean_cores'], 0.5)
        self.assertEqual(result['resources']['next-1']['peak_memory_mib'], 150.0)
        self.assertEqual(result['sample_errors'], {'next-2': 1})

    def test_timeline(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'requests.csv.gz'
            with gzip.open(path, 'wt', newline='') as stream:
                writer = csv.writer(stream)
                writer.writerow(['metric_name', 'timestamp', 'metric_value'])
                writer.writerows([['http_reqs', 100, 1], ['showcase_ok_origin', 100, 1], ['showcase_ok_ms', 100, 40],
                                  ['showcase_ok_origin', 100, 1], ['showcase_ok_ms', 100, 60],
                                  ['showcase_shed', 102, 1]])
            timeline = showcase.read_timeline(path)
            anchored = showcase.read_timeline(path, start=98.6)
        self.assertEqual([row['t'] for row in anchored][:3], [0, 1, 2], 'anchored at the phase start')
        self.assertEqual(anchored[2]['ok_origin'], 2)
        self.assertEqual([row['t'] for row in timeline], [0, 1, 2], 'empty seconds are kept, not skipped')
        self.assertEqual(timeline[0]['ok_origin'], 2)
        self.assertEqual(timeline[0]['ok_p50_ms'], 40)
        self.assertEqual(timeline[0]['ok_p95_ms'], 60)
        self.assertEqual(timeline[1]['ok_origin'], 0)
        self.assertIsNone(timeline[1]['ok_p50_ms'])
        self.assertEqual(timeline[2]['shed'], 1)


class Analysis(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        self.params = showcase.load_params({})
        (self.root / 'params.json').write_text(json.dumps(self.params))

    def tearDown(self):
        self.directory.cleanup()

    def tanod_case(self, workload='unique', ok_cache=0, hits=0, peaks=None):
        peaks = peaks or [[(2, 2), (2, 2), (2, 2)], [(2, 2), (2, 2), (1, 2)]]
        rows = origin_samples(peaks)
        rows += [['1000.000', 'tanod', 'in_flight', 6], ['1000.000', 'tanod', 'queue_depth', 30]]
        summary = k6_summary(outcomes(ok_origin=60, ok_cache=ok_cache, shed=40),
                             ok_ms={'med': 50, 'p(90)': 80, 'p(95)': 90, 'p(99)': 120, 'max': 300, 'count': 60 + ok_cache})
        return write_case(self.root, arm='tanod', workload=workload, summary=summary,
                          before=origin_snapshot([10, 10, 10], tanod_metrics()),
                          after=origin_snapshot([30, 30, 30], tanod_metrics(origin_requests=60, hits=hits,
                                                                            misses=60, shed_full=40, admitted=60,
                                                                            waits={'0.1': 50, '1': 60, '+Inf': 60})),
                          samples=rows, timeline=[['showcase_ok_origin', 5, 1], ['showcase_ok_ms', 5, 50]])

    def test_tanod_case(self):
        case = showcase.analyze_case(self.tanod_case(), self.params)
        self.assertEqual(case['origin']['renders'], 60)
        self.assertEqual(case['origin']['renders_per_origin'], {'next-1': 20, 'next-2': 20, 'next-3': 20})
        self.assertEqual(case['origin']['renders_not_delivered_as_200'], 0)
        self.assertEqual(case['tanod']['admission'], {'admitted': 60, 'shed_queue_full': 40})
        self.assertEqual(case['tanod']['cache'], {'miss': 60})
        self.assertEqual(case['tanod']['configured_ceiling'], 6)
        self.assertEqual(case['tanod']['peak_queue_depth_sampled'], 30)
        # rank 57 of 60 falls in the (0.1s, 1s] bucket.
        self.assertEqual(case['tanod']['queue_wait_p95_at_most_s'], 1.0)
        self.assertEqual(case['checks'], {'no_cache_reuse': True, 'ceiling_held': True})
        self.assertTrue((self.root / 'cases/delay-unique-tanod-t1/timeline.csv').exists())

    def test_cache_hits_invalidate_admission_evidence(self):
        case = showcase.analyze_case(self.tanod_case(ok_cache=5, hits=5), self.params)
        self.assertFalse(case['checks']['no_cache_reuse'])
        self.assertTrue(any('invalid' in note for note in case['notes']))

    def test_repeated_workload_does_not_claim_admission(self):
        case = showcase.analyze_case(self.tanod_case(workload='repeated', ok_cache=500, hits=500), self.params)
        self.assertNotIn('no_cache_reuse', case['checks'])
        self.assertIsNone(case['origin']['renders_not_delivered_as_200'])

    def test_ceiling_breach_is_reported(self):
        case = showcase.analyze_case(self.tanod_case(peaks=[[(3, 4), (3, 3), (2, 2)]]), self.params)
        self.assertFalse(case['checks']['ceiling_held'])

    def baseline_case(self, peaks):
        summary = k6_summary(outcomes(ok_origin=50, timeout=30, upstream_error=5), dropped=15,
                             ok_ms={'med': 900, 'p(90)': 5000, 'p(95)': 8000, 'p(99)': 9500, 'max': 9900, 'count': 50})
        return write_case(self.root, arm='baseline', workload='unique', summary=summary,
                          before=origin_snapshot([0, 0, 0]), after=origin_snapshot([30, 30, 30]),
                          samples=origin_samples(peaks))

    def test_baseline_case(self):
        case = showcase.analyze_case(self.baseline_case([[(20, 25), (19, 22), (21, 23)]]), self.params)
        self.assertIsNone(case['tanod'])
        self.assertEqual(case['checks'], {'no_cache_reuse': True, 'load_exceeded_ceiling': True})
        self.assertEqual(case['origin']['renders_not_delivered_as_200'], 40)
        self.assertEqual(case['origin']['renders_per_successful_response'], 1.8)
        self.assertTrue(any('dropped 15' in note for note in case['notes']))

    def test_stale_or_mixed_stack_fails_isolation(self):
        case_dir = self.baseline_case([[(20, 25), (19, 22), (21, 23)]])
        meta = json.loads((case_dir / 'case-meta.json').read_text())
        meta['started_epoch'] = showcase._parse_timestamp('2026-10-30T14:00:00Z')
        (case_dir / 'case-meta.json').write_text(json.dumps(meta))

        def container(service, created='2026-10-30T14:00:05Z', oom=False):
            return {'Created': created, 'Config': {'Labels': {'com.docker.compose.service': service}},
                    'State': {'Status': 'running', 'OOMKilled': oom}}
        fresh = [container(name) for name in ('next-1', 'next-2', 'next-3', 'edge')]
        (case_dir / 'containers.json').write_text(json.dumps(fresh))
        self.assertTrue(showcase.analyze_case(case_dir, self.params)['checks']['isolated_fresh_stack'])
        # A Tanod container left over from the previous case, in the baseline arm.
        (case_dir / 'containers.json').write_text(json.dumps(fresh + [container('tanod', '2026-10-30T13:59:00Z')]))
        self.assertFalse(showcase.analyze_case(case_dir, self.params)['checks']['isolated_fresh_stack'])
        # An origin reused from an earlier case, and an OOM kill.
        stale = [container('next-1', '2026-10-30T13:00:00Z', oom=True)] + fresh[1:]
        (case_dir / 'containers.json').write_text(json.dumps(stale))
        case = showcase.analyze_case(case_dir, self.params)
        self.assertFalse(case['checks']['isolated_fresh_stack'])
        self.assertTrue(any('OOM' in note for note in case['notes']))

    def test_underpowered_baseline_is_flagged(self):
        case = showcase.analyze_case(self.baseline_case([[(1, 1), (2, 2), (1, 1)]]), self.params)
        self.assertFalse(case['checks']['load_exceeded_ceiling'])

    def test_aggregate(self):
        for case_dir in (self.tanod_case(), self.baseline_case([[(20, 25), (19, 22), (21, 23)]])):
            showcase.analyze_case(case_dir, self.params)
        results = showcase.aggregate(self.root)
        self.assertEqual([c['arm'] for c in results['cases']], ['baseline', 'tanod'])
        with open(self.root / 'results.csv', newline='') as stream:
            rows = list(csv.DictReader(stream))
        self.assertEqual(list(rows[0]), showcase.CSV_COLUMNS)
        self.assertEqual(rows[1]['outcome_shed'], '40')
        comparison = results['comparisons'][0]
        self.assertEqual(comparison['tanod']['shed'], 40)
        self.assertEqual(comparison['baseline']['timeouts'], 30)
        summary = (self.root / 'summary.md').read_text()
        self.assertIn('| delay | unique | tanod | 1 |', summary)
        self.assertIn('mechanism test', summary)

    def test_charts_use_only_measured_cases(self):
        try:
            import matplotlib  # noqa: F401
        except ImportError:
            self.skipTest('Matplotlib is not installed')
        for case_dir in (self.tanod_case(), self.baseline_case([[(20, 25), (19, 22), (21, 23)]])):
            showcase.analyze_case(case_dir, self.params)
        showcase.aggregate(self.root)
        result = subprocess.run([sys.executable, str(SHOWCASE_DIR / 'plot.py'), str(self.root)],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        manifest = json.loads((self.root / 'charts/manifest.json').read_text())
        measured = {'delay-unique-tanod-t1', 'delay-unique-baseline-t1'}
        for chart in manifest['charts']:
            self.assertTrue(set(chart['cases']) <= measured, chart)
            for name in chart['files']:
                self.assertTrue((self.root / 'charts' / name).stat().st_size > 0)


class Observability(unittest.TestCase):
    def test_dashboard_parses_and_queries_known_series(self):
        dashboard = json.loads((SHOWCASE_DIR / 'observe/dashboard.json').read_text())
        self.assertEqual(dashboard['uid'], 'tanod-showcase')
        expressions = [target['expr'] for panel in dashboard['panels'] for target in panel.get('targets', [])]
        self.assertTrue(expressions)
        for expr in expressions:
            self.assertRegex(expr, r'(k6_|tanod_|fixture_)', expr)


if __name__ == '__main__':
    unittest.main()
