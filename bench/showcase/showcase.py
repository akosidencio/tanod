#!/usr/bin/env python3
"""Configuration, sampling and analysis for the Tanod overload showcase.

Standard library only, so the parsing and analysis are testable anywhere;
plot.py is the only part that needs Matplotlib. run.sh is the entry point:

    showcase.py params    --out params.json            (defaults + SHOWCASE_* env)
    showcase.py render    --params P --mode M --arm A --out DIR
    showcase.py snapshot  --params P --arm A --out FILE [--reset-window]
    showcase.py sample    --params P --arm A --out FILE --stop FILE [--container NAME=ID ...]
    showcase.py drain     --params P --out FILE
    showcase.py case-meta --params P --out FILE --mode M --workload W --arm A --trial N --run-id ID
    showcase.py case      --params P --dir CASE_DIR
    showcase.py aggregate --run RUN_DIR
    showcase.py metadata  --params P --out FILE [--image NAME=REF ...]

Every number written to results.json is derived from files in the run
directory: the k6 summary and request log, the origin's own render counters,
Tanod's Prometheus metrics and container statistics. Nothing is estimated
unless its field name says so.
"""
import argparse
import csv
import datetime
import gzip
import http.client
import json
import math
import os
import platform
import re
import socket
import statistics
import subprocess
import sys
import threading
import time
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent

OUTCOMES = [
    'ok_origin', 'ok_cache', 'shed', 'rate_limited',
    'upstream_error', 'timeout', 'transport_error', 'other_status',
]
ARMS = ('baseline', 'tanod')
WORKLOADS = ('unique', 'repeated')
MODES = ('delay', 'cpu')

# What each mode exercises. `delay` is the fixture's sleeping product page: a
# mechanism test, because a sleep occupies no CPU and the origin is never really
# saturated. `cpu` is the opt-in catalog page that does a fixed amount of CPU
# work per request. The rates and ceilings are starting points for the default
# resources (1 CPU per origin); see docs/demo/README.md before changing them.
MODE_DEFAULTS = {
    'delay': {'route': '/products', 'rate': 120, 'tanod_max': 12},
    'cpu': {'route': '/catalog', 'rate': 90, 'tanod_max': 6},
}

DEFAULTS = {
    'modes': 'delay',
    'workloads': 'unique,repeated',
    'arms': 'baseline,tanod',
    'trials': 1,
    'profile': 'constant',
    'rate': None,                 # None: the mode's default
    'tanod_max': None,            # None: the mode's default
    'duration_s': 60,
    'base_rate': 10,
    'base_s': 15,
    'ramp_s': 5,
    'recovery_s': 20,
    'warmup_s': 15,
    'warmup_rate': 5,
    'cooldown_s': 5,
    'drain_timeout_s': 60,
    'request_timeout_s': 10,
    'pre_vus': 200,
    'max_vus': 2000,
    'hot_paths': 4,
    'clients': 200,
    'nginx_rate': '20r/s',
    'nginx_burst': 40,
    'nginx_limit_key': 'client',
    'nginx_workers': 2,
    'nginx_connect_timeout': '60s',
    'nginx_proxy_timeout': '60s',
    'tanod_queue_max': 50,
    'tanod_queue_timeout': '1s',
    'tanod_cache': True,
    'tanod_coalesce': True,
    'tanod_cache_ttl': '1s',
    'origin_cpus': '1.0',
    'origin_memory': '768m',
    'tanod_cpus': '1.0',
    'edge_cpus': '1.0',
    'render_delay_ms': 350,
    'catalog_units': 40000,
    'sample_interval_s': 0.5,
    'observe': False,
    'port': 18480,
    'origin_ports': '13401,13402,13403',
    'tanod_metrics_port': 19490,
    'tanod_admin_port': 19491,
    'prometheus_port': 19400,
}

NGINX_LIMIT_KEYS = {'client': '$http_x_showcase_client', 'ip': '$binary_remote_addr'}
DURATION_RE = re.compile(r'^[1-9][0-9]*(ms|s|m)$')
RATE_RE = re.compile(r'^[1-9][0-9]*r/[sm]$')


# --------------------------------------------------------------------------
# Parameters
# --------------------------------------------------------------------------

def _coerce(name, raw):
    default = DEFAULTS[name]
    if isinstance(default, bool):
        if str(raw).lower() in ('1', 'true', 'yes', 'on'):
            return True
        if str(raw).lower() in ('0', 'false', 'no', 'off'):
            return False
        raise ValueError(f'{name}: expected a boolean, got {raw!r}')
    if isinstance(default, int) or name in ('rate', 'tanod_max'):
        try:
            return int(raw)
        except ValueError:
            raise ValueError(f'{name}: expected an integer, got {raw!r}') from None
    if isinstance(default, float):
        return float(raw)
    return str(raw)


def load_params(environ=None):
    """Defaults overridden by SHOWCASE_<NAME> environment variables."""
    environ = os.environ if environ is None else environ
    params = dict(DEFAULTS)
    for name in DEFAULTS:
        raw = environ.get('SHOWCASE_' + name.upper())
        if raw not in (None, ''):
            params[name] = _coerce(name, raw)
    validate_params(params)
    return params


def _csv(value):
    return [item.strip() for item in str(value).split(',') if item.strip()]


def validate_params(params):
    errors = []
    for key, allowed in (('modes', MODES), ('workloads', WORKLOADS), ('arms', ARMS)):
        items = _csv(params[key])
        if not items:
            errors.append(f'{key} is empty')
        errors += [f'{key}: unknown {item!r} (allowed: {", ".join(allowed)})'
                   for item in items if item not in allowed]
    if params['profile'] not in ('constant', 'spike'):
        errors.append(f"profile must be constant or spike, got {params['profile']!r}")
    if params['nginx_limit_key'] not in NGINX_LIMIT_KEYS:
        errors.append(f"nginx_limit_key must be one of {sorted(NGINX_LIMIT_KEYS)}")
    if not RATE_RE.match(str(params['nginx_rate'])):
        errors.append(f"nginx_rate must look like 20r/s, got {params['nginx_rate']!r}")
    for key in ('tanod_queue_timeout', 'tanod_cache_ttl', 'nginx_proxy_timeout', 'nginx_connect_timeout'):
        if not DURATION_RE.match(str(params[key])):
            errors.append(f'{key} must look like 1s or 500ms, got {params[key]!r}')
    for key in ('trials', 'duration_s', 'warmup_rate', 'warmup_s', 'request_timeout_s',
                'pre_vus', 'max_vus', 'hot_paths', 'clients', 'nginx_burst',
                'nginx_workers', 'tanod_queue_max', 'catalog_units'):
        if not isinstance(params[key], int) or params[key] < 1:
            errors.append(f'{key} must be a positive integer, got {params[key]!r}')
    for key in ('rate', 'tanod_max'):
        if params[key] is not None and params[key] < 1:
            errors.append(f'{key} must be a positive integer')
    if params['pre_vus'] > params['max_vus']:
        errors.append('pre_vus must not exceed max_vus')
    if len(_csv(params['origin_ports'])) != 3:
        errors.append('origin_ports must list three ports')
    if params['sample_interval_s'] <= 0:
        errors.append('sample_interval_s must be positive')
    if errors:
        raise ValueError('invalid showcase parameters:\n  ' + '\n  '.join(errors))


