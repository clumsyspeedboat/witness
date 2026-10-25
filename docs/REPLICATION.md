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
If a local manuscript is present, `make claims` also refreshes every
`paper/**/claim.tex` from that manifest, and the paper drift test checks every
`.tex` file below `paper/`.

## Full remeasurement

```bash
./reproduce.sh
```

It needs Rust, Python 3 with the pinned `duckdb` package
(`pip install -r experiments/requirements.txt`), and
about 30 GB of free disk for the Parquet files the paged studies export for
DuckDB (`experiments/results/paged_*/parquet/`, ignored by git).

This one command fetches all corpus inputs, verifies `INPUTS.sha256`, applies
the validation gates, reruns every claim-bearing experiment, regenerates the
claim manifest and worked example, and writes provenance and checksums.

The eight stages are:

| Stage | Output |
|---:|---|
| 1 | invariant and encoding incidence |
| 2 | paired 16,384/131,072-row predicate studies |
| 3 | Bloom, min/max, and sparse-fence controls |
| 4 | logical, delivered, and transferred access measurements |
| 5 | paged queries (facts, zone map, scan) against Parquet and DuckDB, checker and attestation fault injection, the untimed direct against byte-optimised menu study, 4 Ki and 64 Ki page sizes, and 60 M-row thread scaling |
| 6 | self-contained reader and synthetic ordered-region measurements, auxiliary plans, and claim manifest |
| 7 | generated kernels, physical toy artifacts, and walkthrough |
| 8 | host provenance and input/output checksums |

Structural facts, answers, serialized bytes, and byte accounting are
deterministic for fixed inputs and dependencies. Runtime measurements vary by
machine. A full rerun therefore validates the method and expected behavior;
the committed canonical CSVs preserve the exact environment-specific values
reported by the study.

## Supporting diagnostics

The [native reader and ordered-region study](NATIVE_FILES.md) supplies the
manuscript's explicitly scoped extension claims from
`experiments/results/native_reader/confirmed/`. Full remeasurement runs it in
a fresh directory during stage 6, validates its exports, and then refreshes
that canonical directory before regenerating claims. Its CSVs are included
in stage 8's checksum manifest. To run an additional diagnostic without
replacing the confirmed inputs:

```bash
WITNESS_NATIVE_RESULT_DIR=experiments/results/native_reader/NEW_RUN \
  cargo run --locked --release --features experiment --bin native_reader_study
```

The evidence-layout pilot compares compression around whole compositions with
compression of packed streams only, with and without 4 KiB chunks:

```bash
./experiments/run_evidence_layout.sh --smoke
./experiments/run_evidence_layout.sh
python3 -m unittest discover -s tests -p test_evidence_layout_summary.py
```

It uses a fixed development/held-out source split and writes to a new directory
under `experiments/results/evidence_layout/`. Its frozen promotion rule,
matched codec controls, timing scope, and output definitions are documented in
[`src/bin/evidence_layout/README.md`](../src/bin/evidence_layout/README.md).
Set `WITNESS_EVIDENCE_CPU` to select the query worker's CPU. The runner takes
a cooperative lock and pins that worker; unrelated host jobs can still contend
for hardware. The pilot does not feed the claim manifest.

```bash
make diagnostics
```

This separately regenerates `experiments/results/access_compiler/` and
`experiments/results/access_crossover/`. These runs diagnose generated-plan
overhead, access closure, selectivity crossover, and storage scheduling. They
are not read by `claim_manifest.rs`, do not support a displayed manuscript
number, and are therefore outside the eight-stage checksum manifest.

## Inputs

Downloaded inputs live under the ignored `experiments/eval/data/` directory.
They cover NAB, UCI Household Power, TPC-H, ClickBench, Public BI, and NYC Taxi.
Every consumed file is listed in `INPUTS.sha256`; acquisition fails if an
upstream file has changed. Before measuring, the input gate also rejects
missing or extra CSVs in the discovered corpus directories, including the
NAB, census, and scale corpora. This prevents a partial download or a newly
added column from silently changing the workload. Verify the cached inputs
without rerunning a study with:

```bash
python3 experiments/eval/scripts/verify_inputs.py
```

Sources and licences are documented in
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
| `paged_study/*.csv` and `paged_study_rows*/*.csv` | page planning, checker mutations, statistics and attestation faults, bounds, and page-size controls |
| `paged_study/menus.csv` | untimed menu study: per pair, under the direct and the byte-optimised menu, the compositions chosen, selector pages with a derived bound and with order from their bytes or from the writer, page actions under facts and a zone map, and bytes against Zstd Parquet |
| `paged_scale/*.csv` | large-column queries and thread scaling |
| `duckdb/*.csv` | checked DuckDB answers, timings, scan counts, and version |
| `native_reader/confirmed/*.csv` | resident reader initialization/storage and synthetic regional query timings |
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
python3 -m unittest discover -s tests -p 'test_*.py'
```

Tests cover invariant soundness, restart seams, nullable and patched streams,
dictionary/RLE/frame compositions, descriptor tampering, malformed inputs,
generated/interpreted/handwritten answer equality, physical access closure,
certificate refinement, and generated-artifact drift.
