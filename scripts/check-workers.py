#!/usr/bin/env python3
"""Linux affinity/startup/idle probe. Does not emulate cgroup CPU quotas.

Run only against a local validation binary. Results include explicit overrides
and several replicas sharing a CPU; throughput is measured by bench/perf.sh.
"""
import argparse
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import tempfile
import time


def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


def sample(pid):
    result = dict(cpu_ticks=0, context_switches=0)
    for task in Path(f'/proc/{pid}/task').iterdir():
        fields = (task / 'stat').read_text().split(') ', 1)[1].split()
        result['cpu_ticks'] += int(fields[11]) + int(fields[12])
        status = (task / 'status').read_text()
        result['context_switches'] += sum(int(v) for v in re.findall(r'^(?:nonvoluntary|voluntary)_ctxt_switches:\s+(\d+)', status, re.M))
    status = Path(f'/proc/{pid}/status').read_text()
    result['rss_kib'] = int(re.search(r'^VmRSS:\s+(\d+)', status, re.M)[1])
    result['threads'] = int(re.search(r'^Threads:\s+(\d+)', status, re.M)[1])
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary', type=Path)
    args = parser.parse_args()
    binary = str(args.binary.resolve())
    allowed = sorted(os.sched_getaffinity(0))
    for count, override, replicas in [(1, None, 1), (2, None, 1), (4, None, 1), (1, 4, 1), (1, None, 2), (1, None, 4)]:
        if len(allowed) < count:
            raise SystemExit(f'need {count} allowed CPUs, have {allowed}')
        cpus = ','.join(map(str, allowed[:count]))
        processes = []
        with tempfile.TemporaryDirectory(prefix='tanod-workers-', dir='/tmp') as folder:
            try:
                for replica in range(replicas):
                    path = Path(folder) / f'{replica}.yaml'
                    log = Path(folder) / f'{replica}.log'
                    port = free_port()
                    threads = f'  threads: {override}\n' if override else ''
                    path.write_text(f'''version: 1
server:
  listen: "127.0.0.1:{port}"
{threads}  graceful:
    drain_period: 0s
    shutdown_timeout: 1s
    pid_file: "{folder}/{replica}.pid"
    upgrade_socket: "{folder}/{replica}.sock"
origin:
  upstreams: ["127.0.0.1:9"]
  concurrency: {{max: 1}}
cache: {{enabled: false, max_memory: 8MiB, max_body_size: 1MiB}}
''')
                    with log.open('w') as stream:
                        proc = subprocess.Popen(['taskset', '-c', cpus, binary, 'run', '--config', str(path)], stdout=stream, stderr=stream)
                    processes.append(proc)
                    for _ in range(200):
                        if proc.poll() is not None:
                            raise RuntimeError(log.read_text())
                        found = re.search(r'\((\d+) worker threads\)', log.read_text())
                        if found:
                            assert int(found[1]) == (override or count), log.read_text()
                            break
                        time.sleep(.025)
                    else:
                        raise RuntimeError(f'startup timed out: {log.read_text()}')
                before = [sample(p.pid) for p in processes]
                time.sleep(1)
                after = [sample(p.pid) for p in processes]
                print(json.dumps(dict(affinity=cpus, override=override, replicas=replicas,
                    workers_per_replica=override or count, idle_seconds=1,
                    idle_cpu_seconds=sum(b['cpu_ticks']-a['cpu_ticks'] for a,b in zip(before,after))/os.sysconf('SC_CLK_TCK'),
                    idle_context_switches=sum(b['context_switches']-a['context_switches'] for a,b in zip(before,after)),
                    rss_kib=sum(s['rss_kib'] for s in after), threads=sum(s['threads'] for s in after))), flush=True)
            finally:
                for proc in processes:
                    if proc.poll() is None:
                        proc.terminate()
                for proc in processes:
                    try:
                        proc.wait(timeout=8)
                    except subprocess.TimeoutExpired:
                        proc.kill()
                        proc.wait()
                        raise
                assert all(p.returncode == 0 for p in processes), [p.returncode for p in processes]


if __name__ == '__main__':
    main()