def mode_params(params, mode):
    """The effective values for one mode: explicit overrides win."""
    defaults = MODE_DEFAULTS[mode]
    return {
        'mode': mode,
        'route': defaults['route'],
        'rate': params['rate'] or defaults['rate'],
        'tanod_max': params['tanod_max'] or defaults['tanod_max'],
    }


def scheduled_iterations(profile, rate, duration_s, base_rate=0, base_s=0, ramp_s=0, recovery_s=0):
    """Iterations the open-model schedule offers, by integrating its rate.

    k6 rounds each stage, so the count it actually starts (iterations plus
    dropped_iterations) can differ from this by a few.
    """
    if profile == 'constant':
        return rate * duration_s
    return (base_rate * base_s + (base_rate + rate) / 2 * ramp_s + rate * duration_s
            + (rate + base_rate) / 2 * ramp_s + base_rate * recovery_s)


def schedule_seconds(profile, duration_s, base_s=0, ramp_s=0, recovery_s=0):
    if profile == 'constant':
        return duration_s
    return base_s + 2 * ramp_s + duration_s + recovery_s


# --------------------------------------------------------------------------
# Configuration rendering
# --------------------------------------------------------------------------

def render_template(text, values):
    for key, value in values.items():
        text = text.replace(f'@@{key}@@', str(value))
    leftover = sorted(set(re.findall(r'@@([A-Z0-9_]+)@@', text)))
    if leftover:
        raise ValueError(f'unrendered placeholders: {", ".join(leftover)}')
    return text


def render_nginx(params, arm):
    if arm == 'baseline':
        servers = ['next-1:3000', 'next-2:3000', 'next-3:3000']
    elif arm == 'tanod':
        servers = ['tanod:8080']
    else:
        raise ValueError(f'unknown arm {arm!r}')
    return render_template((HERE / 'nginx.conf.tpl').read_text(), {
        'NGINX_WORKERS': params['nginx_workers'],
        'NGINX_LIMIT_KEY': NGINX_LIMIT_KEYS[params['nginx_limit_key']],
        'NGINX_RATE': params['nginx_rate'],
        'NGINX_BURST': params['nginx_burst'],
        'NGINX_PROXY_TIMEOUT': params['nginx_proxy_timeout'],
        'NGINX_CONNECT_TIMEOUT': params['nginx_connect_timeout'],
        'UPSTREAM_SERVERS': '\n'.join(f'    server {server};' for server in servers),
    })


def render_tanod(params, mode):
    effective = mode_params(params, mode)
    flag = lambda value: 'true' if value else 'false'
    return render_template((HERE / 'tanod.yaml.tpl').read_text(), {
        'TANOD_MAX': effective['tanod_max'],
        'TANOD_QUEUE_MAX': params['tanod_queue_max'],
        'TANOD_QUEUE_TIMEOUT': params['tanod_queue_timeout'],
        'TANOD_CACHE': flag(params['tanod_cache']),
        'TANOD_COALESCE': flag(params['tanod_coalesce']),
        'TANOD_CACHE_TTL': params['tanod_cache_ttl'],
    })


# --------------------------------------------------------------------------
# Prometheus text and HTTP helpers
# --------------------------------------------------------------------------

_SAMPLE_RE = re.compile(r'^([a-zA-Z_:][a-zA-Z0-9_:]*)(\{(.*)\})?\s+(\S+)(\s+\d+)?$')
_LABEL_RE = re.compile(r'([a-zA-Z_][a-zA-Z0-9_]*)="((?:[^"\\]|\\.)*)"')


def parse_prometheus(text, prefix=''):
    """[(name, {label: value}, float)] for every sample whose name has prefix."""
    samples = []
    for line in text.splitlines():
        if not line or line.startswith('#'):
            continue
        match = _SAMPLE_RE.match(line.strip())
        if not match or not match.group(1).startswith(prefix):
            continue
        labels = {key: value.encode().decode('unicode_escape')
                  for key, value in _LABEL_RE.findall(match.group(3) or '')}
        try:
            value = float(match.group(4))
        except ValueError:
            continue
        samples.append((match.group(1), labels, value))
    return samples


def metric_sum(samples, name, **match):
    return sum(value for sample, labels, value in samples
               if sample == name and all(labels.get(k) == v for k, v in match.items()))


def metric_by(samples, name, label, **match):
    totals = {}
    for sample, labels, value in samples:
        if sample == name and all(labels.get(k) == v for k, v in match.items()):
            key = labels.get(label, '')
            totals[key] = totals.get(key, 0.0) + value
    return totals


def http_get(url, timeout=2.0):
    with urllib.request.urlopen(url, timeout=timeout) as response:
        return response.read().decode()


def origin_ports(params):
    return [int(port) for port in _csv(params['origin_ports'])]


def read_origin(port, reader, timeout=2.0):
    body = http_get(f'http://127.0.0.1:{port}/api/fixture-stats?reader={reader}', timeout)
    return json.loads(body)


def read_tanod(params, timeout=2.0):
    return parse_prometheus(http_get(f"http://127.0.0.1:{params['tanod_metrics_port']}/metrics", timeout), 'tanod_')


def read_tanod_admission(params, timeout=2.0):
    """In-flight work and queued requests right now, from the admin /status document.

    Not the Prometheus gauges: those are refreshed as requests pass through
    admission, so a sample can read the value the last request left behind.
    Requests queue in whichever limiter refused them first (route, then
    priority tier, then global; with default priorities the tier fills first),
    and wait in exactly one at a time, so the queue is the sum over limiters.
    """
    status = json.loads(http_get(f"http://127.0.0.1:{params['tanod_admin_port']}/status", timeout))
    admission = status['admission']
    limiters = [admission['global'], *admission.get('tiers', []), *admission.get('routes', [])]
    return {'in_flight': admission['global']['in_flight'],
            'queue_depth': sum(limiter['queue_depth'] for limiter in limiters)}


# --------------------------------------------------------------------------
# Container statistics through the Docker Engine API
# --------------------------------------------------------------------------

class _UnixConnection(http.client.HTTPConnection):
    def __init__(self, path, timeout):
        super().__init__('localhost', timeout=timeout)
        self._path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self._path)


def docker_socket():
    host = os.environ.get('DOCKER_HOST', '')
    if host.startswith('unix://'):
        return host[len('unix://'):]
    if host:
        return None
    return '/var/run/docker.sock'


def container_stats(container_id, timeout=3.0):
    """(cumulative CPU ns, memory bytes excluding page cache) for one container."""
    path = docker_socket()
    if not path:
        raise OSError('DOCKER_HOST is not a unix socket')
    connection = _UnixConnection(path, timeout)
    try:
        connection.request('GET', f'/containers/{container_id}/stats?stream=false&one-shot=true')
        response = connection.getresponse()
        if response.status != 200:
            raise OSError(f'docker stats returned {response.status}')
        stats = json.loads(response.read())
    finally:
        connection.close()
    cpu = stats['cpu_stats']['cpu_usage']['total_usage']
    memory = stats.get('memory_stats', {})
    usage = memory.get('usage', 0)
    inner = memory.get('stats', {})
    cache = inner.get('inactive_file', inner.get('total_inactive_file', 0))
    return cpu, max(0, usage - cache)


# --------------------------------------------------------------------------
# Snapshots, sampling and drain
# --------------------------------------------------------------------------

