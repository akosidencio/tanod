#!/usr/bin/env python3
"""Export all measured trial rows and a compact median/range comparison."""
import csv
import json
from pathlib import Path
import statistics
import sys


def main():
    source, out = map(Path, sys.argv[1:])
    out.mkdir(parents=True, exist_ok=True)
    records = []
    for directory in sorted(source.iterdir()):
        if not directory.is_dir():
            continue
        for file in sorted(directory.glob('*.jsonl')):
            if file.name == 'environments.jsonl':
                continue
            for line in file.read_text().splitlines():
                row = json.loads(line)
                if 'rps' not in row:
                    continue
                process = (row.get('processes') or [{}])[0]
                records.append(dict(build=directory.name, scenario=row['scenario'], trial=row['trial'],
                    connections=row['connections'], body=row.get('body'), label=row['label'],
                    requests=row['requests'], rps=row['rps'], p50_us=row['latency_us']['p50'],
                    p95_us=row['latency_us']['p95'], p99_us=row['latency_us']['p99'],
                    successful_p99_us=row['successful_latency_us']['p99'],
                    ttfb_p50_us=row['ttfb_us']['p50'], ttfb_p99_us=row['ttfb_us']['p99'],
                    transport_errors=row['errors'], censored_requests=row['censored_requests'],
                    cpu_us_per_request=process.get('cpu_s',0)*1e6/max(row['requests'],1),
                    rss_peak_window_kib=process.get('rss_kb_peak_window'),
                    hwm_kib=process.get('hwm_kb'), context_switches=process.get('context_switches')))
    if not records:
        raise SystemExit('no measured rows')
    with (out/'trials.csv').open('w',newline='') as stream:
        writer=csv.DictWriter(stream,fieldnames=list(records[0])); writer.writeheader(); writer.writerows(records)
    def display(values):
        if not values:
            return 'missing'
        return f'{statistics.median(values):,.1f} ({min(values):,.1f}–{max(values):,.1f}; n={len(values)})'
    with (out/'comparison.md').open('w') as stream:
        stream.write('| Policy / workload | Metric | Phase 2 median (range) | Candidate median (range) |\n|---|---|---|---|\n')
        for policy in ('clock','tinyufo'):
            for scenario,body,connections in [('hit',1024,8),('hit',1024,32),('sizes',4194304,32),('sizes',4194304,512),('miss',16384,512)]:
                selected={side:[r for r in records if r['build']==f'{side}-{policy}' and r['scenario']==scenario and r['body']==body and r['connections']==connections] for side in ('baseline','candidate')}
                for metric in ('rps','p99_us','cpu_us_per_request','rss_peak_window_kib'):
                    values={side:[r[metric] for r in selected[side]] for side in selected}
                    stream.write(f'| {policy}: {scenario}, {body} B, c{connections} | {metric} | {display(values["baseline"])} | {display(values["candidate"])} |\n')
    print(f'Exported {len(records)} measured trial rows')


if __name__ == '__main__':
    main()
