#!/usr/bin/env bash
# Compare two bench result directories produced by scripts/bench.sh.
# Prints a per-scenario delta table for elapsed time, peak RSS, and
# stdout bytes. Exits 1 if any metric regresses by more than the threshold
# (default 10%). CI-runnable.
#
# Usage:
#   bash scripts/bench-compare.sh <before-dir> <after-dir> [threshold]
#
# Example:
#   bash scripts/bench-compare.sh target/bench-results/before \
#                                 target/bench-results/after 0.10

set -euo pipefail

if [[ $# -lt 2 ]]; then
    echo "usage: $0 <before-dir> <after-dir> [threshold-fraction]" >&2
    exit 2
fi

BEFORE="$1"
AFTER="$2"
THRESHOLD="${3:-0.10}"

if [[ ! -d "$BEFORE" || ! -d "$AFTER" ]]; then
    echo "error: both directories must exist" >&2
    exit 2
fi

# Use python for the JSON parsing + median + delta math; avoids depending
# on jq. Median is more honest than mean for small sample counts.
python3 - "$BEFORE" "$AFTER" "$THRESHOLD" <<'PY'
import json
import os
import statistics
import sys

before_dir, after_dir, threshold = sys.argv[1], sys.argv[2], float(sys.argv[3])

def median(values):
    return statistics.median(values) if values else 0.0

def load(directory):
    out = {}
    for name in sorted(os.listdir(directory)):
        if not name.endswith(".json"):
            continue
        with open(os.path.join(directory, name)) as fh:
            data = json.load(fh)
        scenario = data["scenario"]
        out[scenario] = {
            "elapsed_secs": median([float(v) for v in data["elapsed_secs"]]),
            "max_rss_kb": median([float(v) for v in data["max_rss_kb"]]),
            "stdout_bytes": median([float(v) for v in data["stdout_bytes"]]),
        }
    return out

before = load(before_dir)
after = load(after_dir)

regressions = []

# Stable column widths.
hdr = f"{'scenario':<28}  {'metric':<14}  {'before':>12}  {'after':>12}  {'delta':>10}"
sep = "-" * len(hdr)
print(hdr)
print(sep)

scenarios = sorted(set(before) | set(after))
for s in scenarios:
    if s not in before or s not in after:
        marker = "added" if s in after else "removed"
        print(f"{s:<28}  {marker}")
        continue
    b = before[s]
    a = after[s]
    for metric in ("elapsed_secs", "max_rss_kb", "stdout_bytes"):
        bv = b[metric]
        av = a[metric]
        if bv == 0:
            ratio = 0.0 if av == 0 else float("inf")
        else:
            ratio = (av - bv) / bv
        sign = "+" if ratio > 0 else ""
        if metric == "elapsed_secs":
            bvs = f"{bv:.4f}"
            avs = f"{av:.4f}"
        else:
            bvs = f"{int(bv)}"
            avs = f"{int(av)}"
        ds = "n/a" if ratio == float("inf") else f"{sign}{ratio*100:.1f}%"
        flag = ""
        if ratio > threshold:
            flag = "  REGRESSION"
            regressions.append((s, metric, bv, av, ratio))
        elif ratio < -threshold:
            flag = "  improved"
        print(f"{s:<28}  {metric:<14}  {bvs:>12}  {avs:>12}  {ds:>10}{flag}")

if regressions:
    print()
    print(f"FAIL: {len(regressions)} metric(s) regressed by > {threshold*100:.0f}%:")
    for s, metric, bv, av, ratio in regressions:
        print(f"  - {s} {metric}: {bv} -> {av} ({ratio*100:+.1f}%)")
    sys.exit(1)
PY