def take_snapshot(params, arm, reset_window):
    """Counters at one instant. `reset_window` restarts the sampler's peak window."""
    snapshot = {'time': time.time(), 'origins': {}, 'tanod': None}
    reader = 'showcase' if reset_window else 'snapshot'
    for index, port in enumerate(origin_ports(params), start=1):
        try:
            snapshot['origins'][f'next-{index}'] = read_origin(port, reader, timeout=10)
        except (OSError, ValueError) as error:
            snapshot['origins'][f'next-{index}'] = {'error': str(error)}
    if arm == 'tanod':
        try:
            snapshot['tanod'] = [[name, labels, value] for name, labels, value in read_tanod(params, 10)]
        except (OSError, ValueError) as error:
            snapshot['tanod'] = {'error': str(error)}
    return snapshot


def run_sampler(params, arm, out, stop, containers, clock=time.time):
    """Poll every source on its own thread until `stop` exists; long-format CSV.

    One slow source (an overloaded origin answers its stats request late) must
    not delay the others, hence a thread each. Every row carries the time its
    value was read, so gaps are visible rather than smoothed over.
    """
    interval = params['sample_interval_s']
    rows, lock = [], threading.Lock()
    stop = Path(stop)

    def record(source, values):
        now = clock()
        with lock:
            rows.extend((f'{now:.3f}', source, metric, value) for metric, value in values)

    def poll(source, read):
        while not stop.exists():
            started = time.monotonic()
            try:
                record(source, read())
            except Exception as error:  # noqa: BLE001 - a failed read is data too
                record(source, [('error', type(error).__name__)])
            time.sleep(max(0.0, interval - (time.monotonic() - started)))

    def origin_reader(port):
        def read():
            stats = read_origin(port, 'showcase', timeout=5.0)
            return [('active', stats['active']), ('window_peak', stats['window_peak']),
                    ('total', stats['total'])]
        return read

    def tanod_reader():
        admission = read_tanod_admission(params)
        return [('in_flight', admission['in_flight']), ('queue_depth', admission['queue_depth'])]

    def container_reader(container_id):
        def read():
            cpu, memory = container_stats(container_id)
            return [('cpu_ns', cpu), ('memory_bytes', memory)]
        return read

    sources = [(f'next-{i}', origin_reader(port)) for i, port in enumerate(origin_ports(params), 1)]
    if arm == 'tanod':
        sources.append(('tanod', tanod_reader))
    sources += [(f'container:{name}', container_reader(cid)) for name, cid in containers.items()]
    threads = [threading.Thread(target=poll, args=source, daemon=True) for source in sources]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    with open(out, 'w', newline='') as stream:
        writer = csv.writer(stream)
        writer.writerow(['time', 'source', 'metric', 'value'])
        writer.writerows(sorted(rows, key=lambda row: float(row[0])))


def wait_for_drain(params, timeout_s):
    """Seconds until every origin reports no render in progress, or None."""
    started = time.monotonic()
    while time.monotonic() - started < timeout_s:
        active = []
        for port in origin_ports(params):
            try:
                active.append(read_origin(port, 'drain', timeout=5)['active'])
            except (OSError, ValueError):
                active.append(None)
        if all(value == 0 for value in active):
            return round(time.monotonic() - started, 2)
        time.sleep(0.5)
    return None


# --------------------------------------------------------------------------
# Analysis of one case
# --------------------------------------------------------------------------

def _count(metrics, name):
    metric = metrics.get(name)
    return int(metric['values'].get('count', 0)) if metric else 0


def _trend(metrics, name):
    metric = metrics.get(name)
    if not metric or not metric['values'].get('count'):
        return {'count': 0, 'p50': None, 'p90': None, 'p95': None, 'p99': None, 'max': None}
    values = metric['values']
    pick = lambda key: round(values[key], 2) if values.get(key) is not None else None
    return {'count': int(values['count']), 'p50': pick('med'), 'p90': pick('p(90)'),
            'p95': pick('p(95)'), 'p99': pick('p(99)'), 'max': pick('max')}


def summarize_k6(summary):
    """Offered vs executed iterations, outcomes and latency from a k6 summary."""
    metrics = summary['metrics']
    config = summary.get('showcase_config', {})
    outcomes = {name: _count(metrics, f'showcase_{name}') for name in OUTCOMES}
    iterations = _count(metrics, 'iterations')
    dropped = _count(metrics, 'dropped_iterations')
    classified = sum(outcomes.values())
    if classified != iterations:
        raise ValueError(f'k6 summary is inconsistent: {iterations} iterations but {classified} classified outcomes')
    profile = config.get('profile', 'constant')
    scheduled = scheduled_iterations(profile, config.get('rate', 0), config.get('duration', 0),
                                     config.get('baseRate', 0), config.get('baseS', 0),
                                     config.get('rampS', 0), config.get('recoveryS', 0))
    seconds = schedule_seconds(profile, config.get('duration', 0), config.get('baseS', 0),
                               config.get('rampS', 0), config.get('recoveryS', 0))
    successful = outcomes['ok_origin'] + outcomes['ok_cache']
    attempted = iterations + dropped
    return {
        'k6_config': config,
        'scheduled_iterations': round(scheduled, 1),
        'schedule_seconds': seconds,
        'iterations': iterations,
        'dropped_iterations': dropped,
        'attempted_iterations': attempted,
        'outcomes': outcomes,
        'successful': successful,
        'success_share_of_attempted': round(successful / attempted, 4) if attempted else None,
        'successful_per_second': round(successful / seconds, 2) if seconds else None,
        'ok_ms': _trend(metrics, 'showcase_ok_ms'),
        'ok_origin_ms': _trend(metrics, 'showcase_ok_origin_ms'),
        'shed_ms': _trend(metrics, 'showcase_shed_ms'),
        'test_run_duration_ms': summary.get('state', {}).get('testRunDurationMs'),
    }


def read_samples(path):
    rows = []
    if not Path(path).exists():
        return rows
    with open(path, newline='') as stream:
        for row in csv.DictReader(stream):
            rows.append(row)
    return rows


