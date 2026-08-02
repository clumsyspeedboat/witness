#!/bin/sh
set -eu

cd "$(dirname "$0")/.."
result=experiments/results/access_crossover

cargo fmt --all -- --check
cargo run --features experiment --bin generate_access_compiler
cargo test --release --features experiment \
    --test access_compiler_ir --test access_compiler_exec --test access_compiler_generated
cargo clippy --release --features experiment --all-targets -- -D warnings
cargo run --release --features experiment --bin access_crossover_study

test "$(wc -l < "$result/curve.csv")" -eq 4081
test "$(wc -l < "$result/crossovers.csv")" -eq 121
test "$(wc -l < "$result/model_eval.csv")" -eq 1361
test "$(wc -l < "$result/model_summary.csv")" -eq 17
test "$(wc -l < "$result/policy_summary.csv")" -eq 49
test "$(wc -l < "$result/scan.csv")" -eq 13
test "$(stat -c %s "$result/scan_bundle.acp")" -ge 134000000

awk -F, 'NR > 1 && $1 == "preflight_free_estimate" {
    if ($5 < 0.75 || $6 > 1.05 || $8 > 2.0) exit 1; n++
} END { if (n != 8) exit 1 }' "$result/model_summary.csv"

awk -F, 'NR > 1 && $1 == "preflight_free_estimate" {
    cells[$2]+=$5; regret[$2]+=$5*$7
} END {
    if (cells["cost_model"] != 680 || cells["always_selective"] != 680) exit 1
    if (regret["cost_model"]/cells["cost_model"] > regret["always_selective"]/cells["always_selective"]) exit 1
}' "$result/policy_summary.csv"

awk -F, 'NR > 1 {
    if (!seen) { checksum=$19; seen=1 }
    if ($19 != checksum) exit 1
    if ($2 == "always_fused") fused[$1]=$11
    if ($2 == "cost_model_preflight_free") model[$1]=$11
} END {
    if (length(fused) != 3 || length(model) != 3) exit 1
    for (tier in fused) if (model[tier] >= fused[tier]) exit 1
}' "$result/scan.csv"

{
    rustc --version
    cargo --version
    uname -a
    lscpu | grep -E 'Model name|CPU\(s\):|L3 cache'
    df -T "$result" | tail -n 1
} > "$result/environment.txt"

echo "access-crossover gates passed: 4080 curve cells, 680 held-out decisions, 12 scans"
