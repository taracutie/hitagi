#!/usr/bin/env bash
# End-to-end CLI benchmark harness for hitagi.
#
# For each scenario, runs the hitagi binary RUNS times under /usr/bin/time -v,
# records elapsed seconds, peak RSS (kB), and stdout byte count per run, and
# emits one JSON file per scenario into $HITAGI_BENCH_OUT.
#
# Usage:
#   HITAGI_BENCH_OUT=target/bench-results/before bash scripts/bench.sh
#
# Environment:
#   HITAGI_BENCH_OUT   Output directory for JSON. Default: target/bench-results/run.
#   HITAGI_BENCH_RUNS  Iterations per scenario. Default: 5.
#   HITAGI_BENCH_BIN   Path to the hitagi binary. Default: target/release/hitagi.
#   HITAGI_BENCH_CORPUS Repo to bench against. Default: tests/fixtures/sample_repo.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$HERE"

OUT="${HITAGI_BENCH_OUT:-target/bench-results/run}"
RUNS="${HITAGI_BENCH_RUNS:-5}"
BIN="${HITAGI_BENCH_BIN:-target/release/hitagi}"
CORPUS="${HITAGI_BENCH_CORPUS:-tests/fixtures/sample_repo}"

if [[ ! -x "$BIN" ]]; then
    echo "building release binary..." >&2
    cargo build --release --quiet
fi
if [[ ! -x "$BIN" ]]; then
    echo "error: $BIN not found after build" >&2
    exit 1
fi

mkdir -p "$OUT"
ABS_CORPUS="$(cd "$CORPUS" && pwd)"
ABS_BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"

# A synthetic git-initialized repo for the diff_overview scenario, so we
# don't pollute the host repo and so it stays deterministic.
DIFF_REPO="$(mktemp -d -t hitagi-bench-diff-XXXXXX)"
trap 'rm -rf "$DIFF_REPO"' EXIT
(
    cd "$DIFF_REPO"
    git init -q
    git -c user.email=b@b -c user.name=b config commit.gpgsign false
    printf 'fn main() {}\n' > a.rs
    git add a.rs
    git -c user.email=b@b -c user.name=b commit -q -m init
    printf 'fn main() { println!("hi"); }\nfn helper() {}\n' > a.rs
    printf 'pub struct New {}\n' > b.rs
)

# run_scenario <name> <pre-cmd-or-empty> <hitagi-args...>
run_scenario() {
    local name="$1"
    shift
    local pre="$1"
    shift
    local cmd_repo="$1"
    shift
    local args=("$@")

    local stat_file
    stat_file="$(mktemp)"
    local out_file
    out_file="$(mktemp)"

    local times=()
    local rsses=()
    local bytes=()
    local rc

    for ((i = 0; i < RUNS; i++)); do
        if [[ -n "$pre" ]]; then
            eval "$pre"
        fi
        : > "$stat_file"
        : > "$out_file"
        # %e = elapsed seconds (wall), %M = max RSS in kB.
        if /usr/bin/time -f '%e %M' -o "$stat_file" -- \
            "$ABS_BIN" --repo "$cmd_repo" "${args[@]}" \
            > "$out_file" 2> "$out_file.err"; then
            rc=0
        else
            rc=$?
            echo "warn: scenario $name run $i exit=$rc:" >&2
            head -3 "$out_file.err" >&2 || true
        fi
        local elapsed rss
        read -r elapsed rss < "$stat_file" || true
        if [[ -z "${elapsed:-}" ]]; then
            elapsed="0.00"
            rss="0"
        fi
        local size
        size=$(wc -c < "$out_file")
        times+=("$elapsed")
        rsses+=("$rss")
        bytes+=("$size")
    done
    rm -f "$out_file.err"

    rm -f "$stat_file" "$out_file"

    # Emit a tiny JSON object: scenario name, ordered samples per metric.
    {
        printf '{\n'
        printf '  "scenario": "%s",\n' "$name"
        printf '  "runs": %d,\n' "$RUNS"
        printf '  "elapsed_secs": ['
        for i in "${!times[@]}"; do
            [[ $i -gt 0 ]] && printf ', '
            printf '%s' "${times[$i]}"
        done
        printf '],\n  "max_rss_kb": ['
        for i in "${!rsses[@]}"; do
            [[ $i -gt 0 ]] && printf ', '
            printf '%s' "${rsses[$i]}"
        done
        printf '],\n  "stdout_bytes": ['
        for i in "${!bytes[@]}"; do
            [[ $i -gt 0 ]] && printf ', '
            printf '%s' "${bytes[$i]}"
        done
        printf ']\n}\n'
    } > "$OUT/$name.json"

    # Compact summary line for the operator's terminal.
    local sum_t=0 sum_r=0 sum_b=0
    for t in "${times[@]}"; do sum_t=$(awk "BEGIN{print $sum_t + $t}"); done
    for r in "${rsses[@]}"; do sum_r=$((sum_r + r)); done
    for b in "${bytes[@]}"; do sum_b=$((sum_b + b)); done
    local mean_t mean_r mean_b
    mean_t=$(awk "BEGIN{printf \"%.4f\", $sum_t / $RUNS}")
    mean_r=$(awk "BEGIN{printf \"%d\", $sum_r / $RUNS}")
    mean_b=$(awk "BEGIN{printf \"%d\", $sum_b / $RUNS}")
    printf '  %-28s  mean: %ss  rss: %s kB  bytes: %s\n' \
        "$name" "$mean_t" "$mean_r" "$mean_b"
}

