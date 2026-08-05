#!/usr/bin/env python3
"""Percentiles over a results CSV.

Reads the raw samples, prints a summary. It never writes to the input: the raw file is the
evidence, and a summary that has replaced its own source cannot be re-analysed by someone who
disagrees with the analysis.

Input format (docs/REPO-STRUCTURE.md): a header row `kind,index,ns`, then one row per sample. Rows
are grouped by `kind` so that a calibration series and a measurement series can live in the same
file without being mixed.

Usage:
    tools/summarise.py rung-01-kvm/results/vmexit-cost-host-2026-08-05.csv
    tools/summarise.py a.csv b.csv        # two runs, side by side, to eyeball the noise floor
"""

import csv
import sys
from collections import defaultdict

# Deliberately no mean. The reason to collect 200,000 samples is that the tail carries the finding,
# and a mean is the one statistic guaranteed to hide it.
QUANTILES = [("min", 0.0), ("p50", 0.50), ("p90", 0.90), ("p99", 0.99),
             ("p99.9", 0.999), ("max", 1.0)]


def quantile(sorted_values, q):
    """Nearest-rank quantile. No interpolation: every number printed is a sample that
    actually occurred, which matters when a reader wants to go find it in the raw file."""
    if not sorted_values:
        return None
    idx = round((len(sorted_values) - 1) * q)
    return sorted_values[idx]


def load(path):
    groups = defaultdict(list)
    with open(path, newline="") as fh:
        for row in csv.DictReader(fh):
            groups[row["kind"]].append(int(row["ns"]))
    return groups


def main(paths):
    if not paths:
        print(__doc__)
        return 2

    for path in paths:
        groups = load(path)
        print(f"\n{path}")
        for kind, values in sorted(groups.items()):
            values.sort()
            cells = "  ".join(
                f"{name}={quantile(values, q)}" for name, q in QUANTILES
            )
            print(f"  {kind:<24} n={len(values):<8} {cells}")
            if len(values) < 1000:
                print("    (fewer than 1,000 samples: exploratory only, p99 is not meaningful)")
            elif len(values) < 100_000:
                print("    (p99 meaningful; p99.9 is not, at this sample count)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
