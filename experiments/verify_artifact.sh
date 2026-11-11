#!/usr/bin/env bash
# Reviewer validation: no corpus downloads, no remeasurement, no paper dependency.
set -euo pipefail
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2}
cd "$(dirname "$0")/.."
for tool in cargo python3 make sha256sum; do
  command -v "$tool" >/dev/null || { printf 'Missing command: %s\n' "$tool" >&2; exit 1; }
done
cargo fmt --all -- --check
cargo clippy --locked --release --all-targets --all-features -- -D warnings
cargo test --locked --release --all-features
python3 -m unittest discover -s tests -p 'test_*.py'
expected_claims=$(sha256sum experiments/results/claim_manifest.csv | cut -d' ' -f1)
make --no-print-directory claims
actual_claims=$(sha256sum experiments/results/claim_manifest.csv | cut -d' ' -f1)
if [[ "$expected_claims" != "$actual_claims" ]]; then
  printf 'Claim manifest differs from committed evidence; inspect the diff.\n' >&2
  exit 1
fi
printf '\nVerified code, tests, and exact claim reconstruction without downloading corpora.\n'
