#!/usr/bin/env python3
"""What a flood of distinct query strings costs (cache review finding C7).

With no `cache.query` policy every parameter is part of the key, so a client
that appends `?x=<unique>` mints a new key, a new fill and a new render per
request. This measures whether the existing bounds contain that: the cache's
byte budget, the origin ceiling and queue, and eviction. It does not assert a
throughput number.

Phases, each against a hot set of 100 keys that should stay cached:

  hot           the hot set alone (the reference)
  flood         half the requests carry a unique parameter
  flood-long    the unique parameter has a 2 KiB name and a 2 KiB value
  flood-purge   flood, with a purge of the flooded path every 500 ms
  bounded       the same flood on a route with `query: {mode: include}`

Samples every 250 ms: Tanod's RSS and CPU, `tanod_cache_bytes`, entries,
pinned (in-progress fill) bytes, evictions, origin in-flight units and the
origin's own render count. Asserts only the bounds Tanod promises: cache bytes
within budget, pinned bytes within budget plus one body of headroom, origin
render concurrency within the ceiling, and nothing held once traffic stops.

Usage: bench/query-cardinality.py [--policy clock|tinyufo] [--seconds N]
       [--json OUT]
"""
import argparse
import json
import os
import re
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BUDGET = 16 * 1024 * 1024
BODY = 16384
CEILING = 8


def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


def get(url, timeout=5, data=None, headers=None):
    req = urllib.request.Request(url, data=data, headers=headers or {},
                                 method='POST' if data is not None else 'GET')
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.read().decode()


def metric(text, name, labels=None):
    pattern = '^' + re.escape(name) + (r'\{' + labels + r'\}' if labels else '') + r' (\S+)$'
    match = re.search(pattern, text, re.MULTILINE)
    return float(match[1]) if match else 0.0


def proc_status(pid):
    fields = {}
    for line in Path(f'/proc/{pid}/status').read_text().splitlines():
        key, _, value = line.partition(':')
        fields[key] = value.strip()
    return int(fields['VmRSS'].split()[0]), int(fields['VmHWM'].split()[0])


def cpu_seconds(pid):
    parts = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()
    return (int(parts[11]) + int(parts[12])) / os.sysconf('SC_CLK_TCK')


class Sampler(threading.Thread):
    def __init__(self, pid, metrics, admin, origin):
        super().__init__(daemon=True)
        self.pid, self.metrics, self.admin, self.origin = pid, metrics, admin, origin
        self.rows, self.stop = [], threading.Event()

    def sample(self):
        text = get(f'http://127.0.0.1:{self.metrics}/metrics')
        status = get(f'http://127.0.0.1:{self.admin}/status')
        stats = json.loads(get(f'http://127.0.0.1:{self.origin}/__stats'))
        rss, hwm = proc_status(self.pid)
        return {
            't': time.monotonic(), 'rss_kb': rss, 'hwm_kb': hwm, 'cpu_s': cpu_seconds(self.pid),
            'cache_bytes': metric(text, 'tanod_cache_bytes'),
            'entries': metric(text, 'tanod_cache_entries'),
            'evicted': metric(text, 'tanod_cache_evicted_total'),
            'in_flight': metric(text, 'tanod_origin_in_flight', 'limiter="global"'),
            'pinned': int(re.search(r'"pinned_bytes":(\d+)', status)[1]),
            'origin_total': stats['total'], 'origin_peak': stats['peak'],
            'origin_in_flight': stats['in_flight'],
        }

    def run(self):
        while not self.stop.is_set():
            try:
                self.rows.append(self.sample())
            except (OSError, ValueError, KeyError, TypeError):
                pass
            self.stop.wait(.25)


def config(folder, listen, metrics, admin, origin, policy):
    return f'''version: 1
server:
  listen: "127.0.0.1:{listen}"
  threads: 2
  graceful: {{drain_period: 0s, shutdown_timeout: 5s, pid_file: "{folder}/tanod.pid",
             upgrade_socket: "{folder}/upgrade.sock"}}
origin:
  upstreams: ["127.0.0.1:{origin}"]
  concurrency:
    max: {CEILING}
    queue: {{max: 512, timeout: 5s}}
cache:
  enabled: true
  eviction: {policy}
  max_memory: {BUDGET}
  max_body_size: 1MiB
  purge: {{token: "cardinality-token-0123456789"}}
timeouts: {{origin: 10s}}
telemetry:
  admin: {{listen: "127.0.0.1:{admin}"}}
  prometheus: {{listen: "127.0.0.1:{metrics}"}}
routes:
  - id: open
    match: "/open/**"
    class: public_ssr
    cache: {{override_origin: true, ttl: {{max: 60s}}}}
  - id: bounded
    match: "/bounded/**"
    class: public_ssr
    cache:
      override_origin: true
      ttl: {{max: 60s}}
      query: {{mode: include, keys: [p]}}
'''


