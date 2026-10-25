#!/usr/bin/env bash
# Isolated diagnostic; never overwrites canonical measurements.
set -euo pipefail
cd "$(dirname "$0")/.."

mode=pilot
if [[ ${1:-} == --smoke ]]; then mode=smoke; fi
if [[ ${1:-} == --all ]]; then mode=all; fi
run_dir=${WITNESS_EVIDENCE_LAYOUT_RESULT_DIR:-experiments/results/evidence_layout/$(date -u +%Y%m%dT%H%M%SZ)-$mode}
mkdir -p "$run_dir"
export WITNESS_EVIDENCE_LAYOUT_RESULT_DIR="$run_dir"

# Serialize our own study invocations. Affinity fixes the query worker to one
# CPU; it does not claim exclusive access against unrelated host processes.
exec 9>/tmp/witness-evidence-layout.lock
flock -n 9 || { printf 'An evidence-layout study already holds the lock.\n' >&2; exit 1; }
if [[ -e "$run_dir/freeze.csv" || -e "$run_dir/PROVENANCE.txt" ]]; then
  printf 'Refusing to reuse an existing diagnostic directory: %s\n' "$run_dir" >&2
  exit 1
fi
study_cpu=${WITNESS_EVIDENCE_CPU:-$(python3 -c 'import os; print(min(os.sched_getaffinity(0)))')}
{
  printf 'generated_utc=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf 'git_commit=%s\n' "$(git rev-parse HEAD)"
  printf 'git_branch=%s\n' "$(git branch --show-current)"
  printf 'cpu_affinity=%s\n' "$study_cpu"
  printf 'timing_scope=resident; single-threaded; OS scheduling is shared\n'
  rustc --version
  uname -a
  git status --short
  sha256sum Cargo.lock INPUTS.sha256 src/experiment/evidence_layout.rs \
    src/bin/evidence_layout_study.rs src/bin/evidence_layout/measure.rs \
    src/bin/evidence_layout/README.md experiments/summarize_evidence_layout.py
} > "$run_dir/PROVENANCE.txt"
CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-4} cargo build --locked --release --features experiment --bin evidence_layout_study
build_dir=$(cargo metadata --locked --format-version 1 --no-deps | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')
study_binary="$build_dir/release/evidence_layout_study"
sha256sum "$study_binary" >> "$run_dir/PROVENANCE.txt"
taskset -c "$study_cpu" "$study_binary" "$@" 2>&1 | tee "$run_dir/run.log"
sha256sum experiments/summarize_evidence_layout.py > "$run_dir/ANALYSIS.sha256"
python3 experiments/summarize_evidence_layout.py "$run_dir"
printf '\nResults: %s\n' "$run_dir"