def summarize_samples(rows, interval_s):
    """Origin concurrency, Tanod queueing and container resources from samples."""
    origins = {}
    tanod = {'in_flight': [], 'queue_depth': []}
    containers = {}
    errors = {}
    origin_active_by_tick = {}
    for row in rows:
        source, metric, value, at = row['source'], row['metric'], row['value'], float(row['time'])
        if metric == 'error':
            errors[source] = errors.get(source, 0) + 1
            continue
        value = float(value)
        if source.startswith('next-'):
            origins.setdefault(source, {}).setdefault(metric, []).append((at, value))
            if metric == 'active':
                tick = round(at / interval_s)
                origin_active_by_tick.setdefault(tick, {})[source] = value
        elif source == 'tanod':
            tanod.setdefault(metric, []).append(value)
        elif source.startswith('container:'):
            containers.setdefault(source.split(':', 1)[1], {}).setdefault(metric, []).append((at, value))

    per_origin = {}
    for name, metrics in sorted(origins.items()):
        peaks = [value for _, value in metrics.get('window_peak', [])]
        actives = [value for _, value in metrics.get('active', [])]
        per_origin[name] = {
            'peak_concurrent': int(max(peaks)) if peaks else None,
            'mean_active_sampled': round(statistics.fmean(actives), 2) if actives else None,
            'samples': len(actives),
        }
    # Sum across origins only where all three were read in the same tick.
    complete = [sum(ticks.values()) for ticks in origin_active_by_tick.values() if len(ticks) == len(origins)]
    resources = {}
    for name, metrics in sorted(containers.items()):
        cpu = metrics.get('cpu_ns', [])
        memory = [value for _, value in metrics.get('memory_bytes', [])]
        entry = {'cpu_seconds': None, 'mean_cores': None, 'peak_cores': None,
                 'peak_memory_mib': round(max(memory) / 2**20, 1) if memory else None}
        if len(cpu) >= 2:
            (t0, c0), (t1, c1) = cpu[0], cpu[-1]
            entry['cpu_seconds'] = round((c1 - c0) / 1e9, 2)
            entry['mean_cores'] = round((c1 - c0) / 1e9 / (t1 - t0), 3) if t1 > t0 else None
            rates = [(b - a) / 1e9 / (tb - ta) for (ta, a), (tb, b) in zip(cpu, cpu[1:]) if tb > ta]
            entry['peak_cores'] = round(max(rates), 3) if rates else None
        resources[name] = entry
    return {
        'per_origin': per_origin,
        'peak_any_single_origin': max((o['peak_concurrent'] or 0) for o in per_origin.values()) if per_origin else None,
        'peak_total_active_sampled': int(max(complete)) if complete else None,
        'peak_total_window_upper_bound': _window_upper_bound(origins, interval_s),
        'tanod_peak_in_flight_sampled': int(max(tanod['in_flight'])) if tanod['in_flight'] else None,
        'tanod_peak_queue_depth_sampled': int(max(tanod['queue_depth'])) if tanod['queue_depth'] else None,
        'resources': resources,
        'sample_errors': errors,
    }


def _window_upper_bound(origins, interval_s):
    """Max over ticks of the sum of per-origin window peaks.

    Each origin's window peak is exact for its own interval, but the three
    peaks need not coincide, so their sum bounds the true total from above.
    """
    ticks = {}
    for name, metrics in origins.items():
        for at, value in metrics.get('window_peak', []):
            ticks.setdefault(round(at / interval_s), {})[name] = value
    sums = [sum(values.values()) for values in ticks.values() if len(values) == len(origins)]
    return int(max(sums)) if sums else None


def origin_deltas(before, after):
    per_origin = {}
    for name, end in after.get('origins', {}).items():
        start = before.get('origins', {}).get(name, {})
        if 'total' in end and 'total' in start:
            per_origin[name] = int(end['total'] - start['total'])
        else:
            per_origin[name] = None
    known = [value for value in per_origin.values() if value is not None]
    return {'renders': sum(known) if len(known) == len(per_origin) else None, 'renders_per_origin': per_origin}


def tanod_deltas(before, after):
    if not isinstance(before, list) or not isinstance(after, list):
        return None
    start, end = [tuple(s) for s in before], [tuple(s) for s in after]

    def delta_by(name, label):
        a, b = metric_by(start, name, label), metric_by(end, name, label)
        return {key: int(b.get(key, 0) - a.get(key, 0)) for key in sorted(set(a) | set(b))
                if b.get(key, 0) - a.get(key, 0)}

    def delta_sum(name, **match):
        return int(metric_sum(end, name, **match) - metric_sum(start, name, **match))

    responses = {}
    for name, labels, value in end:
        if name == 'tanod_responses_total':
            key = f"{labels.get('code')}/{labels.get('source')}"
            responses[key] = responses.get(key, 0) + value
    for name, labels, value in start:
        if name == 'tanod_responses_total':
            key = f"{labels.get('code')}/{labels.get('source')}"
            responses[key] = responses.get(key, 0) - value
    buckets = {}
    for snapshot, sign in ((end, 1), (start, -1)):
        for name, labels, value in snapshot:
            # Labelled by route; summed into one distribution for the run.
            if name == 'tanod_queue_wait_seconds_bucket':
                buckets[labels['le']] = buckets.get(labels['le'], 0) + sign * value
    cumulative = sorted((math.inf if le == '+Inf' else float(le), int(count)) for le, count in buckets.items())
    limit = metric_by(end, 'tanod_concurrency_limit', 'limiter').get('global')
    return {
        'configured_ceiling': int(limit) if limit is not None else None,
        'origin_requests': delta_sum('tanod_origin_requests_total'),
        'cache': delta_by('tanod_cache_total', 'status'),
        'admission': delta_by('tanod_admission_total', 'decision'),
        'responses': {key: int(value) for key, value in sorted(responses.items()) if value},
        'queue_waits': int(sum(v for le, v in buckets.items() if le == '+Inf')),
        # Bucket bounds, not interpolated percentiles: interpolation inside a
        # wide bucket invents values (a 1s queue timeout can "estimate" 2s).
        'queue_wait_p95_at_most_s': bucket_bound(0.95, cumulative),
        'queue_wait_p99_at_most_s': bucket_bound(0.99, cumulative),
        'queue_wait_buckets': {('+Inf' if math.isinf(le) else f'{le:g}'): count for le, count in cumulative},
    }


def bucket_bound(q, cumulative):
    """The upper bound of the histogram bucket holding quantile q, or None."""
    if not cumulative or cumulative[-1][1] <= 0:
        return None
    rank = q * cumulative[-1][1]
    for bound, count in cumulative:
        if count >= rank:
            return None if math.isinf(bound) else bound
    return None


_LOG_RE = re.compile(r'(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z)\s+(\{.*\})\s*$')


def _parse_timestamp(text):
    whole, _, fraction = text.rstrip('Z').partition('.')
    seconds = datetime.datetime.strptime(whole, '%Y-%m-%dT%H:%M:%S').replace(tzinfo=datetime.timezone.utc).timestamp()
    return seconds + (float('0.' + fraction) if fraction else 0.0)


def origin_log_events(lines):
    """(time, active) for every render_start/render_end in one origin's log.

    Each event carries the origin's own count of renders in progress right
    after it, so the sequence is the origin's concurrency, exactly, at every
    change; the time is Docker's receive timestamp for the line.
    """
    events = []
    for line in lines:
        match = _LOG_RE.search(line)
        if not match or '"render_' not in match.group(2):
            continue
        try:
            record = json.loads(match.group(2))
        except ValueError:
            continue
        if record.get('event') in ('render_start', 'render_end') and 'active' in record:
            events.append((_parse_timestamp(match.group(1)), int(record['active'])))
    events.sort(key=lambda event: event[0])
    return events


def origin_concurrency_from_logs(per_origin_events, start, end, bin_s=0.1):
    """Exact total renders in progress across origins within [start, end].

    Returns per-origin peaks, the total peak, and the total's maximum per bin
    (for plotting), or None when no origin produced a usable log.
    """
    if not per_origin_events or not any(per_origin_events.values()):
        return None
    current = {}
    for name, events in per_origin_events.items():
        before = [active for at, active in events if at < start]
        current[name] = before[-1] if before else 0
    merged = sorted((at, name, active) for name, events in per_origin_events.items()
                    for at, active in events if start <= at <= end)
    peaks = dict(current)
    initial_total = total_peak = sum(current.values())
    bins = {}
    for at, name, active in merged:
        current[name] = active
        peaks[name] = max(peaks[name], active)
        total = sum(current.values())
        total_peak = max(total_peak, total)
        index = int((at - start) / bin_s)
        bins[index] = max(bins.get(index, 0), total)
    series = []
    last = initial_total
    for index in range(0, int((end - start) / bin_s) + 1):
        if index in bins:
            last = bins[index]
        series.append((round(index * bin_s, 3), last))
    return {'peak_per_origin': {name: int(value) for name, value in sorted(peaks.items())},
            'peak_total': int(total_peak), 'events': len(merged), 'series': series}


