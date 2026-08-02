# Witness: build, test, and reproduce the study.
.PHONY: build test core-test fetch-study claims walkthrough diagnostics reproduce

build:
	cargo build --locked --release --features experiment

test:
	cargo test --locked --all-features

core-test:
	cargo test --locked

fetch-study:
	./experiments/fetch_nab_real.sh
	./experiments/eval/scripts/fetch_data.sh
	python3 experiments/eval/scripts/fetch_bench_data.py

claims:
	cargo run --locked --release --features experiment --bin claim_manifest

walkthrough: claims
	cargo run --locked --release --features experiment --bin generate_documentation_example
	cargo run --locked --release --features experiment --bin documentation_example

diagnostics:
	./experiments/run_access_compiler.sh
	./experiments/run_access_crossover.sh

reproduce:
	./reproduce.sh
