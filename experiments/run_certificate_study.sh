#!/bin/sh
set -eu

cd "$(dirname "$0")/.."
cargo fmt --all -- --check
cargo test --release --all-features --test certificate_plans
cargo clippy --release --all-features --all-targets -- -D warnings
cargo run --release --features experiment --bin certificate_study

result=experiments/results/certificate_study
test "$(($(wc -l < "$result/cells.csv") - 1))" -ge 1700
awk -F, 'NR > 1 && $5 != "scan" && $8 > $6 { exit 1 }' "$result/cells.csv"
awk -F, 'NR > 1 { plan[$1]=1; query[$2]=1 }
    END { if (length(plan) != 5 || length(query) != 4) exit 1 }' "$result/summary.csv"

echo "certificate study gates passed"
