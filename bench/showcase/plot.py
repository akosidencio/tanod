#!/usr/bin/env python3
"""Charts for a showcase run, drawn only from what the run measured.

Usage: plot.py RUN_DIR

Reads RUN_DIR/results.json (from `showcase.py aggregate`) and each case's
timeline.csv and samples.csv. Writes RUN_DIR/charts/*.svg and *.png, a light and
a dark version of each, and charts/manifest.json listing every chart with the
cases it was drawn from. Nothing is interpolated, smoothed, or filled in: a
missing measurement is a gap in the chart, not a guess.
"""
import csv
import json
import os
import statistics
import sys
from pathlib import Path

ARM_LABEL = {'baseline': 'NGINX → Next.js', 'tanod': 'NGINX → Tanod → Next.js'}
WORKLOAD_LABEL = {'unique': 'Unique URLs (no reuse possible)', 'repeated': 'Repeated URLs (reuse possible)'}
MODE_LABEL = {
    'delay': 'Mechanism test: origins sleep per render (no CPU saturation)',
    'cpu': 'Constrained test: fixed CPU work per render',
}

# Categorical identity for the two arms (validated: both modes pass every check).
THEMES = {
    'light': {
        'surface': '#fcfcfb', 'ink': '#0b0b0b', 'ink2': '#52514e', 'muted': '#898781',
        'grid': '#e1e0d9', 'axis': '#c3c2b7', 'baseline': '#2a78d6', 'tanod': '#eb6834',
        'inside': '#ffffff',
    },
    'dark': {
        'surface': '#1a1a19', 'ink': '#f0efec', 'ink2': '#c3c2b7', 'muted': '#898781',
        'grid': '#2c2c2a', 'axis': '#383835', 'baseline': '#3987e5', 'tanod': '#d95926',
        'inside': '#ffffff',
    },
}

# Outcomes are status, not identity: good, intentional rejection, failure, and
# load the generator never sent. Each always carries a text label as well.
OUTCOME_STYLE = [
    ('ok_origin', '200 rendered by origin', '#0ca30c', '#ffffff'),
    ('ok_cache', '200 from Tanod cache', '#7ccf7c', '#0b0b0b'),
    ('shed', '503 shed by Tanod', '#fab219', '#0b0b0b'),
    ('rate_limited', '429 NGINX rate limit', '#b5b3ab', '#0b0b0b'),
    ('errors', '5xx / connection error', '#d03b3b', '#ffffff'),
    ('timeout', 'Client timeout', '#8c2323', '#ffffff'),
    ('dropped', 'Not sent (k6 out of VUs)', '#e1e0d9', '#0b0b0b'),
]


def load_rows(path):
    if not Path(path).exists():
        return []
    with open(path, newline='') as stream:
        return list(csv.DictReader(stream))


def setup(plt, theme):
    t = THEMES[theme]
    plt.rcParams.update({
        'svg.hashsalt': 'tanod-showcase-v1',
        'svg.fonttype': 'none',
        'font.family': ['DejaVu Sans'],
        'font.size': 10,
        'figure.facecolor': t['surface'],
        'axes.facecolor': t['surface'],
        'axes.edgecolor': t['axis'],
        'axes.labelcolor': t['ink2'],
        'axes.titlecolor': t['ink'],
        'axes.titlesize': 11,
        'axes.titleweight': 'bold',
        'axes.titlelocation': 'left',
        'axes.grid': True,
        'axes.axisbelow': True,
        'grid.color': t['grid'],
        'grid.linewidth': 0.8,
        'axes.spines.top': False,
        'axes.spines.right': False,
        'xtick.color': t['muted'],
        'ytick.color': t['muted'],
        'xtick.labelcolor': t['ink2'],
        'ytick.labelcolor': t['ink2'],
        'legend.frameon': False,
        'legend.labelcolor': t['ink2'],
        'lines.linewidth': 2,
        'text.color': t['ink'],
    })
    return t


def footer(fig, t, meta, extra=''):
    host = meta.get('host') or {}
    git = meta.get('git') or {}
    text = (f"Measured {meta.get('created_utc', '?')[:10]} · run {meta.get('run_id', '?')} · "
            f"commit {(git.get('commit') or '?')[:10]}{' (dirty)' if git.get('dirty_files') else ''} · "
            f"{host.get('cpu_model') or 'unknown CPU'}, {host.get('logical_cpus', '?')} CPUs. {extra}")
    fig.text(0.01, 0.01, text.strip(), fontsize=7.5, color=t['muted'], ha='left', va='bottom', wrap=True)


