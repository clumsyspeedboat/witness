# Validation — 25 October 2026

- `cargo test --locked --all-features --no-fail-fast`: 205 passed, zero failed, 45 target summaries.
- `python3 -m unittest discover -s tests -p 'test_*.py'`: 9 passed.
- `cargo clippy --locked --all-targets --all-features -- -D warnings`: passed.
- `cargo fmt --all -- --check`: passed.
- Shell syntax, Python compilation, and `git diff --check`: passed.
- Smoke pipeline: 288 timing cells; exact SUM/count checks passed.
- Real-data pilot: 1,824 timing cells over 76 predicates and 24 sources; exact SUM/count checks passed.
- Independently recomputed held-out byte and latency ratios match the exported summary.
- Output-directory reuse is rejected before overwriting provenance or measurements.
- Timed benchmark and recipe-transformer hashes match their premeasurement provenance.
- The core rule fingerprint is unchanged (`a3706a5cdcb90cf6`).

The frozen gate rejects both candidates, so no broad rerun or manuscript rewrite is warranted by this pilot. The implementation remains an opt-in experimental diagnostic; canonical menus and claim-bearing CSVs are unchanged.

Canonical claim manifest SHA-256: `001828d4155eec646dd6f36d03bfa0cd8a5eb4e15bee0fd8f55821648b45d296`.
Local manuscript PDF SHA-256: `e062c21a64cd587f7b3944c2d913127a34da121027e5cd1bc9566e0ae9ba2ee7`.
