#!/usr/bin/env python3
"""Compare two bench/perf.sh result directories.

    python3 bench/perf-compare.py BASELINE_DIR CANDIDATE_DIR [--json]

For every run label present in both, prints the median over trials of each
metric for both sides, the spread (min-max) behind each median, and the
relative difference. Medians, not means: one trial interrupted by an
unrelated process should move the spread, not the headline.

Nothing here decides pass or fail. Whether a difference is real is a
judgement about the spread next to it.
"""

import json
import os
import statistics
import sys

METRICS = [
    ("rps", lambda r: r.get("rps"), "higher"),
    ("p50_us", lambda r: r.get("latency_us", {}).get("p50"), "lower"),
    ("p99_us", lambda r: r.get("latency_us", {}).get("p99"), "lower"),
    ("ttfb_p50_us", lambda r: r.get("ttfb_us", {}).get("p50"), "lower"),
    ("ttfb_max_us", lambda r: r.get("ttfb_us", {}).get("max"), "lower"),
    ("fast_p99_us", lambda r: r.get("fast_latency_us", {}).get("p99"), "lower"),
    ("errors", lambda r: r.get("errors"), "lower"),
    ("non_2xx", lambda r: (r.get("status", {}).get("4xx", 0) + r.get("status", {}).get("5xx", 0))
     if "status" in r else None, "lower"),
    ("tanod_cpu_cores", lambda r: (r.get("processes") or [{}])[0].get("cpu_cores"), "lower"),
    ("cpu_us_per_req", lambda r: cpu_per_request(r), "lower"),
    ("rss_peak_kb", lambda r: (r.get("processes") or [{}])[0].get("rss_kb_peak_window"), "lower"),
    ("origin_renders", lambda r: r.get("origin_renders"), "lower"),
    ("origin_peak", lambda r: r.get("origin_peak"), "lower"),
]


def cpu_per_request(r):
    procs = r.get("processes") or []
    if not procs or not r.get("requests"):
        return None
    return procs[0].get("cpu_s", 0) * 1e6 / r["requests"]


def load(directory):
    runs = {}
    for name in sorted(os.listdir(directory)):
        if not name.endswith(".jsonl"):
            continue
        with open(os.path.join(directory, name)) as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                r = json.loads(line)
                key = r.get("label") or "{}:{}:{}".format(
                    r.get("scenario"), r.get("connections", ""), r.get("path", ""))
                key = "{}/{}".format(name[:-6], key)
                runs.setdefault(key, []).append(r)
    return runs


def summarise(records, extract):
    values = [v for v in (extract(r) for r in records) if v is not None]
    if not values:
        return None
    return statistics.median(values), min(values), max(values)


def fmt(value):
    if value is None:
        return "-"
    if abs(value) >= 100:
        return "{:.0f}".format(value)
    return "{:.3g}".format(value)


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    if len(args) != 2:
        print(__doc__.strip(), file=sys.stderr)
        sys.exit(2)
    base, cand = load(args[0]), load(args[1])
    rows = []
    for key in sorted(set(base) & set(cand)):
        for name, extract, better in METRICS:
            b = summarise(base[key], extract)
            c = summarise(cand[key], extract)
            if b is None or c is None:
                continue
            delta = None
            if b[0]:
                delta = (c[0] - b[0]) / b[0] * 100
            rows.append({
                "run": key, "metric": name, "better": better,
                "baseline": b[0], "baseline_min": b[1], "baseline_max": b[2],
                "candidate": c[0], "candidate_min": c[1], "candidate_max": c[2],
                "delta_pct": delta, "trials": [len(base[key]), len(cand[key])],
            })
    if "--json" in sys.argv:
        json.dump(rows, sys.stdout, indent=1)
        print()
        return
    print("| run | metric | baseline (min-max) | candidate (min-max) | diff |")
    print("|---|---|---|---|---|")
    for r in rows:
        diff = "-" if r["delta_pct"] is None else "{:+.1f}%".format(r["delta_pct"])
        print("| {} | {} | {} ({}-{}) | {} ({}-{}) | {} |".format(
            r["run"], r["metric"],
            fmt(r["baseline"]), fmt(r["baseline_min"]), fmt(r["baseline_max"]),
            fmt(r["candidate"]), fmt(r["candidate_min"]), fmt(r["candidate_max"]),
            diff))


if __name__ == "__main__":
    main()
