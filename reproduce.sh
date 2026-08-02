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

(cd "$DATA" && sha256sum --quiet --check "$ROOT/INPUTS.sha256") || {
  printf 'Corpus input does not match INPUTS.sha256; refusing to measure.\n' >&2
  exit 1
}

PAIRED_16K=experiments/results/predicate_pipeline_rows16k
RESULTS=experiments/results/predicate_pipeline

printf '1/7 invariant census\n'
./experiments/run_invariant_census.sh

printf '2/7 paired predicate pipeline (16Ki and 128Ki rows)\n'
./experiments/run_predicate_scale.sh

printf '3/7 membership-certificate controls\n'
./experiments/run_certificate_study.sh

printf '4/7 physical access and storage schedules\n'
./experiments/run_real_access.sh

printf '5/7 auxiliary plans and claim manifest\n'
cargo run --locked --release --features experiment --bin claim_manifest -- --measure-additional

printf '6/7 numerical walkthrough and physical artifacts\n'
cargo run --locked --release --features experiment --bin generate_documentation_example
cargo run --locked --release --features experiment --bin documentation_example

# The last two entries are tooling rather than evidence: the JSON bridge and the
# companion notebook produce no claim. They are covered because the notebook
# demonstrates the compiler's behaviour next to the paper's prose, so a silent
# drift in either could show a reader something the manuscript does not say.
printf '7/7 provenance and checksums\n'
{
  printf 'generated_utc='; date -u +%Y-%m-%dT%H:%M:%SZ
  printf 'system='; uname -a
  printf 'rustc='; rustc --version
  printf 'cargo='; cargo --version
  printf 'python='; python3 --version
  printf 'rows_per_column=131072\nscale_reference_rows=16384\n'
  printf 'timing_scope=resident buffers plus cold XFS mechanism control; single-threaded\n'
} > "$RESULTS/PROVENANCE.txt"
{
  for file in "${required[@]}"; do sha256sum "$DATA/$file"; done
  sha256sum "$RESULTS"/*.csv \
    experiments/results/invariant_census/*.csv \
    experiments/results/certificate_study/*.csv \
    experiments/results/real_access/*.csv \
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
