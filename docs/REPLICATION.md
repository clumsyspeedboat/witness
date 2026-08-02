# Replication

Witness separates exact reconstruction of reported values from host-dependent
remeasurement.

The test oracles, adversarial cases, and per-study gates are detailed in
[Testing and Validation](TESTING.md).

## Exact reported values

The repository commits the canonical CSV evidence. Regenerate the complete
claim-to-value mapping without downloading data:

```bash
make claims
git diff --exit-code experiments/results/claim_manifest.csv
```

`claim_manifest.csv` contains every scalar, table row, and plot coordinate
derived for the submitted study. The generator reads only canonical CSVs and
uses a fixed bootstrap seed.

## Full remeasurement

```bash
./reproduce.sh
```

This one command fetches all corpus inputs, verifies `INPUTS.sha256`, applies
the validation gates, reruns every claim-bearing experiment, regenerates the
claim manifest and worked example, and writes provenance and checksums.

The seven stages are:

| Stage | Output |
|---:|---|
| 1 | invariant and encoding incidence |
| 2 | paired 16,384/131,072-row predicate studies |
| 3 | Bloom, min/max, and sparse-fence controls |
| 4 | logical, delivered, and transferred access measurements |
| 5 | auxiliary-plan measurements and neutral claim manifest |
| 6 | generated kernels, physical toy artifacts, and walkthrough |
| 7 | host provenance and input/output checksums |

Structural facts, answers, serialized bytes, and byte accounting are
deterministic for fixed inputs and dependencies. Runtime measurements vary by
machine. A full rerun therefore validates the method and expected behavior;
the committed canonical CSVs preserve the exact environment-specific values
reported by the study.

## Supporting diagnostics

```bash
make diagnostics
```

This separately regenerates `experiments/results/access_compiler/` and
`experiments/results/access_crossover/`. These runs diagnose generated-plan
overhead, access closure, selectivity crossover, and storage scheduling. They
are not read by `claim_manifest.rs`, do not support a displayed manuscript
number, and are therefore outside the seven-stage checksum manifest.

## Inputs

Downloaded inputs live under the ignored `experiments/eval/data/` directory.
They cover NAB, UCI Household Power, TPC-H, ClickBench, Public BI, and NYC Taxi.
Every consumed file is listed in `INPUTS.sha256`; acquisition fails if an
upstream file has changed. Sources and licences are documented in
[THIRD_PARTY_DATA.md](THIRD_PARTY_DATA.md).

`WITNESS_MAX_ROWS` controls the predicate/access prefix and must be at least
1,024. The canonical study uses 131,072 rows per column; the paired scale run
uses 16,384.

## Canonical outputs

| Path | Meaning |
|---|---|
| `experiments/results/claim_manifest.csv` | exact displayed claim values |
| `invariant_census/*.csv` | per-column facts, evidence, and premiums |
| `predicate_pipeline/*.csv` | complete query cells and source summaries |
| `predicate_pipeline_rows16k/*.csv` | paired smaller-scale study |
| `additional_plans.csv` | dictionary translation and run-length counting summaries |
| `certificate_study/*.csv` | Bloom/min-max/fence controls |
| `real_access/*.csv` | selected layouts and storage schedules |
| `experiments/generated/` | frozen generated Rust kernels |
| `docs/generated/` | worked example, exact layouts, and tiny artifacts |

`PROVENANCE.txt` records the host and toolchain.
`CHECKSUMS.sha256` covers inputs, canonical CSVs, the claim manifest,
generated source, and the generated walkthrough.

Large page bundles and intermediate Parquet files are regenerated on demand
and intentionally not versioned.

## Validation

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-features
```

Tests cover invariant soundness, restart seams, nullable and patched streams,
dictionary/RLE/frame compositions, descriptor tampering, malformed inputs,
generated/interpreted/handwritten answer equality, physical access closure,
certificate refinement, and generated-artifact drift.
