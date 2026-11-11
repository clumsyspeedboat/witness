#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

run_dir=${WITNESS_QUERY_OPTIMIZATION_RESULT_DIR:-experiments/results/query_optimization/$(date -u +%Y%m%dT%H%M%SZ)}
mkdir -p "$run_dir"
exec 9>/tmp/witness-evidence-layout.lock
flock -n 9 || { printf 'Another Witness diagnostic holds the measurement lock.\n' >&2; exit 1; }
if [[ -e "$run_dir/freeze.csv" || -e "$run_dir/PROVENANCE.txt" ]]; then
  printf 'Refusing to reuse diagnostic directory: %s\n' "$run_dir" >&2
  exit 1
fi
study_cpu=${WITNESS_QUERY_OPTIMIZATION_CPU:-$(python3 -c 'import os; print(min(os.sched_getaffinity(0)))')}
export WITNESS_QUERY_OPTIMIZATION_RESULT_DIR="$run_dir"
{
  printf 'generated_utc=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf 'git_commit=%s\n' "$(git rev-parse HEAD)"
  printf 'git_branch=%s\n' "$(git branch --show-current)"
  printf 'cpu_affinity=%s\n' "$study_cpu"
  printf 'timing_scope=resident byte buffers; complete initialization charged in total timings; shared host\n'
  rustc --version
  sha256sum Cargo.lock INPUTS.sha256 src/bin/query_optimization_study.rs \
    experiments/run_query_optimization.sh experiments/summarize_query_optimization.py
  rg --files src/access_compiler | sort | xargs sha256sum
} > "$run_dir/PROVENANCE.txt"
CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-4} cargo build --locked --release --features experiment --bin query_optimization_study
build_dir=$(cargo metadata --locked --format-version 1 --no-deps | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')
study_binary="$build_dir/release/query_optimization_study"
sha256sum "$study_binary" >> "$run_dir/PROVENANCE.txt"
taskset -c "$study_cpu" "$study_binary" "$@" 2>&1 | tee "$run_dir/run.log"
python3 experiments/summarize_query_optimization.py "$run_dir"
sha256sum "$run_dir"/*.csv "$run_dir"/summary.json "$run_dir"/summary.md > "$run_dir/CHECKSUMS.sha256"
printf '\nResults: %s\n' "$run_dir"