def read_origin_logs(case_dir):
    events = {}
    for index in (1, 2, 3):
        path = Path(case_dir) / 'logs' / f'next-{index}.log.gz'
        if path.exists():
            with gzip.open(path, 'rt', errors='replace') as stream:
                events[f'next-{index}'] = origin_log_events(stream)
    return events


_EDGE_ERROR_RE = re.compile(r'(no live upstreams|upstream timed out \(\d+: [^)]*\) while [a-z ]+?(?= upstream|,)'
                            r'|connect\(\) failed \(\d+: [^)]*\)|upstream prematurely closed connection'
                            r'|recv\(\) failed \(\d+: [^)]*\))')


def edge_errors(case_dir):
    """NGINX's own reasons for failed upstream requests, counted from its error log."""
    path = Path(case_dir) / 'logs' / 'edge.log.gz'
    if not path.exists():
        return None
    counts = {}
    with gzip.open(path, 'rt', errors='replace') as stream:
        for line in stream:
            if '[error]' not in line and '[warn]' not in line:
                continue
            match = _EDGE_ERROR_RE.search(line)
            if match:
                counts[match.group(1)] = counts.get(match.group(1), 0) + 1
    return dict(sorted(counts.items(), key=lambda item: -item[1]))


def container_checks(containers, started):
    """Isolation evidence from `docker inspect`: fresh containers, right arm."""
    services = {c.get('Config', {}).get('Labels', {}).get('com.docker.compose.service', c.get('Name', '')): c
                for c in containers}
    created = [_parse_timestamp(c['Created']) for c in containers if c.get('Created')]
    return {
        'services': sorted(services),
        'created_after_case_start': bool(created) and started is not None and min(created) >= started - 1,
        'oom_killed': sorted(s for s, c in services.items() if c.get('State', {}).get('OOMKilled')),
        'not_running': sorted(s for s, c in services.items() if c.get('State', {}).get('Status') != 'running'),
        'restarts': {s: c['RestartCount'] for s, c in services.items() if c.get('RestartCount')},
    }


def read_timeline(path, start=None, outcomes=OUTCOMES):
    """Per-second outcome counts and successful-response latency from k6's CSV.

    `start` (epoch seconds) anchors t=0 at the measured phase's start, so two
    arms line up even when one answers its first request seconds later.
    """
    if not Path(path).exists():
        return []
    seconds = {}
    opener = gzip.open if str(path).endswith('.gz') else open
    with opener(path, 'rt', newline='') as stream:
        for row in csv.DictReader(stream):
            name = row['metric_name']
            if not name.startswith('showcase_'):
                continue
            second = int(row['timestamp'])
            bucket = seconds.setdefault(second, {'ok_ms': [], **{o: 0 for o in outcomes}})
            short = name[len('showcase_'):]
            if short in outcomes:
                bucket[short] += int(float(row['metric_value']))
            elif short == 'ok_ms':
                bucket['ok_ms'].append(float(row['metric_value']))
    if not seconds:
        return []
    start = int(start) if start is not None else min(seconds)
    timeline = []
    for second in range(start, max(seconds) + 1):
        bucket = seconds.get(second, {'ok_ms': [], **{o: 0 for o in outcomes}})
        latencies = sorted(bucket.pop('ok_ms'))
        timeline.append({'t': second - start, **bucket,
                         'ok_p50_ms': _percentile(latencies, 0.50),
                         'ok_p95_ms': _percentile(latencies, 0.95)})
    return timeline


def _percentile(values, q):
    if not values:
        return None
    index = min(len(values) - 1, max(0, math.ceil(q * len(values)) - 1))
    return round(values[index], 1)


def case_meta(params, mode, workload, arm, trial, run_id, started=None):
    effective = mode_params(params, mode)
    return {
        'started_epoch': started,
        'mode': mode, 'workload': workload, 'arm': arm, 'trial': trial, 'run_id': run_id,
        'route': effective['route'], 'rate': effective['rate'], 'tanod_max': effective['tanod_max'],
        'profile': params['profile'], 'duration_s': params['duration_s'],
        'mechanism_only': mode == 'delay',
    }


