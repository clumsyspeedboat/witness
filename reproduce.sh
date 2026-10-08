#!/usr/bin/env bash
# Rebuild every claim-bearing result from pinned corpus slices.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"

need() {
  command -v "$1" >/dev/null 2>&1 || {
    printf 'Missing required command: %s\n' "$1" >&2
    exit 1
  }
}
for command in cargo rustc make python3 sha256sum; do
  need "$command"
done
python3 -c 'import duckdb' 2>/dev/null || {
  printf 'Missing Python package: duckdb (pip install -r experiments/requirements.txt)\n' >&2
  exit 1
}

DATA=experiments/eval/data
make --no-print-directory fetch-study

required=(
  realKnownCause__nyc_taxi.csv
  realKnownCause__ambient_temperature_system_failure.csv
  realTweets__Twitter_volume_AAPL.csv
  household_power_consumption.txt
  tpch/lineitem__c0_l_orderkey.csv
  tpch/lineitem__c3_l_linenumber.csv
  tpch/lineitem__c4_l_quantity.csv
  tpch/lineitem__c8_timestamp.csv
  clickbench/hits__c0_timestamp.csv
  clickbench/hits__c1_CounterID.csv
  clickbench/hits__c2_RegionID.csv
  clickbench/hits__c3_ResolutionWidth.csv
  publicbi/Bimbo__c0_Agencia_ID.csv
  publicbi/Bimbo__c2_Cliente_ID.csv
  publicbi/Bimbo__c3_Demanda_uni_equil.csv
  yellow_tripdata_2024-01.parquet
)
for file in "${required[@]}"; do
  test -f "$DATA/$file" || {
    printf 'Missing study input: %s\n' "$DATA/$file" >&2
    exit 1
  }
done

python3 experiments/eval/scripts/verify_inputs.py

PAIRED_16K=experiments/results/predicate_pipeline_rows16k
RESULTS=experiments/results/predicate_pipeline

printf '1/8 invariant census\n'
./experiments/run_invariant_census.sh

printf '2/8 paired predicate pipeline (16Ki and 128Ki rows)\n'
./experiments/run_predicate_scale.sh

printf '3/8 membership-certificate controls\n'
./experiments/run_certificate_study.sh

printf '4/8 physical access and storage schedules\n'
./experiments/run_real_access.sh

printf '5/8 paged queries, page sizes, scaling, conjunctions, floats, strings, cold reads, Parquet facts, DuckDB, and repeats\n'
cargo run --locked --release --features experiment --bin paged_study
for rows in 4096 65536; do
  WITNESS_PAGE_ROWS=$rows cargo run --locked --release --features experiment --bin paged_study
done
cargo run --locked --release --features experiment --bin paged_scale_study
cargo run --locked --release --features experiment --bin paged_conjunctive_study
cargo run --locked --release --features experiment --bin parquet_facts_study
cargo run --locked --release --features experiment --bin paged_float_study
cargo run --locked --release --features experiment --bin paged_string_study
cargo run --locked --release --features experiment --bin paged_cold_study
python3 experiments/duckdb_baseline.py
./experiments/run_repeats.sh

printf '6/8 auxiliary plans and claim manifest\n'
cargo run --locked --release --features experiment --bin claim_manifest -- --measure-additional
python3 experiments/render_paper_claims.py

printf '7/8 numerical walkthrough and physical artifacts\n'
cargo run --locked --release --features experiment --bin generate_documentation_example
cargo run --locked --release --features experiment --bin documentation_example

# The last two entries are tooling rather than evidence: the JSON bridge and the
# companion notebook produce no claim. They are covered because the notebook
# demonstrates the compiler's behaviour next to the paper's prose, so a silent
# drift in either could show a reader something the manuscript does not say.
printf '8/8 provenance and checksums\n'
{
  printf 'generated_utc='; date -u +%Y-%m-%dT%H:%M:%SZ
  printf 'system='; uname -a
  printf 'rustc='; rustc --version
  printf 'cargo='; cargo --version
  printf 'python='; python3 --version
  printf 'duckdb='; python3 -c 'import duckdb; print(duckdb.__version__)'
  printf 'rows_per_column=131072\nscale_reference_rows=16384\n'
  printf 'timing_scope=resident buffers plus cold XFS mechanism control; single-threaded except paged scaling (1-64 threads)\n'
} > "$RESULTS/PROVENANCE.txt"
{
  # These hashes were checked before measurement; preserve the complete
  # inventory here with paths relative to the repository root.
  sed -n "s@  @  $DATA/@p" INPUTS.sha256
  sha256sum "$RESULTS"/*.csv \
    experiments/results/invariant_census/*.csv \
    experiments/results/certificate_study/*.csv \
    experiments/results/real_access/*.csv \
    experiments/results/paged_study/*.csv \
    experiments/results/paged_study_rows*/*.csv \
    experiments/results/paged_scale/*.csv \
    experiments/results/duckdb/*.csv \
    experiments/results/paged_conjunctive/*.csv \
    experiments/results/parquet_facts/*.csv \
    experiments/results/paged_float/*.csv \
    experiments/results/paged_string/*.csv \
    experiments/results/paged_cold/*.csv \
    experiments/results/repeats/*/*/*.csv \
    "$PAIRED_16K"/*.csv \
    experiments/results/additional_plans.csv \
    experiments/results/claim_manifest.csv \
    docs/generated/*.csv \
    docs/generated/END_TO_END_EXAMPLE.md \
    docs/generated/artifacts/* \
    experiments/generated/documentation_example/generated.rs \
    src/bin/witness_explore.rs \
    notebooks/witness_paper_companion.ipynb
} > "$RESULTS/CHECKSUMS.sha256"

printf 'Complete: canonical results, claim manifest, and walkthrough regenerated.\n'
