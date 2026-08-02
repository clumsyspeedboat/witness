#!/bin/sh
set -eu

cd "$(dirname "$0")/.."
result=experiments/results/invariant_census

cargo fmt --all -- --check
cargo test --release --features experiment --test invariant_calculus
cargo clippy --release --features experiment --all-targets -- -D warnings
cargo run --release --features experiment --bin invariant_census_study

columns=$(($(wc -l < "$result/columns.csv") - 1))
sources=$(($(wc -l < "$result/sources.csv") - 1))
candidates=$(($(wc -l < "$result/candidates.csv") - 1))
test "$columns" -ge 100
test "$sources" -ge 30
test "$candidates" -ge $((columns * 8))
awk -F, 'NR > 1 && $8 == "true" && $16 == "" { exit 1 }' "$result/columns.csv"
awk -F, 'NR > 1 && $4 == "true" { structural[$1]=1 }
    END { if (length(structural) < 20) exit 1 }' "$result/candidates.csv"
awk -F, 'NR > 1 { metric[$1]=$2 }
    END { if (metric["columns"] < 100 || metric["sources"] < 30 ||
              metric["global_monotone_column_fraction"] <= 0 ||
              metric["page_1024_monotone_fraction"] <= 0) exit 1 }' "$result/summary.csv"

echo "invariant-census gates passed: $columns columns, $sources sources, $candidates candidates"
