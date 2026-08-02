#!/bin/sh
set -eu

cd "$(dirname "$0")/.."
result=experiments/results/real_access

cargo fmt --all -- --check
cargo run --release --features experiment --bin generate_real_access
# See run_predicate_pipeline.sh: the claim_bearing_ tests are documentation
# currency checks against a manifest this run regenerates, not gate conditions.
cargo test --release --all-features -- --skip claim_bearing_
cargo clippy --release --features experiment --all-targets -- -D warnings
cargo run --release --features experiment --bin real_access_study

test "$(wc -l < "$result/selection.csv")" -eq 15
test "$(wc -l < "$result/candidates.csv")" -eq 179
test "$(wc -l < "$result/queries.csv")" -eq 337
test "$(wc -l < "$result/query_distribution.csv")" -eq 169
test "$(wc -l < "$result/storage_scan.csv")" -eq 73
test "$(find "$result/pages" -maxdepth 1 -name '*.acp' | wc -l)" -eq 14

awk -F, 'NR > 1 {
    recipes[$7]=1; columns[$1]=1
} END {
    if (length(columns) != 14 || length(recipes) < 4) exit 1
}' "$result/selection.csv"

awk -F, 'NR > 1 {
    if ($8 == "1.00000000" && $10 != "broad_fused") exit 1
    if ($8 != "1.00000000" && $10 != "selective_closure") exit 1
    cells++
} END { if (cells != 336) exit 1 }' "$result/queries.csv"

awk -F, 'NR > 1 {
    if (!seen) { checksum=$22; seen=1 }
    if ($22 != checksum) exit 1
    storage[$1]=1; policy[$5]=1
    if ($4 == "cold" && $2 != "tmpfs" && $17 == 0) exit 1
    if ($2 == "tmpfs" && $17 != 0) exit 1
} END {
    if (length(storage) != 3 || length(policy) != 6) exit 1
}' "$result/storage_scan.csv"

{
    rustc --version
    cargo --version
    uname -a
    lscpu | grep -E 'Model name|CPU\(s\):|L3 cache'
    findmnt -T "$PWD" -o TARGET,SOURCE,FSTYPE,OPTIONS
    findmnt -T /tmp -o TARGET,SOURCE,FSTYPE,OPTIONS
    findmnt -T /dev/shm -o TARGET,SOURCE,FSTYPE,OPTIONS
    lsblk -d -o NAME,ROTA,TYPE,SIZE,MODEL
} > "$result/environment.txt"

echo "real-access gates passed: 14 columns, 178 candidates, 336 query cells, 72 storage scans"