def heading(fig, t, title, subtitle):
    """Title and subtitle at a fixed distance from the top; returns the plot area's top."""
    height = fig.get_figheight()
    fig.text(0.01, 1 - 0.12 / height, title, ha='left', va='top', fontsize=11.5, fontweight='bold', color=t['ink'])
    fig.text(0.01, 1 - 0.40 / height, subtitle, ha='left', va='top', fontsize=8.5, color=t['ink2'])
    return 1 - 0.62 / height


def arm_legend(fig, t, top):
    """One legend for the figure, under the heading; returns the new plot top."""
    handles = [plt_line(t[arm]) for arm in ('baseline', 'tanod')]
    fig.legend(handles, [ARM_LABEL[arm] for arm in ('baseline', 'tanod')], loc='upper left',
               bbox_to_anchor=(0.005, top + 0.02 / fig.get_figheight()), ncol=2, fontsize=8.5)
    return top - 0.30 / fig.get_figheight()


def plt_line(color):
    from matplotlib.lines import Line2D
    return Line2D([0], [0], color=color, linewidth=2)


def layout(fig, top):
    fig.tight_layout(rect=(0, 0.42 / fig.get_figheight(), 1, top))


def save(fig, out, name, theme, manifest, sources, description):
    suffix = '' if theme == 'light' else '-dark'
    paths = []
    for extension in ('svg', 'png'):
        path = out / f'{name}{suffix}.{extension}'
        fig.savefig(path, dpi=200, metadata={'Date': None} if extension == 'svg' else None)
        paths.append(path.name)
    if theme == 'light':
        manifest.append({'chart': name, 'files': paths, 'description': description, 'cases': sources})
    return paths


def outcome_counts(case):
    o = case['outcomes']
    return {
        'ok_origin': o['ok_origin'], 'ok_cache': o['ok_cache'], 'shed': o['shed'],
        'rate_limited': o['rate_limited'],
        'errors': o['upstream_error'] + o['transport_error'] + o['other_status'],
        'timeout': o['timeout'], 'dropped': case['dropped_iterations'],
    }


def case_id(case):
    return f"{case['mode']}-{case['workload']}-{case['arm']}-t{case['trial']}"


def chart_outcomes(plt, t, results, mode, out, theme, manifest):
    """Share of attempted requests by outcome, one bar per workload and arm."""
    cases = [c for c in results['cases'] if c['mode'] == mode]
    rows = []
    for workload in ('unique', 'repeated'):
        for arm in ('baseline', 'tanod'):
            runs = [c for c in cases if c['workload'] == workload and c['arm'] == arm]
            if not runs:
                continue
            # Trials are pooled: counts add, so the shares are over every attempt.
            totals = {key: sum(outcome_counts(c)[key] for c in runs) for key, *_ in OUTCOME_STYLE}
            rows.append((workload, arm, totals, runs))
    if not rows:
        return
    fig, ax = plt.subplots(figsize=(10, 1.0 + 0.62 * len(rows) + 1.2))
    labels = []
    for index, (workload, arm, totals, runs) in enumerate(rows):
        attempted = sum(totals.values())
        left = 0.0
        y = len(rows) - 1 - index
        for key, label, color, ink in OUTCOME_STYLE:
            share = totals[key] / attempted if attempted else 0
            if share <= 0:
                continue
            ax.barh(y, share, left=left, height=0.62, color=color, edgecolor=t['surface'], linewidth=2)
            if share >= 0.07:
                ax.text(left + share / 2, y, f'{share:.0%}', ha='center', va='center', fontsize=8.5,
                        color=ink, fontweight='bold')
            left += share
        n = len(runs)
        labels.append((y, f"{ARM_LABEL[arm]}\n{WORKLOAD_LABEL[workload].split(' (')[0]} · "
                          f"{attempted:,} attempted{f' · {n} trials' if n > 1 else ''}"))
    ax.set_yticks([y for y, _ in labels], [text for _, text in labels], fontsize=8.5)
    ax.set_xlim(0, 1)
    ax.xaxis.set_major_formatter(plt.FuncFormatter(lambda v, _: f'{v:.0%}'))
    ax.grid(axis='y', visible=False)
    ax.tick_params(axis='y', length=0)
    rate = cases[0]['rate']
    top = heading(fig, t, f'What happened to every request · {rate} req/s offered for {cases[0]["duration_s"]}s',
                  MODE_LABEL[mode])
    handles = [plt.Rectangle((0, 0), 1, 1, color=color) for _, _, color, _ in OUTCOME_STYLE]
    ax.legend(handles, [label for _, label, _, _ in OUTCOME_STYLE], ncol=4, fontsize=8,
              loc='upper left', bbox_to_anchor=(0, -0.12))
    footer(fig, t, results.get('metadata') or {}, 'Shares of attempted iterations (sent + dropped).')
    layout(fig, top)
    save(fig, out, f'outcomes-{mode}', theme, manifest, [case_id(c) for _, _, _, runs in rows for c in runs],
         'Outcome of every attempted request, by workload and arm')
    plt.close(fig)


