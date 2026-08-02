#!/bin/sh
set -eu

cd "$(dirname "$0")/.."

WITNESS_MAX_ROWS=16384 \
WITNESS_PREDICATE_RESULT_DIR=experiments/results/predicate_pipeline_rows16k \
    ./experiments/run_predicate_pipeline.sh

WITNESS_MAX_ROWS=131072 \
WITNESS_PREDICATE_RESULT_DIR=experiments/results/predicate_pipeline \
    ./experiments/run_predicate_pipeline.sh

small=$(awk -F, 'NR == 2 { print $1 }' experiments/results/predicate_pipeline_rows16k/freeze.csv)
large=$(awk -F, 'NR == 2 { print $1 }' experiments/results/predicate_pipeline/freeze.csv)
test "$small" = "$large"

echo "paired predicate scale runs passed at fingerprint $large"