def analyze_case(case_dir, params):
    case_dir = Path(case_dir)
    meta = json.loads((case_dir / 'case-meta.json').read_text())
    summary = json.loads((case_dir / 'k6-summary.json').read_text())
    before = json.loads((case_dir / 'snapshot-before.json').read_text())
    after = json.loads((case_dir / 'snapshot-after.json').read_text())
    drain = json.loads((case_dir / 'drain.json').read_text()) if (case_dir / 'drain.json').exists() else {}
    warmup_path = case_dir / 'k6-warmup-summary.json'
    warmup = summarize_k6(json.loads(warmup_path.read_text())) if warmup_path.exists() else None

    k6 = summarize_k6(summary)
    samples = summarize_samples(read_samples(case_dir / 'samples.csv'), params['sample_interval_s'])
    origin = origin_deltas(before, after)
    tanod = tanod_deltas(before.get('tanod'), after.get('tanod')) if meta['arm'] == 'tanod' else None
    ceiling = meta['tanod_max']

    logged = origin_concurrency_from_logs(read_origin_logs(case_dir), before['time'], after['time'])
    if logged:
        with open(case_dir / 'origin-concurrency.csv', 'w', newline='') as stream:
            writer = csv.writer(stream)
            writer.writerow(['t', 'renders_in_progress'])
            writer.writerows(logged['series'])
    containers_path = case_dir / 'containers.json'
    stack = container_checks(json.loads(containers_path.read_text()), meta.get('started_epoch')) \
        if containers_path.exists() else None

    timeline = read_timeline(case_dir / 'k6-requests.csv.gz', before['time'])
    nginx_errors = edge_errors(case_dir)
    if timeline:
        with open(case_dir / 'timeline.csv', 'w', newline='') as stream:
            writer = csv.DictWriter(stream, fieldnames=list(timeline[0]))
            writer.writeheader()
            writer.writerows(timeline)

    outcomes = k6['outcomes']
    checks = {}
    notes = []
    # The exact total from the origins' logs when available; the sampled sum
    # (a lower bound) otherwise.
    origin_peak = logged['peak_total'] if logged else samples['peak_total_active_sampled']
    if stack is not None:
        expected = {'next-1', 'next-2', 'next-3', 'edge'} | ({'tanod'} if meta['arm'] == 'tanod' else set())
        checks['isolated_fresh_stack'] = set(stack['services']) == expected and stack['created_after_case_start']
        if not checks['isolated_fresh_stack']:
            notes.append(f"stack was not the expected fresh set: {stack['services']}, "
                         f"created after case start: {stack['created_after_case_start']}")
        if stack['oom_killed']:
            notes.append('OOM-killed during the case: ' + ', '.join(stack['oom_killed']))
        if stack['not_running']:
            notes.append('not running at the end of the case: ' + ', '.join(stack['not_running']))
    if meta['workload'] == 'unique':
        hits = outcomes['ok_cache'] + (sum((tanod or {}).get('cache', {}).get(s, 0) for s in ('hit', 'stale')))
        checks['no_cache_reuse'] = hits == 0
        if hits:
            notes.append(f'{hits} cache hits in a unique-URL workload: admission evidence from this case is invalid')
    if meta['arm'] == 'tanod':
        observed = [samples['peak_any_single_origin'], origin_peak, samples['tanod_peak_in_flight_sampled']]
        observed = [value for value in observed if value is not None]
        checks['ceiling_held'] = bool(observed) and max(observed) <= ceiling
    if meta['arm'] == 'baseline' and meta['workload'] == 'unique':
        peak = origin_peak or 0
        checks['load_exceeded_ceiling'] = peak > ceiling
        if peak <= ceiling:
            notes.append(f'baseline origin peak {peak} never exceeded the Tanod ceiling {ceiling}: '
                         'the offered load was too low to show admission control')
    if k6['dropped_iterations']:
        notes.append(f"k6 dropped {k6['dropped_iterations']} scheduled iterations (all {k6['k6_config'].get('maxVUs')} VUs busy): "
                     'that load was offered but never sent')
    if outcomes['rate_limited']:
        notes.append(f"NGINX rate limiting answered {outcomes['rate_limited']} requests with 429")
    if outcomes['upstream_error'] or outcomes['transport_error']:
        reasons = ', '.join(f'{reason}: {count}' for reason, count in (nginx_errors or {}).items())
        notes.append(f"{outcomes['upstream_error']} upstream 5xx and {outcomes['transport_error']} transport errors"
                     + (f' (NGINX error log: {reasons})' if reasons else ''))
    if samples['sample_errors']:
        notes.append('sampler read failures: ' + ', '.join(f'{k}={v}' for k, v in sorted(samples['sample_errors'].items())))
    if drain.get('drain_seconds') is None and drain:
        notes.append('origins still had renders in progress when the drain timeout expired')

    renders = origin['renders']
    result = {
        **meta,
        **{key: value for key, value in k6.items() if key != 'k6_config'},
        'warmup': None if warmup is None else {
            'iterations': warmup['iterations'], 'ok_origin_p50_ms': warmup['ok_origin_ms']['p50'],
            'ok_origin_p95_ms': warmup['ok_origin_ms']['p95'],
        },
        'origin': {
            **origin,
            'peak_total_concurrent': origin_peak,
            'peak_total_source': 'origin logs (exact)' if logged else 'sampled (lower bound)',
            'peak_per_origin_from_logs': logged['peak_per_origin'] if logged else None,
            **{key: samples[key] for key in ('per_origin', 'peak_any_single_origin',
                                             'peak_total_active_sampled', 'peak_total_window_upper_bound')},
            'renders_per_successful_response': round(renders / k6['successful'], 3)
            if renders is not None and k6['successful'] else None,
            # Only meaningful when every 200 needed its own render: in the
            # unique workload, a render that did not become a 200 was work the
            # origin did for a client that had already given up or failed.
            'renders_not_delivered_as_200': max(0, renders - outcomes['ok_origin'])
            if renders is not None and meta['workload'] == 'unique' else None,
            'drain_seconds_after_load': drain.get('drain_seconds'),
        },
        'tanod': None if tanod is None else {
            **tanod,
            'peak_in_flight_sampled': samples['tanod_peak_in_flight_sampled'],
            'peak_queue_depth_sampled': samples['tanod_peak_queue_depth_sampled'],
        },
        'resources': samples['resources'],
        'containers': stack,
        'nginx_errors': nginx_errors,
        'checks': checks,
        'notes': notes,
    }
    (case_dir / 'case.json').write_text(json.dumps(result, indent=2) + '\n')
    return result


# --------------------------------------------------------------------------
# Aggregation across cases
# --------------------------------------------------------------------------

CSV_COLUMNS = [
    'mode', 'workload', 'arm', 'trial', 'profile', 'rate', 'duration_s', 'tanod_max',
    'scheduled_iterations', 'iterations', 'dropped_iterations', 'attempted_iterations',
    *[f'outcome_{name}' for name in OUTCOMES],
    'successful', 'success_share_of_attempted', 'successful_per_second',
    'ok_p50_ms', 'ok_p95_ms', 'ok_p99_ms', 'ok_origin_p50_ms', 'ok_origin_p95_ms', 'ok_origin_p99_ms',
    'shed_p95_ms', 'origin_renders', 'renders_per_successful_response', 'renders_not_delivered_as_200',
    'origin_peak_total', 'origin_peak_any_single', 'origin_peak_total_sampled', 'origin_peak_total_upper_bound',
    'origin_drain_seconds', 'tanod_origin_requests', 'tanod_cache_hit', 'tanod_cache_miss',
    'tanod_cache_stale', 'tanod_shed_queue_full', 'tanod_shed_queue_timeout', 'tanod_admitted',
    'tanod_queue_wait_p95_at_most_s', 'tanod_peak_in_flight_sampled', 'tanod_peak_queue_depth_sampled',
    'origin_cpu_seconds', 'tanod_cpu_seconds', 'edge_cpu_seconds', 'origin_peak_memory_mib',
    'checks_passed', 'checks_failed',
]


def flatten(case):
    tanod = case.get('tanod') or {}
    resources = case.get('resources') or {}
    origin_cpu = [resources.get(f'next-{i}', {}).get('cpu_seconds') for i in (1, 2, 3)]
    origin_memory = [resources.get(f'next-{i}', {}).get('peak_memory_mib') for i in (1, 2, 3)]
    checks = case.get('checks', {})
    row = {
        'mode': case['mode'], 'workload': case['workload'], 'arm': case['arm'], 'trial': case['trial'],
        'profile': case['profile'], 'rate': case['rate'], 'duration_s': case['duration_s'],
        'tanod_max': case['tanod_max'],
        'scheduled_iterations': case['scheduled_iterations'], 'iterations': case['iterations'],
        'dropped_iterations': case['dropped_iterations'], 'attempted_iterations': case['attempted_iterations'],
        **{f'outcome_{name}': case['outcomes'][name] for name in OUTCOMES},
        'successful': case['successful'], 'success_share_of_attempted': case['success_share_of_attempted'],
        'successful_per_second': case['successful_per_second'],
        'ok_p50_ms': case['ok_ms']['p50'], 'ok_p95_ms': case['ok_ms']['p95'], 'ok_p99_ms': case['ok_ms']['p99'],
        'ok_origin_p50_ms': case['ok_origin_ms']['p50'], 'ok_origin_p95_ms': case['ok_origin_ms']['p95'],
        'ok_origin_p99_ms': case['ok_origin_ms']['p99'], 'shed_p95_ms': case['shed_ms']['p95'],
        'origin_renders': case['origin']['renders'],
        'renders_per_successful_response': case['origin']['renders_per_successful_response'],
        'renders_not_delivered_as_200': case['origin']['renders_not_delivered_as_200'],
        'origin_peak_total': case['origin']['peak_total_concurrent'],
        'origin_peak_any_single': case['origin']['peak_any_single_origin'],
        'origin_peak_total_sampled': case['origin']['peak_total_active_sampled'],
        'origin_peak_total_upper_bound': case['origin']['peak_total_window_upper_bound'],
        'origin_drain_seconds': case['origin']['drain_seconds_after_load'],
        'tanod_origin_requests': tanod.get('origin_requests'),
        'tanod_cache_hit': (tanod.get('cache') or {}).get('hit'),
        'tanod_cache_miss': (tanod.get('cache') or {}).get('miss'),
        'tanod_cache_stale': (tanod.get('cache') or {}).get('stale'),
        'tanod_shed_queue_full': (tanod.get('admission') or {}).get('shed_queue_full'),
        'tanod_shed_queue_timeout': (tanod.get('admission') or {}).get('shed_queue_timeout'),
        'tanod_admitted': (tanod.get('admission') or {}).get('admitted'),
        'tanod_queue_wait_p95_at_most_s': tanod.get('queue_wait_p95_at_most_s'),
        'tanod_peak_in_flight_sampled': tanod.get('peak_in_flight_sampled'),
        'tanod_peak_queue_depth_sampled': tanod.get('peak_queue_depth_sampled'),
        'origin_cpu_seconds': round(sum(origin_cpu), 2) if all(v is not None for v in origin_cpu) else None,
        'tanod_cpu_seconds': resources.get('tanod', {}).get('cpu_seconds'),
        'edge_cpu_seconds': resources.get('edge', {}).get('cpu_seconds'),
        'origin_peak_memory_mib': max(origin_memory) if all(v is not None for v in origin_memory) else None,
        'checks_passed': ';'.join(sorted(k for k, v in checks.items() if v)),
        'checks_failed': ';'.join(sorted(k for k, v in checks.items() if not v)),
    }
    assert list(row) == CSV_COLUMNS, 'CSV_COLUMNS and flatten() disagree'
    return row