# A per-scenario tempdir for cache redirection; avoids polluting the
# user's real ~/.cache/hitagi.
COLD_CACHE="$(mktemp -d -t hitagi-bench-cold-XXXXXX)"
WARM_CACHE="$(mktemp -d -t hitagi-bench-warm-XXXXXX)"
trap 'rm -rf "$DIFF_REPO" "$COLD_CACHE" "$WARM_CACHE"' EXIT

# Pre-warm the warm cache by running each search mode once.
export HITAGI_CACHE_DIR="$WARM_CACHE"
"$ABS_BIN" --repo "$ABS_CORPUS" search "config" --mode bm25 > /dev/null 2>&1 || true

# Detect if the embedding model is available locally; only run hybrid/semantic
# scenarios if it is, so benches don't depend on network.
HAS_MODEL=false
if "$ABS_BIN" --repo "$ABS_CORPUS" --json search "config" --mode hybrid --no-download > /dev/null 2>&1; then
    MODEL_STATUS="$("$ABS_BIN" --repo "$ABS_CORPUS" --json index status 2>/dev/null || true)"
    if [[ "$MODEL_STATUS" == *'"encoder_kind"'*':'*'"model2vec"'* ]]; then
        HAS_MODEL=true
    fi
fi

echo "running scenarios into $OUT (RUNS=$RUNS, model=$HAS_MODEL)" >&2

# Warm-cache scenarios:
run_scenario "warm_search_bm25" \
    "" \
    "$ABS_CORPUS" \
    --json search "config schema" --mode bm25

run_scenario "find_full" \
    "" \
    "$ABS_CORPUS" \
    --json find main --limit 200

run_scenario "outline_one" \
    "" \
    "$ABS_CORPUS" \
    --json outline apps/desktop/src-tauri/src/main.rs

run_scenario "diff_overview" \
    "" \
    "$DIFF_REPO" \
    diff

if $HAS_MODEL; then
    # Use --no-download so a missing model fails fast rather than hitting HF.
    run_scenario "warm_search_semantic" \
        "" \
        "$ABS_CORPUS" \
        --json search "config schema" --mode semantic --no-download

    run_scenario "warm_search_hybrid" \
        "" \
        "$ABS_CORPUS" \
        --json search "config schema" --mode hybrid --no-download

    run_scenario "cold_search_hybrid" \
        "rm -rf \"$COLD_CACHE\"; mkdir -p \"$COLD_CACHE\"; export HITAGI_CACHE_DIR=\"$COLD_CACHE\"" \
        "$ABS_CORPUS" \
        --json search "config schema" --mode hybrid --no-download
else
    echo "skipping hybrid/semantic scenarios (model not cached locally)" >&2
fi

echo "wrote results into $OUT" >&2
