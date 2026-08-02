#!/bin/sh
set -eu

cd "$(dirname "$0")/.."
result=${WITNESS_PREDICATE_RESULT_DIR:-experiments/results/predicate_pipeline}

cargo fmt --all -- --check
cargo run --release --features experiment --bin generate_access_compiler
cargo run --release --features experiment --bin generate_predicate_access
# The claim_bearing_ tests compare docs/ and paper/main.tex against the claim
# manifest that this run is about to regenerate, so they cannot pass before the
# numbers exist. They are documentation-currency checks rather than
# code-correctness ones, and run in the full suite (`make test`, CI) instead of
# in this measurement gate.
cargo test --release --all-features -- --skip claim_bearing_
cargo clippy --release --features experiment --all-targets -- -D warnings
cargo run --release --features experiment --bin predicate_pipeline_study

predicates=$(($(wc -l < "$result/predicates.csv") - 1))
pairs=$(($(wc -l < "$result/pairs.csv") - 1))
logical_columns=$(($(wc -l < "$result/columns.csv") - 1))
kernels=$(awk -F, 'NR == 2 { print $2 }' "$result/freeze.csv")
test "$pairs" -ge 30
test "$logical_columns" -ge 60
test "$kernels" -eq $((logical_columns * 2))
test "$(find "$result/pages" -name '*.acp' | wc -l)" -eq "$kernels"
test "$(find "$result/parquet" -name 'pair_*.parquet' | wc -l)" -eq $((pairs * 3))
test "$(wc -l < "$result/known_selection.csv")" -eq $((predicates * 4 + 1))
test "$(wc -l < "$result/complete_query.csv")" -eq $((predicates * 10 + 1))
test "$(wc -l < "$result/diagnostics.csv")" -eq $((predicates + 1))
test "$(wc -l < "$result/source_summary.csv")" -ge 26
test "$(wc -l < "$result/certificate_summary.csv")" -eq 3

awk -F, 'NR > 1 && $9 ~ /Frame/ { exit 1 }' "$result/columns.csv"
awk -F, 'NR > 1 { source[$1 SUBSEP $3 SUBSEP $4]=1; certificate[$5]=1 }
    END { if (length(source) < 25 || length(certificate) != 2) exit 1 }' "$result/source_summary.csv"
awk -F, 'NR > 1 { baseline[$3]=1 }
    END { if (!("generated_direct_selective" in baseline) ||
              !("parquet_boundary_search" in baseline) ||
              !("parquet_boundary_search_p4096" in baseline) ||
              !("parquet_boundary_search_p16384" in baseline)) exit 1 }' "$result/complete_query.csv"

awk -F, 'NR > 1 { k=$1 SUBSEP $2; if (!(k in sum)) { sum[k]=$7; rows[k]=$8 }
    if (sum[k] != $7 || rows[k] != $8) exit 1; count[k]++ }
    END { for (k in count) if (count[k] != 4) exit 1 }' "$result/known_selection.csv"
awk -F, 'NR > 1 { k=$1 SUBSEP $2; if (!(k in sum)) { sum[k]=$7; rows[k]=$8 }
    if (sum[k] != $7 || rows[k] != $8) exit 1; count[k]++ }
    END { for (k in count) if (count[k] != 10) exit 1 }' "$result/complete_query.csv"

{
    rustc --version
    cargo --version
    uname -a
    lscpu | grep -E 'Model name|CPU\(s\):|L3 cache'
    findmnt -T "$PWD" -o TARGET,SOURCE,FSTYPE,OPTIONS
    lsblk -d -o NAME,ROTA,TYPE,SIZE,MODEL
} > "$result/environment.txt"

echo "predicate-pipeline gates passed: $pairs pairs, $predicates distinct predicate cells"
