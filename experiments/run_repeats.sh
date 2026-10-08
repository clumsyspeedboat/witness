#!/usr/bin/env bash
# Rerun the timed paged studies and their DuckDB baselines twice more, so the
# claim manifest can report how far each headline moves between runs.
set -euo pipefail

cd "$(dirname "$0")/.."
for run in 2 3; do
  dir=experiments/results/repeats/run$run
  mkdir -p "$dir"
  printf 'repeat %s of the timed studies\n' "$run"
  WITNESS_PAGED_RESULT_DIR="$dir/paged_study" \
    cargo run --locked --release --features experiment --bin paged_study
  WITNESS_PAGED_SCALE_RESULT_DIR="$dir/paged_scale" \
    cargo run --locked --release --features experiment --bin paged_scale_study
  WITNESS_PAGED_CONJUNCTIVE_RESULT_DIR="$dir/paged_conjunctive" \
    cargo run --locked --release --features experiment --bin paged_conjunctive_study
  WITNESS_PAGED_RESULT_DIR="$dir/paged_study" \
    WITNESS_PAGED_SCALE_RESULT_DIR="$dir/paged_scale" \
    WITNESS_PAGED_CONJUNCTIVE_RESULT_DIR="$dir/paged_conjunctive" \
    WITNESS_DUCKDB_RESULT_DIR="$dir/duckdb" \
    python3 experiments/duckdb_baseline.py paged scale conjunctive
  # The exported Parquet files are inputs to DuckDB, not results.
  rm -rf "$dir"/paged_study/parquet "$dir"/paged_scale/parquet "$dir"/paged_conjunctive/parquet
done
