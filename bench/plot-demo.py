#!/usr/bin/env python3
"""Export a capacity-demo CSV and Matplotlib figure from perf.sh JSONL.

Usage: python bench/plot-demo.py RESULTS_DIR OUTPUT_DIR
Needs Matplotlib only for the standalone plot; never runs load itself.
"""
import csv
import json
import os
from pathlib import Path
import statistics
import sys


def main():
    source, out = map(Path, sys.argv[1:])
    out.mkdir(parents=True, exist_ok=True)
    os.environ.setdefault('MPLCONFIGDIR', str(out / '.matplotlib-cache'))
    import matplotlib
    matplotlib.use('Agg')
    matplotlib.rcParams['svg.hashsalt'] = 'tanod-capacity-demo-v1'
    import matplotlib.pyplot as plt
    rows = [json.loads(line) for line in (source / 'overload.jsonl').read_text().splitlines()]
    measured = [r for r in rows if r.get('scenario') == 'overload' and 'rps' in r]
    if not measured or any('successful_latency_us' not in r for r in measured):
        raise SystemExit('need the corrected loadgen with separate successful-response latencies')
    exported = []
    for row in measured:
        origin = next(r for r in rows if r.get('scenario') == 'overload-origin'
                      and all(r.get(k) == row.get(k) for k in ('trial', 'connections', 'path')))
        success = row['status']['2xx']
        result = dict(path=row['path'], trial=row['trial'], connections=row['connections'],
                      successful_responses=success, successful_rps=success / row['window_s'],
                      http_503=row['status_codes'].get('503', 0), transport_errors=row['errors'],
                      censored_requests=row['censored_requests'],
                      successful_p99_ms=row['successful_latency_us']['p99'] / 1000,
                      all_p99_ms=row['latency_us']['p99'] / 1000,
                      origin_outstanding_peak=origin['origin_peak'],
                      origin_completed_uncached_responses_in_window=success,
                      origin_requests_received_including_warmup=origin.get('origin_requests_received_including_warmup', origin.get('origin_renders_including_warmup')))
        if row['path'] == 'tanod':
            outcome = next(r for r in rows if r.get('scenario') == 'overload-outcomes'
                           and all(r.get(k) == row.get(k) for k in ('trial', 'connections', 'path')))
            for key in ('intentional_rejections', 'origin_5xx', 'cache_hits', 'responses'):
                result[key + '_including_warmup'] = outcome[key]
            result['cache_hit_ratio_including_warmup'] = outcome['cache_hits'] / max(outcome['responses'], 1)
        exported.append(result)
    columns = list(dict.fromkeys(key for row in exported for key in row))
    with (out / 'capacity-demo.csv').open('w', newline='') as stream:
        writer = csv.DictWriter(stream, fieldnames=columns)
        writer.writeheader()
        writer.writerows(exported)
    fig, axes = plt.subplots(1, 2, figsize=(10, 4), layout='constrained')
    for name, label, color in [('direct', 'Direct origin', '#bd6523'), ('tanod', 'Tanod protected', '#087f8c')]:
        concurrencies = sorted({r['connections'] for r in exported if r['path'] == name})
        for axis, key in zip(axes, ('successful_rps', 'successful_p99_ms')):
            values = [[r[key] for r in exported if r['path'] == name and r['connections'] == c] for c in concurrencies]
            median = [statistics.median(v) for v in values]
            axis.errorbar(concurrencies, median,
                          yerr=[[m-min(v) for m,v in zip(median,values)], [max(v)-m for m,v in zip(median,values)]],
                          marker='o', color=color, label=label, capsize=3)
            axis.set_xscale('log', base=2)
            axis.set_xticks(concurrencies, labels=concurrencies)
            axis.set_xlabel('Concurrent clients')
            axis.grid(alpha=.2)
    axes[0].set_ylabel('Successful responses / second')
    axes[1].set_ylabel('Successful-response p99 (ms)')
    axes[0].legend()
    fig.suptitle('Capacity-limited origin: 8 render slots, 50 ms simulated work\nMedians and full trial ranges; excess protected demand may receive intentional 503')
    for extension in ('svg', 'png'):
        fig.savefig(out / f'capacity-demo.{extension}', dpi=160, metadata={'Date': None} if extension == 'svg' else None)
    (out / 'plot-tool.json').write_text(json.dumps(dict(matplotlib=matplotlib.__version__), indent=2)+'\n')


if __name__ == '__main__':
    main()