def load(listen, mix, seconds, folder, label, pid):
    path = Path(folder) / f'{label}.mix'
    path.write_text(''.join(f'{w}|{p}\n' for w, p in mix))
    out = subprocess.run(
        [str(ROOT / 'target/release/loadgen'), 'run', '--addr', f'127.0.0.1:{listen}',
         '--mix', str(path), '--connections', '32', '--threads', '2',
         '--duration', str(seconds), '--warmup', '1', '--timeout', '10',
         '--watch-pid', str(pid), '--label', label],
        capture_output=True, text=True, check=True)
    return json.loads(out.stdout.strip().splitlines()[-1])


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--policy', default='clock', choices=['clock', 'tinyufo'])
    parser.add_argument('--seconds', type=int, default=15)
    parser.add_argument('--json')
    args = parser.parse_args()
    hot = f'/size/{BODY}?p={{n:100}}'
    long_name = 'n' * 2048
    phases = {
        'hot': [(1, f'/open{hot}')],
        'flood': [(1, f'/open{hot}'), (1, f'/open/size/{BODY}?x={{u}}')],
        'flood-long': [(1, f'/open{hot}'), (1, f'/open/size/{BODY}?{long_name}{{u}}={"v" * 2048}')],
        'flood-purge': [(1, f'/open{hot}'), (1, f'/open/size/{BODY}?x={{u}}')],
        'bounded': [(1, f'/bounded{hot}'), (1, f'/bounded/size/{BODY}?p={{n:100}}&x={{u}}')],
    }
    results, failures = {'policy': args.policy, 'budget': BUDGET, 'ceiling': CEILING}, []
    with tempfile.TemporaryDirectory(prefix='tanod-qc-', dir=os.environ.get('TMPDIR')) as folder:
        origin_port = free_port()
        origin = subprocess.Popen([str(ROOT / 'target/release/slow-origin'), str(origin_port), '20'],
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        listen, metrics, admin = free_port(), free_port(), free_port()
        cfg = Path(folder) / 'tanod.yaml'
        cfg.write_text(config(folder, listen, metrics, admin, origin_port, args.policy))
        log = (Path(folder) / 'tanod.log').open('wb')
        tanod = subprocess.Popen([str(ROOT / 'target/release/tanod'), 'run', '--config', str(cfg)],
                                 stdout=log, stderr=log)
        try:
            for _ in range(200):
                try:
                    get(f'http://127.0.0.1:{admin}/health/ready', timeout=1)
                    get(f'http://127.0.0.1:{origin_port}/__stats', timeout=1)
                    break
                except OSError:
                    time.sleep(.05)
            else:
                raise SystemExit('tanod did not become ready:\n'
                                 + (Path(folder) / 'tanod.log').read_text(errors='replace')[-2000:])
            for label, mix in phases.items():
                # Each phase starts from an empty cache and a reset origin.
                get(f'http://127.0.0.1:{admin}/purge?all=true', data=b'',
                    headers={'Authorization': 'Bearer cardinality-token-0123456789'})
                get(f'http://127.0.0.1:{origin_port}/__reset')
                sampler = Sampler(tanod.pid, metrics, admin, origin_port)
                sampler.start()
                purging = threading.Event()
                purges = []

                def purge_loop():
                    while not purging.wait(.5):
                        try:
                            purges.append(get(
                                f'http://127.0.0.1:{admin}/purge?path=/open/size/{BODY}',
                                data=b'', headers={'Authorization': 'Bearer cardinality-token-0123456789'}))
                        except OSError as error:
                            purges.append(f'error {error}')
                purger = threading.Thread(target=purge_loop, daemon=True)
                if label == 'flood-purge':
                    purger.start()
                report = load(listen, mix, args.seconds, folder, label, tanod.pid)
                purging.set()
                time.sleep(1)
                sampler.stop.set()
                sampler.join()
                rows = sampler.rows
                first, last = rows[0], rows[-1]
                summary = {
                    'requests': report['requests'], 'rps': report['rps'], 'errors': report['errors'],
                    'status': report.get('status'),
                    'latency_us': report.get('latency_us'),
                    'per_target': report.get('targets') or report.get('per_target'),
                    'tanod_cpu_s': round(last['cpu_s'] - first['cpu_s'], 2),
                    'rss_peak_kb': max(r['rss_kb'] for r in rows),
                    'rss_end_kb': last['rss_kb'],
                    'cache_bytes_peak': max(r['cache_bytes'] for r in rows),
                    'pinned_peak': max(r['pinned'] for r in rows),
                    'entries_peak': max(r['entries'] for r in rows),
                    'evictions': last['evicted'] - first['evicted'],
                    'origin_renders': last['origin_total'],
                    'origin_peak': max(r['origin_peak'] for r in rows),
                    'pinned_end': last['pinned'], 'in_flight_end': last['in_flight'],
                    'origin_in_flight_end': last['origin_in_flight'],
                    'purges': len(purges), 'purge_errors': sum(1 for p in purges if p.startswith('error')),
                }
                results[label] = summary
                print(f'{label:<12} ' + ' '.join(f'{k}={v}' for k, v in summary.items()
                                                 if k not in ('latency_us', 'per_target', 'status')))
                checks = [
                    (summary['cache_bytes_peak'] <= BUDGET, 'cache bytes exceeded the budget'),
                    (summary['pinned_peak'] <= BUDGET + 1024 * 1024, 'fills exceeded budget + headroom'),
                    (summary['origin_peak'] <= CEILING, 'origin renders exceeded the ceiling'),
                    (summary['pinned_end'] == 0, 'fill bytes still pinned after traffic'),
                    (summary['in_flight_end'] == 0, 'origin permits still held after traffic'),
                    (summary['errors'] == 0, 'client errors'),
                    (summary['purge_errors'] == 0, 'purge errors'),
                ]
                failures += [f'{label}: {why}' for ok, why in checks if not ok]
        finally:
            tanod.send_signal(signal.SIGTERM)
            try:
                tanod.wait(15)
            except subprocess.TimeoutExpired:
                tanod.kill()
            origin.kill()
            log.close()
            text = (Path(folder) / 'tanod.log').read_text(errors='replace')
            if 'panicked' in text:
                failures.append('panic in tanod log')
    results['failures'] = failures
    if args.json:
        Path(args.json).write_text(json.dumps(results, indent=1))
    for failure in failures:
        print(f'FAIL {failure}')
    print('query cardinality bounded' if not failures else f'{len(failures)} failures')
    return 1 if failures else 0


if __name__ == '__main__':
    sys.exit(main())