def aggregate(run_dir):
    run_dir = Path(run_dir)
    params = json.loads((run_dir / 'params.json').read_text())
    cases = []
    for case_json in sorted(run_dir.glob('cases/*/case.json')):
        cases.append(json.loads(case_json.read_text()))
    if not cases:
        raise SystemExit(f'no analysed cases under {run_dir}/cases')
    cases.sort(key=lambda c: (MODES.index(c['mode']), WORKLOADS.index(c['workload']), c['trial'], ARMS.index(c['arm'])))
    metadata_path = run_dir / 'metadata.json'
    results = {
        'schema': 'tanod-showcase-results/v1',
        'metadata': json.loads(metadata_path.read_text()) if metadata_path.exists() else None,
        'params': params,
        'cases': cases,
        'comparisons': compare(cases),
    }
    (run_dir / 'results.json').write_text(json.dumps(results, indent=2) + '\n')
    with open(run_dir / 'results.csv', 'w', newline='') as stream:
        writer = csv.DictWriter(stream, fieldnames=CSV_COLUMNS)
        writer.writeheader()
        writer.writerows(flatten(case) for case in cases)
    (run_dir / 'summary.md').write_text(summary_markdown(results))
    return results


def _median(values):
    values = [v for v in values if v is not None]
    return statistics.median(values) if values else None


def compare(cases):
    """Per mode and workload: median across trials of each arm's key figures."""
    groups = {}
    for case in cases:
        groups.setdefault((case['mode'], case['workload']), {}).setdefault(case['arm'], []).append(case)
    comparisons = []
    for (mode, workload), arms in groups.items():
        entry = {'mode': mode, 'workload': workload}
        for arm, runs in arms.items():
            entry[arm] = {
                'trials': len(runs),
                'successful_per_second': _median([r['successful_per_second'] for r in runs]),
                'success_share_of_attempted': _median([r['success_share_of_attempted'] for r in runs]),
                'ok_p95_ms': _median([r['ok_ms']['p95'] for r in runs]),
                'ok_p99_ms': _median([r['ok_ms']['p99'] for r in runs]),
                'shed': _median([r['outcomes']['shed'] for r in runs]),
                'timeouts': _median([r['outcomes']['timeout'] for r in runs]),
                'dropped_iterations': _median([r['dropped_iterations'] for r in runs]),
                'origin_peak_total': _median([r['origin']['peak_total_concurrent'] for r in runs]),
                'origin_renders': _median([r['origin']['renders'] for r in runs]),
                'all_checks_passed': all(all(r['checks'].values()) for r in runs),
            }
        comparisons.append(entry)
    return comparisons


def _fmt(value, suffix=''):
    if value is None:
        return '—'
    if isinstance(value, float):
        value = f'{value:,.1f}' if abs(value) < 1000 else f'{value:,.0f}'
    elif isinstance(value, int):
        value = f'{value:,}'
    return f'{value}{suffix}'


def summary_markdown(results):
    """A plain table of what was measured. Generated; edit the code, not the file."""
    meta = results.get('metadata') or {}
    host = meta.get('host', {})
    lines = [
        '# Tanod overload showcase — measured results',
        '',
        f"Generated from `results.json`. Run `{meta.get('run_id', '?')}` at commit "
        f"`{(meta.get('git') or {}).get('commit', '?')[:12]}` on {host.get('cpu_model', 'unknown CPU')} "
        f"({host.get('logical_cpus', '?')} logical CPUs).",
        '',
        'Mode `delay` is a mechanism test (the origin sleeps; no CPU is saturated). '
        'Mode `cpu` does a fixed amount of CPU work per request. Successful latency covers HTTP 200 only.',
        '',
        '| Mode | Workload | Arm | Trial | Rate (req/s) | Attempted | Dropped | 200 origin | 200 cache | 503 shed | 429 | 5xx | Timeout | p50 ms | p95 ms | p99 ms | Origin peak | Origin renders | Checks |',
        '|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|',
    ]
    for case in results['cases']:
        o = case['outcomes']
        failed = [k for k, v in case['checks'].items() if not v]
        lines.append('| ' + ' | '.join([
            case['mode'], case['workload'], case['arm'], str(case['trial']), _fmt(case['rate']),
            _fmt(case['attempted_iterations']), _fmt(case['dropped_iterations']),
            _fmt(o['ok_origin']), _fmt(o['ok_cache']), _fmt(o['shed']), _fmt(o['rate_limited']),
            _fmt(o['upstream_error'] + o['transport_error'] + o['other_status']), _fmt(o['timeout']),
            _fmt(case['ok_ms']['p50']), _fmt(case['ok_ms']['p95']), _fmt(case['ok_ms']['p99']),
            _fmt(case['origin']['peak_total_concurrent']), _fmt(case['origin']['renders']),
            'FAILED: ' + ', '.join(failed) if failed else ('pass' if case['checks'] else '—'),
        ]) + ' |')
    notes = [(case, note) for case in results['cases'] for note in case['notes']]
    if notes:
        lines += ['', '## Notes recorded during the run', '']
        lines += [f"- {c['mode']}/{c['workload']}/{c['arm']}/t{c['trial']}: {note}" for c, note in notes]
    lines += ['', 'Origin peak is the highest total of renders in progress across the three origins, '
              "from the origins' own render log (exact) or, without logs, their sampled counters (a lower bound).", '']
    return '\n'.join(lines)


# --------------------------------------------------------------------------
# Environment metadata
# --------------------------------------------------------------------------

def _run(command, timeout=60):
    try:
        result = subprocess.run(command, capture_output=True, text=True, timeout=timeout)
        return result.stdout.strip() if result.returncode == 0 else None
    except (OSError, subprocess.TimeoutExpired):
        return None


def _read(path):
    try:
        return Path(path).read_text().strip()
    except OSError:
        return None


