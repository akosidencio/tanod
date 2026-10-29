#!/usr/bin/env python3
"""Local SIGTERM/SIGINT coverage, using real proxy/origin processes.

No production endpoints. Every process, port and temporary file belongs to
this run. Assertions use published queue/fill state, not startup sleeps.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import http.client
import importlib.util
import json
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('ws_client', ROOT / 'bench/ws-client.py')
ws = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ws)


def port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


def request(port, path, first=None):
    conn = http.client.HTTPConnection('127.0.0.1', port, timeout=12)
    try:
        conn.request('GET', path)
        response = conn.getresponse()
        data = response.read(1)
        if first:
            first.set()
        return response.status, data + response.read()
    finally:
        conn.close()


def until(check, why, seconds=8):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        try:
            value = check()
            if value:
                return value
        except (OSError, http.client.HTTPException):
            pass
        time.sleep(.02)
    raise AssertionError(f'timed out: {why}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--tanod', type=Path, default=ROOT / 'target/debug/tanod')
    parser.add_argument('--origin', type=Path, default=ROOT / 'target/debug/slow-origin')
    args = parser.parse_args()
    for policy in ('clock', 'tinyufo'):
        for scenario in ('idle', 'queued-concurrency', 'cache-fill', 'slow-stream', 'websocket', 'reload-then-stop', 'sigint'):
            with tempfile.TemporaryDirectory(prefix='tanod-life-', dir='/tmp') as folder:
                base = Path(folder)
                origin_port, proxy_port, admin_port = port(), port(), port()
                config = base / 'tanod.yaml'
                config.write_text(f'''version: 1
server:
  listen: "127.0.0.1:{proxy_port}"
  threads: 2
  graceful:
    pid_file: "{folder}/tanod.pid"
    upgrade_socket: "{folder}/upgrade.sock"
    drain_period: 3s
    shutdown_timeout: 4s
origin:
  upstreams: ["127.0.0.1:{origin_port}"]
  concurrency:
    max: 4
    queue: {{max: 32, timeout: 5s}}
cache:
  enabled: true
  eviction: {policy}
  max_memory: 8MiB
  max_body_size: 1MiB
upgrade: {{enabled: true, max_concurrent: 4}}
telemetry:
  admin: {{listen: "127.0.0.1:{admin_port}"}}
debug_headers: false
routes:
  - id: cached
    match: "/stream/**"
    class: public_ssr
    cache: {{override_origin: true, ttl: {{max: 60s}}}}
  - id: uncached
    match: "/**"
    class: public_dynamic
    cache: {{enabled: false}}
''')
                children = []
                tunnel = None
                pool = ThreadPoolExecutor(max_workers=16)
                try:
                    for name, command in [('origin', [str(args.origin.resolve()), str(origin_port), '250']),
                                          ('tanod', [str(args.tanod.resolve()), 'run', '--config', str(config)])]:
                        with (base / f'{name}.log').open('w') as log:
                            proc = subprocess.Popen(command, stdout=log, stderr=log)
                            children.append(proc)
                        if name == 'origin':
                            until(lambda: request(origin_port, '/__stats')[0] == 200, 'origin startup')
                    tanod = children[-1]
                    until(lambda: request(admin_port, '/health/ready')[0] == 200, 'proxy readiness')
                    status = lambda: json.loads(request(admin_port, '/status')[1])
                    jobs = []
                    if scenario in ('queued-concurrency', 'reload-then-stop'):
                        if scenario == 'reload-then-stop':
                            config.write_text(config.read_text().replace('debug_headers: false', 'debug_headers: true'))
                            tanod.send_signal(signal.SIGHUP)
                            until(lambda: status()['config']['generation'] == 2, 'reload publication')
                        jobs = [pool.submit(request, proxy_port, f'/uncached/{i}') for i in range(16)]
                        def queued():
                            state = status()['admission']
                            return any(limiter['queue_depth'] > 0 for limiter in [state['global'], *state['tiers'], *state['routes']])
                        until(queued, 'request queued in an admission limiter')
                    elif scenario in ('cache-fill', 'slow-stream'):
                        first = threading.Event()
                        chunks = 20 if scenario == 'slow-stream' else 6
                        jobs = [pool.submit(request, proxy_port, f'/stream/{chunks}', first) for _ in range(8)]
                        assert first.wait(5), 'stream did not deliver a progressive first byte'
                        until(lambda: status()['cache']['pinned_bytes'] > 0, 'active cache fill')
                    elif scenario == 'websocket':
                        tunnel = socket.create_connection(('127.0.0.1', proxy_port), timeout=10)
                        code, problem, _ = ws.handshake(tunnel, '127.0.0.1', proxy_port, '/ws/echo')
                        assert (code, problem) == (101, None), (code, problem)
                    began = time.monotonic()
                    tanod.send_signal(signal.SIGINT if scenario == 'sigint' else signal.SIGTERM)
                    if scenario != 'sigint':
                        until(lambda: request(admin_port, '/health/ready')[0] == 503, 'readiness withdraws')
                        assert request(admin_port, '/health/live')[0] == 200
                    for job in jobs:
                        code, body = job.result(timeout=12)
                        assert code == 200 and body, (scenario, code, body)
                        if scenario in ('cache-fill', 'slow-stream'):
                            assert body == b''.join(f'<div>chunk {i}</div>'.encode() for i in range(chunks)), body
                    if jobs and scenario != 'slow-stream':
                        # Work completes before listener closure. Published state
                        # proves permits and fills return, rather than assuming
                        # process exit has cleaned them up.
                        until(lambda: status()['admission']['global']['in_flight'] == 0, 'permits returned')
                        until(lambda: status()['cache']['pinned_bytes'] == 0, 'fills released')
                    tanod.wait(timeout=12)
                    assert tanod.returncode == 0, (base / 'tanod.log').read_text()
                    if tunnel:
                        # Pingora drains the TCP tunnel by its deadline; it does
                        # not promise an application WebSocket close frame.
                        assert tunnel.recv(1) == b'', 'tunnel outlived process shutdown'
                    elapsed = time.monotonic() - began
                    assert elapsed < 12, elapsed
                    if scenario in ('cache-fill', 'slow-stream'):
                        stats = json.loads(request(origin_port, '/__stats')[1])
                        assert stats['total'] == 1, stats
                    print(json.dumps(dict(policy=policy, scenario=scenario, exit_code=tanod.returncode,
                                          seconds=round(elapsed, 3), completed=len(jobs))), flush=True)
                except BaseException:
                    for name in ('tanod', 'origin'):
                        path = base / f'{name}.log'
                        if path.exists():
                            print(path.read_text()[-12000:], flush=True)
                    raise
                finally:
                    if tunnel:
                        tunnel.close()
                    pool.shutdown(wait=True, cancel_futures=True)
                    for proc in reversed(children):
                        if proc.poll() is None:
                            proc.terminate()
                        try:
                            proc.wait(timeout=8)
                        except subprocess.TimeoutExpired:
                            proc.kill()
                            proc.wait()


if __name__ == '__main__':
    main()
