# Witness

Witness is the research artifact for *What an Encoding Knows*. It asks a
specific systems question:

> When a serialized column proves a fact about its values or access paths,
> which query algorithm may safely use that fact, what bytes must it read, and
> when is the extra structure worth its storage cost?

The prototype derives typed facts from composed decoder and layout
descriptions. A query plan may use a non-scan algorithm only when one of those
facts authorizes it. Missing evidence causes a conservative scan or decode.

- [Runnable companion notebook](notebooks/witness_paper_companion.ipynb): the
  study cell by cell, each cell labelled with the paper section it mirrors.
  Toy examples compute live; measured results load from the canonical CSVs.
- [Walkthrough](docs/WALKTHROUGH.md): the idea without implementation detail.
- [Generated worked example](docs/generated/END_TO_END_EXAMPLE.md): 16 rows,
  five columns, real serialized files, exact offsets, plans, and byte counts.
- [Architecture](docs/ARCHITECTURE.md): decoder, layout, fact, and plan layers.
- [Replication](docs/REPLICATION.md): inputs, commands, outputs, and timing scope.
- [Testing](docs/TESTING.md): test cases, oracles, adversarial inputs, and gates.
- [Encoding scope](docs/ENCODINGS.md): measured formats and comparison limits.
- [Limitations](docs/LIMITATIONS.md): explicit claim boundaries.
- [Third-party data](docs/THIRD_PARTY_DATA.md): sources, licences, and hashes.

## Evidence

Every reported value is derived from canonical CSVs by
[`claim_manifest`](src/bin/claim_manifest.rs). The frozen mapping from claim
name to exact displayed value is
[`experiments/results/claim_manifest.csv`](experiments/results/claim_manifest.csv).

The claim-bearing evidence chain and supporting diagnostics are public:

- `experiments/results/`: canonical measurements, confidence intervals,
  provenance, and input/output checksums;
- `experiments/generated/`: generated kernels frozen before evaluation;
- `docs/generated/`: a Rust-generated numerical and physical walkthrough;
- `src/` and `tests/`: compiler, runtime, experiments, properties, and
  adversarial checks.

No reported statistic is hand-entered into the claim manifest.

## Reproduce

Regenerate the exact claim values from the committed canonical results:

```bash
make claims
```

Rerun every claim-bearing study from checksum-pinned corpus inputs:

```bash
./reproduce.sh
```

The full run fetches data, verifies `INPUTS.sha256`, applies each study's
answer and cardinality gates, remeasures the experiments, regenerates the
claim manifest and worked example, and writes provenance and checksums.
Structural results and byte counts are deterministic. Timings can vary by
machine; the frozen canonical CSVs preserve the environment used for the
reported values.

Rerun the compiler and crossover diagnostics, which do not feed the claim
manifest:

```bash
make diagnostics
```

Focused validation:

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-features
```

The Rust toolchain is pinned in `rust-toolchain.toml`; dependency resolution
is pinned in `Cargo.lock`.

## Layout

```text
src/access_compiler/   decoder/layout/plan IRs, facts, compiler, runtime
src/experiment/        corpus adapters and measurement logic
src/bin/               generators and study entry points
tests/                 soundness, execution, drift, and malformed-input tests
experiments/           fetch scripts, frozen kernels, and canonical results
docs/                  explanation, replication guide, and generated example
```

## License

The implementation is Apache-2.0. Corpus inputs are fetched rather than
redistributed; their individual terms are documented in
[Third-party data](docs/THIRD_PARTY_DATA.md).