def collect_metadata(params, images, run_id, k6_command):
    cpuinfo = _read('/proc/cpuinfo') or ''
    model = next((line.split(':', 1)[1].strip() for line in cpuinfo.splitlines()
                  if line.startswith('model name')), platform.processor() or None)
    meminfo = _read('/proc/meminfo') or ''
    mem_kib = next((int(line.split()[1]) for line in meminfo.splitlines() if line.startswith('MemTotal')), None)
    image_info = {}
    for name, ref in images.items():
        image_info[name] = {'ref': ref, 'id': _run(['docker', 'image', 'inspect', '--format', '{{.Id}}', ref])}
    next_image = images.get('next')
    versions = {}
    if next_image:
        versions['next'] = _run(['docker', 'run', '--rm', '--entrypoint', 'node', next_image, '-p',
                                 "require('next/package.json').version"])
        versions['node'] = _run(['docker', 'run', '--rm', '--entrypoint', 'node', next_image, '--version'])
    if images.get('nginx'):
        nginx = subprocess.run(['docker', 'run', '--rm', images['nginx'], 'nginx', '-v'],
                               capture_output=True, text=True)
        versions['nginx'] = (nginx.stderr or nginx.stdout).strip() or None
    if images.get('tanod'):
        versions['tanod'] = _run(['docker', 'run', '--rm', images['tanod'], 'version'])
    versions['k6'] = _run(k6_command + ['version'])
    versions['docker'] = _run(['docker', 'version', '--format', '{{.Server.Version}}'])
    versions['python'] = platform.python_version()
    return {
        'run_id': run_id,
        'created_utc': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
        'git': {
            'commit': _run(['git', '-C', str(REPO), 'rev-parse', 'HEAD']),
            'branch': _run(['git', '-C', str(REPO), 'rev-parse', '--abbrev-ref', 'HEAD']),
            'dirty_files': len((_run(['git', '-C', str(REPO), 'status', '--porcelain']) or '').splitlines()),
        },
        'host': {
            'cpu_model': model,
            'logical_cpus': os.cpu_count(),
            'memory_gib': round(mem_kib / 2**20, 1) if mem_kib else None,
            'kernel': platform.release(),
            'os': _read('/etc/os-release') and next((line.split('=', 1)[1].strip('"') for line in
                                                     _read('/etc/os-release').splitlines()
                                                     if line.startswith('PRETTY_NAME=')), None),
            'cpu_governor': _read('/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor'),
            'load_average_at_start': list(os.getloadavg()) if hasattr(os, 'getloadavg') else None,
        },
        'images': image_info,
        'versions': versions,
        'resources': {
            'origin_cpus': params['origin_cpus'], 'origin_memory': params['origin_memory'],
            'tanod_cpus': params['tanod_cpus'], 'edge_cpus': params['edge_cpus'],
            'note': 'Docker CPU quotas (cpus), not pinned cores; k6 and the sampler run on the host unconstrained.',
        },
        'observability_stack_running': params['observe'],
        'k6_command': k6_command,
    }


# --------------------------------------------------------------------------
# Command line
# --------------------------------------------------------------------------

def _pairs(values):
    pairs = {}
    for value in values or []:
        name, _, ref = value.partition('=')
        if not name or not ref:
            raise SystemExit(f'expected NAME=VALUE, got {value!r}')
        pairs[name] = ref
    return pairs


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest='command', required=True)
    p = sub.add_parser('params'); p.add_argument('--out', required=True)
    p = sub.add_parser('get'); p.add_argument('--params', required=True); p.add_argument('key')
    p.add_argument('--mode')
    p = sub.add_parser('render'); p.add_argument('--params', required=True); p.add_argument('--mode', required=True)
    p.add_argument('--arm', required=True); p.add_argument('--out', required=True)
    p = sub.add_parser('snapshot'); p.add_argument('--params', required=True); p.add_argument('--arm', required=True)
    p.add_argument('--out', required=True); p.add_argument('--reset-window', action='store_true')
    p = sub.add_parser('sample'); p.add_argument('--params', required=True); p.add_argument('--arm', required=True)
    p.add_argument('--out', required=True); p.add_argument('--stop', required=True)
    p.add_argument('--container', action='append')
    p = sub.add_parser('drain'); p.add_argument('--params', required=True); p.add_argument('--out', required=True)
    p = sub.add_parser('case-meta'); p.add_argument('--params', required=True); p.add_argument('--out', required=True)
    for name in ('--mode', '--workload', '--arm', '--trial', '--run-id'):
        p.add_argument(name, required=True)
    p = sub.add_parser('case'); p.add_argument('--params', required=True); p.add_argument('--dir', required=True)
    p = sub.add_parser('aggregate'); p.add_argument('--run', required=True)
    p = sub.add_parser('metadata'); p.add_argument('--params', required=True); p.add_argument('--out', required=True)
    p.add_argument('--run-id', required=True); p.add_argument('--image', action='append')
    p.add_argument('--k6', nargs='+', required=True)
    args = parser.parse_args(argv)

    load = lambda: json.loads(Path(args.params).read_text())
    try:
        if args.command == 'params':
            Path(args.out).write_text(json.dumps(load_params(), indent=2) + '\n')
        elif args.command == 'get':
            params = load()
            value = mode_params(params, args.mode)[args.key] if args.mode and args.key in ('route', 'rate', 'tanod_max') \
                else params[args.key]
            print(','.join(_csv(value)) if isinstance(value, str) and ',' in value else
                  (str(value).lower() if isinstance(value, bool) else value))
        elif args.command == 'render':
            params, out = load(), Path(args.out)
            out.mkdir(parents=True, exist_ok=True)
            (out / 'nginx.conf').write_text(render_nginx(params, args.arm))
            (out / 'tanod.yaml').write_text(render_tanod(params, args.mode))
        elif args.command == 'snapshot':
            Path(args.out).write_text(json.dumps(take_snapshot(load(), args.arm, args.reset_window), indent=2) + '\n')
        elif args.command == 'sample':
            run_sampler(load(), args.arm, args.out, args.stop, _pairs(args.container))
        elif args.command == 'drain':
            params = load()
            seconds = wait_for_drain(params, params['drain_timeout_s'])
            Path(args.out).write_text(json.dumps({'drain_seconds': seconds,
                                                  'timeout_s': params['drain_timeout_s']}) + '\n')
        elif args.command == 'case-meta':
            Path(args.out).write_text(json.dumps(case_meta(load(), args.mode, args.workload, args.arm,
                                                           int(args.trial), args.run_id, time.time()),
                                                 indent=2) + '\n')
        elif args.command == 'case':
            result = analyze_case(args.dir, load())
            failed = [k for k, v in result['checks'].items() if not v]
            print(f"  analysed: successful={result['successful']} shed={result['outcomes']['shed']} "
                  f"timeouts={result['outcomes']['timeout']} origin_peak={result['origin']['peak_total_concurrent']}"
                  + (f" FAILED CHECKS: {', '.join(failed)}" if failed else ''))
        elif args.command == 'aggregate':
            results = aggregate(args.run)
            print(f"  {len(results['cases'])} cases -> results.json, results.csv, summary.md")
        elif args.command == 'metadata':
            meta = collect_metadata(load(), _pairs(args.image), args.run_id, args.k6)
            Path(args.out).write_text(json.dumps(meta, indent=2) + '\n')
    except ValueError as error:
        print(f'showcase.py: {error}', file=sys.stderr)
        return 2
    return 0


if __name__ == '__main__':
    sys.exit(main())
