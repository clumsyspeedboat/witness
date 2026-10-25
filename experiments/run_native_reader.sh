#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

run_dir=${WITNESS_NATIVE_RESULT_DIR:-experiments/results/native_reader/$(date -u +%Y%m%dT%H%M%SZ)}
mkdir -p "$run_dir"
exec 9>/tmp/witness-evidence-layout.lock
flock -n 9 || { printf 'Another Witness diagnostic holds the measurement lock.\n' >&2; exit 1; }
if [[ -e "$run_dir/freeze.csv" || -e "$run_dir/PROVENANCE.txt" ]]; then
  printf 'Refusing to reuse diagnostic directory: %s\n' "$run_dir" >&2
  exit 1
fi
study_cpu=${WITNESS_NATIVE_CPU:-$(python3 -c 'import os; print(min(os.sched_getaffinity(0)))')}
export WITNESS_NATIVE_RESULT_DIR="$run_dir"
{
  printf 'generated_utc=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf 'git_commit=%s\n' "$(git rev-parse HEAD)"
  printf 'git_branch=%s\n' "$(git branch --show-current)"
  printf 'cpu_affinity=%s\n' "$study_cpu"
  printf 'timing_scope=resident; initialization measured separately; shared host\n'
  rustc --version
  sha256sum Cargo.lock src/access_compiler/native_file.rs src/access_compiler/paged.rs \
    src/access_compiler/decoder.rs src/access_compiler/encode.rs src/bin/native_reader_study.rs
} > "$run_dir/PROVENANCE.txt"
CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-4} cargo build --locked --release --features experiment --bin native_reader_study
build_dir=$(cargo metadata --locked --format-version 1 --no-deps | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')
study_binary="$build_dir/release/native_reader_study"
sha256sum "$study_binary" >> "$run_dir/PROVENANCE.txt"
taskset -c "$study_cpu" "$study_binary" 2>&1 | tee "$run_dir/run.log"
python3 experiments/summarize_native_reader.py "$run_dir"
printf '\nResults: %s\n' "$run_dir"
