#!/usr/bin/env python3
"""Compare `cargo bench --bench hotpath` output from two builds.

    python3 bench/hotpath-compare.py 'baseline-*.jsonl' 'candidate-*.jsonl'

Each argument is a glob of JSON-lines files, one file per trial. Rows are
matched on scenario and concurrency; the figure compared is the median over
trials (operations per second, or nanoseconds per operation where that is
what the scenario reports), with the min-max spread beside it.
"""

import glob
import json
import statistics
import sys


def load(pattern):
    rows = {}
    for path in sorted(glob.glob(pattern)):
        with open(path) as f:
            for line in f:
                line = line.strip()
                if not line.startswith("{"):
                    continue
                r = json.loads(line)
                key = (r["scenario"], r.get("concurrency", r.get("routes", r.get("entries", ""))))
                for metric in ("ops_per_s", "ns_per_op", "median_ms"):
                    if metric in r:
                        rows.setdefault((key, metric), []).append(float(r[metric]))
    return rows


def main():
    if len(sys.argv) != 3:
        print(__doc__.strip(), file=sys.stderr)
        sys.exit(2)
    a, b = load(sys.argv[1]), load(sys.argv[2])
    print("| scenario | n | metric | baseline (min-max) | candidate (min-max) | diff |")
    print("|---|---|---|---|---|---|")
    for key in sorted(set(a) & set(b), key=lambda k: (k[0][0], str(k[0][1]).zfill(8))):
        (scenario, n), metric = key
        ma, mb = statistics.median(a[key]), statistics.median(b[key])
        diff = (mb - ma) / ma * 100 if ma else 0.0
        print("| {} | {} | {} | {:.4g} ({:.4g}-{:.4g}) | {:.4g} ({:.4g}-{:.4g}) | {:+.1f}% |".format(
            scenario, n, metric, ma, min(a[key]), max(a[key]),
            mb, min(b[key]), max(b[key]), diff))


if __name__ == "__main__":
    main()
