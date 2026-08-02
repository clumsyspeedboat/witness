#!/bin/sh
set -eu

cd "$(dirname "$0")/.."
cargo fmt --all -- --check
cargo run --release --features experiment --bin generate_access_compiler

times=$(mktemp)
trap 'rm -f "$times"' EXIT
for _ in 1 2 3; do
    cargo clean -p witness --release >/dev/null
    start=$(date +%s%N)
    cargo build --release --features experiment --bin access_compiler_study >/dev/null
    end=$(date +%s%N)
    echo $((end - start)) >> "$times"
done

cargo test --release --features experiment \
    --test access_compiler_ir --test access_compiler_exec --test access_compiler_generated
cargo clippy --release --features experiment --all-targets -- -D warnings
cargo run --release --features experiment --bin access_compiler_study

set -- $(sort -n "$times")
p25=$1
median=$2
p75=$3
printf 'rust_crate_compilation,all,all,%s,%s,%s\n' "$p25" "$median" "$p75" \
    >> experiments/results/access_compiler/compile_costs.csv

summary=experiments/results/access_compiler/summary.csv
test "$(wc -l < experiments/results/access_compiler/benchmark.csv)" -eq 5587
test "$(wc -l < "$summary")" -eq 799
test "$(wc -l < experiments/results/access_compiler/compile_costs.csv)" -eq 136
test "$(find experiments/results/access_compiler/pages -name '*.acp' | wc -l)" -eq 19

awk -F, 'NR > 1 && $4 == "generated" { n++; if ($8 > 1.5 || $11 != $12) exit 1 }
    END { if (n != 133) exit 1 }' "$summary"
awk -F, 'NR > 1 { k=$1 SUBSEP $3;
    if ($4 == "generated") { g1[k]=$10; g2[k]=$11; g3[k]=$12 }
    if ($4 == "handwritten_monomorphized") { h1[k]=$10; h2[k]=$11; h3[k]=$12 } }
    END { for (k in g1) {
        split(k, parts, SUBSEP);
        if (g1[k] > h1[k] || g2[k] > h2[k] || g3[k] > h3[k]) exit 1
    }}' "$summary"
awk -F, 'NR > 1 && $4 == "generated" && $2 ~ /Frame/ { n++; if ($14 != 1) exit 1 }
    END { if (n != 35) exit 1 }' "$summary"
awk -F, 'NR > 1 && $1 == 17 && $3 ~ /^FILTER_/ {
    k=$3; if ($4 == "generated") g[k]=$10; if ($4 == "fused_decode") f[k]=$10 }
    END { for (k in g) if (g[k] < f[k]) improved++; if (improved != 2) exit 1 }' "$summary"
! grep -Eq 'DecoderNode|column\.decoder' experiments/generated/access_compiler/generated.rs

echo "access-compiler gates passed: 19 compositions, 133 query cells"