def chart_latency(plt, t, results, mode, out, theme, manifest):
    """Successful-response p50/p95/p99, median across trials with min-max range."""
    cases = [c for c in results['cases'] if c['mode'] == mode]
    if not cases:
        return
    fig, axes = plt.subplots(1, 2, figsize=(10, 3.6), sharex=True)
    sources = []
    measured = [c['ok_ms'][k] for c in cases for k in ('p50', 'p95', 'p99') if c['ok_ms'][k]]
    for ax, workload in zip(axes, ('unique', 'repeated')):
        ax.set_title(WORKLOAD_LABEL[workload], fontsize=10)
        row_labels = []
        for row, arm in enumerate(('tanod', 'baseline')):
            runs = [c for c in cases if c['workload'] == workload and c['arm'] == arm]
            successes = sum(c['ok_ms']['count'] for c in runs)
            row_labels.append(f"{ARM_LABEL[arm]}\nn = {successes:,} HTTP 200" + (f' ({len(runs)} trials)' if len(runs) > 1 else ''))
            sources += [case_id(c) for c in runs]
            for column, (key, marker) in enumerate((('p50', 'o'), ('p95', 's'), ('p99', 'D'))):
                values = [c['ok_ms'][key] for c in runs if c['ok_ms'][key] is not None]
                if not values:
                    continue
                y = row + (column - 1) * 0.18
                median = statistics.median(values)
                if len(values) > 1:
                    ax.plot([min(values), max(values)], [y, y], color=t[arm], linewidth=1.2, alpha=0.6)
                ax.plot(median, y, marker=marker, markersize=7, color=t[arm], markeredgecolor=t['surface'],
                        markeredgewidth=1.5, linestyle='none')
                ax.annotate(f'{key} {median:,.0f}', (median, y), xytext=(6, 0), textcoords='offset points',
                            va='center', fontsize=7.5, color=t['ink2'])
            counts = [c['ok_ms']['count'] for c in runs]
            if not runs:
                continue
            if not any(counts):
                ax.text(0.02, row, 'no successful responses', transform=ax.get_yaxis_transform(),
                        fontsize=8, color=t['ink2'], va='center')
        ax.set_yticks([0, 1], row_labels, fontsize=8.5)
        ax.set_ylim(-0.6, 1.6)
        ax.set_xscale('log')
        if measured:
            # Room on the right for the value labels; the same range on both panels.
            ax.set_xlim(min(measured) / 2, max(measured) * 6)
        ax.set_xlabel('Latency of HTTP 200 responses (ms, log scale)')
        ax.grid(axis='y', visible=False)
        ax.tick_params(axis='y', length=0)
    top = heading(fig, t, 'Successful-response latency · p50 ○  p95 □  p99 ◇', MODE_LABEL[mode])
    footer(fig, t, results.get('metadata') or {},
           'HTTP 200s only; shed, failed and timed-out requests are in the outcome chart. Median across trials.')
    layout(fig, top)
    save(fig, out, f'latency-{mode}', theme, manifest, sources,
         'p50/p95/p99 of successful responses, by workload and arm')
    plt.close(fig)


def origin_series(case_dir, interval):
    """Renders in progress across the three origins over time.

    From the origins' render log (exact, maximum per 100 ms) when the analysis
    produced it; otherwise the sampler's per-tick sum, which can miss peaks.
    """
    logged = load_rows(case_dir / 'origin-concurrency.csv')
    if logged:
        return [float(r['t']) for r in logged], [float(r['renders_in_progress']) for r in logged], 'log'
    ticks = {}
    for row in load_rows(case_dir / 'samples.csv'):
        if row['source'].startswith('next-') and row['metric'] == 'active':
            tick = round(float(row['time']) / interval)
            ticks.setdefault(tick, {})[row['source']] = float(row['value'])
    complete = sorted((tick, sum(v.values())) for tick, v in ticks.items() if len(v) == 3)
    if not complete:
        return [], [], None
    start = complete[0][0]
    return [(tick - start) * interval for tick, _ in complete], [value for _, value in complete], 'sampled'



def chart_origin_concurrency(plt, t, results, run_dir, mode, out, theme, manifest):
    cases = [c for c in results['cases'] if c['mode'] == mode and c['trial'] == 1]
    interval = results['params']['sample_interval_s']
    workloads = [w for w in ('unique', 'repeated') if any(c['workload'] == w for c in cases)]
    if not workloads:
        return
    fig, axes = plt.subplots(len(workloads), 1, figsize=(10, 2.9 * len(workloads) + 0.6), squeeze=False)
    sources = []
    ceiling = None
    kinds = set()
    for ax, workload in zip(axes[:, 0], workloads):
        top = 0
        for arm in ('baseline', 'tanod'):
            case = next((c for c in cases if c['workload'] == workload and c['arm'] == arm), None)
            if not case:
                continue
            ceiling = case['tanod_max']
            xs, ys, kind = origin_series(run_dir / 'cases' / case_id(case), interval)
            if not xs:
                continue
            kinds.add(kind)
            sources.append(case_id(case))
            ax.plot(xs, ys, color=t[arm], label=ARM_LABEL[arm], drawstyle='steps-post')
            peak = case['origin']['peak_total_concurrent']
            if peak is not None:
                ax.annotate(f'peak {peak}', (xs[ys.index(max(ys))], max(ys)), xytext=(4, 4),
                            textcoords='offset points', fontsize=8, color=t['ink2'])
            top = max(top, max(ys))
        if ceiling is not None:
            ax.axhline(ceiling, color=t['ink2'], linewidth=1, linestyle=(0, (4, 3)))
            ax.annotate(f'Tanod ceiling: {ceiling}', (1.0, ceiling), xycoords=('axes fraction', 'data'),
                        xytext=(-4, 4), textcoords='offset points', ha='right', fontsize=8, color=t['ink2'])
        ax.set_ylim(0, max(top, ceiling or 0) * 1.15 + 1)
        ax.set_title(WORKLOAD_LABEL[workload], fontsize=10)
        ax.set_ylabel('Renders in progress\n(sum of 3 origins)')
    axes[-1, 0].set_xlabel('Seconds since the measured phase started (includes the post-load drain)')
    top = arm_legend(fig, t, heading(fig, t, 'Concurrent work inside the Next.js origins, reported by the origins themselves',
                                     MODE_LABEL[mode]))
    source = ("From the origins' own render_start/render_end log; maximum per 100 ms."
              if kinds == {'log'} else f'Includes sampled data (every {interval}s), which can miss brief peaks.')
    footer(fig, t, results.get('metadata') or {}, source)
    layout(fig, top)
    save(fig, out, f'origin-concurrency-{mode}', theme, manifest, sources,
         'Renders in progress across the three origins over time (trial 1)')
    plt.close(fig)


def chart_goodput(plt, t, results, run_dir, mode, out, theme, manifest):
    cases = [c for c in results['cases'] if c['mode'] == mode and c['trial'] == 1]
    workloads = [w for w in ('unique', 'repeated') if any(c['workload'] == w for c in cases)]
    if not workloads:
        return
    fig, axes = plt.subplots(len(workloads), 1, figsize=(10, 2.7 * len(workloads) + 0.6), squeeze=False)
    sources = []
    for ax, workload in zip(axes[:, 0], workloads):
        for arm in ('baseline', 'tanod'):
            case = next((c for c in cases if c['workload'] == workload and c['arm'] == arm), None)
            if not case:
                continue
            rows = load_rows(run_dir / 'cases' / case_id(case) / 'timeline.csv')
            if not rows:
                continue
            sources.append(case_id(case))
            xs = [int(r['t']) for r in rows]
            ok = [int(r['ok_origin']) + int(r['ok_cache']) for r in rows]
            ax.plot(xs, ok, color=t[arm], label=ARM_LABEL[arm], drawstyle='steps-post')
        offered = cases[0]['rate']
        ax.axhline(offered, color=t['ink2'], linewidth=1, linestyle=(0, (4, 3)))
        ax.annotate(f'offered: {offered} req/s', (1.0, offered), xycoords=('axes fraction', 'data'),
                    xytext=(-4, 4), textcoords='offset points', ha='right', fontsize=8, color=t['ink2'])
        ax.set_ylim(0, offered * 1.25)
        ax.set_title(WORKLOAD_LABEL[workload], fontsize=10)
        ax.set_ylabel('HTTP 200 per second')
    axes[-1, 0].set_xlabel('Seconds since the measured phase started')
    top = arm_legend(fig, t, heading(fig, t, 'Successful responses per second, as k6 received them', MODE_LABEL[mode]))
    footer(fig, t, results.get('metadata') or {}, 'Bucketed by the second each response completed.')
    layout(fig, top)
    save(fig, out, f'goodput-{mode}', theme, manifest, sources, 'HTTP 200 per second over time (trial 1)')
    plt.close(fig)


def chart_origin_work(plt, t, results, mode, out, theme, manifest):
    """Origin renders per successful response: reuse and wasted work, side by side."""
    cases = [c for c in results['cases'] if c['mode'] == mode]
    rows = []
    for workload in ('unique', 'repeated'):
        for arm in ('baseline', 'tanod'):
            runs = [c for c in cases if c['workload'] == workload and c['arm'] == arm
                    and c['origin']['renders'] is not None]
            renders = sum(c['origin']['renders'] for c in runs)
            successes = sum(c['successful'] for c in runs)
            if runs:
                rows.append((workload, arm, renders, successes, runs))
    if not rows:
        return
    fig, ax = plt.subplots(figsize=(10, 1.0 + 0.55 * len(rows) + 0.9))
    for index, (workload, arm, renders, successes, runs) in enumerate(rows):
        y = len(rows) - 1 - index
        ax.barh(y, renders, height=0.55, color=t[arm], edgecolor=t['surface'], linewidth=2)
        ratio = f'{renders / successes:.2f} renders per 200' if successes else 'no successful responses'
        ax.annotate(f'{renders:,} renders → {successes:,} × HTTP 200 ({ratio})', (renders, y), xytext=(6, 0),
                    textcoords='offset points', va='center', fontsize=8, color=t['ink2'])
    ax.set_yticks([len(rows) - 1 - i for i in range(len(rows))],
                  [f"{ARM_LABEL[arm]} · {workload}" for workload, arm, *_ in rows], fontsize=8.5)
    ax.set_xlim(0, max(r[2] for r in rows) * 1.75 or 1)
    ax.set_xlabel('Renders started by the origins during the measured phase (origin counters)')
    ax.grid(axis='y', visible=False)
    ax.tick_params(axis='y', length=0)
    top = heading(fig, t, 'Origin work done for the responses that succeeded', MODE_LABEL[mode])
    footer(fig, t, results.get('metadata') or {},
           'In the unique workload, renders beyond the 200 count did not reach a client successfully.')
    layout(fig, top)
    save(fig, out, f'origin-work-{mode}', theme, manifest, [case_id(c) for *_, runs in rows for c in runs],
         'Origin renders versus successful responses, by workload and arm')
    plt.close(fig)


def main():
    run_dir = Path(sys.argv[1])
    results = json.loads((run_dir / 'results.json').read_text())
    out = run_dir / 'charts'
    out.mkdir(exist_ok=True)
    os.environ.setdefault('MPLCONFIGDIR', str(out / '.matplotlib-cache'))
    import matplotlib
    matplotlib.use('Agg')
    import matplotlib.pyplot as plt
    manifest = []
    modes = [m for m in ('delay', 'cpu') if any(c['mode'] == m for c in results['cases'])]
    for theme in ('light', 'dark'):
        t = setup(plt, theme)
        for mode in modes:
            chart_outcomes(plt, t, results, mode, out, theme, manifest)
            chart_latency(plt, t, results, mode, out, theme, manifest)
            chart_origin_concurrency(plt, t, results, run_dir, mode, out, theme, manifest)
            chart_goodput(plt, t, results, run_dir, mode, out, theme, manifest)
            chart_origin_work(plt, t, results, mode, out, theme, manifest)
    (out / 'manifest.json').write_text(json.dumps({
        'generated_from': 'results.json and cases/*/{timeline,samples}.csv',
        'matplotlib': matplotlib.__version__,
        'charts': manifest,
    }, indent=2) + '\n')
    print(f'  {len(manifest)} charts (light + dark, SVG + PNG) -> {out}')


if __name__ == '__main__':
    main()
