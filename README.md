# Witness

Research artifact for *What an Encoding Knows*.

> **A storage format does not merely hold values — it *proves* things about
> them.** Witness derives those proofs from the bytes, requires every query plan
> to cite the one it relies on, and falls back to a recorded scan when it cannot.

Columnar formats are judged on **size** and **decode speed**. This artifact
measures a third axis — **evidence**: what the bytes entail about the values, and
therefore which algorithms are legal at all.

### A proof that costs nothing

Store twelve timestamps as an anchor plus the gaps between them, gaps *unsigned*:

```text
value   1000  1003  1004  1004  1007  1012  1012  1015  …
          │     +3    +1    +0    +3    +5    +0    +3   <- unsigned: no minus sign exists
       anchor
        64 bits + 11 x 7 bits = 141 bits          raw: 12 x 64 = 768 bits
```

The column **cannot** decrease — disorder is unwriteable — so binary search is a
legal substitute for a scan. The proof costs **zero extra bytes**: the data is
the evidence.

### A proof alone is worthless

| | bytes not reachable | bytes reachable |
|---|---|---|
| **order proved** | sealed in a frame: true, unusable | **boundary search authorized** |
| **nothing proved** | scan | restart anchors, nothing to spend |

Order and reachability are independent. In the census **0.0%** of columns hold
the free structural proof *and* fast access: beyond 256 rows the two requirements
contradict each other. Paged columns resolve this differently: every page proves
or certifies its own order, and appending a page checks one seam to keep the
whole column certified.

### An encoding already carries a zone map

A frame-of-reference base is the exact minimum of its page, and the bit width
bounds the maximum; a sorted dictionary holds the exact minimum and maximum in
its first and last entries. Witness derives these bounds as facts, at no byte
cost, and a page whose bound excludes a predicate is skipped, while one whose
bound lies inside it is accepted without decoding.

### Why this is not a cost model

| | conventional optimiser | Witness |
|---|---|---|
| index-scan vs scan is a… | **cost** decision | **legality** decision |
| picking wrong gives… | a slow answer | **the wrong answer** |
| so the check is… | a term in a cost model | **a gate, re-verified before running** |

### What is measured

A census of **1784** columns from **93** sources, and a query study of **152**
cells over **32** sources at 131,072 rows per column.

| | |
|---|---|
| non-decreasing, constant columns included | **10.4%** of columns |
| where evidence exists and is reachable | **0.13x** a sortedness-aware Parquet reference |
| price of keeping that access | **2.10x** the bytes |
| cold disk, fewest-bytes plan | **1.31x** slower than reading the whole file (1,083 scattered reads against one) |

The paged study ([`experiments/results/paged_study/`](experiments/results/paged_study/))
runs **1,166** selective SUM queries over 364 column pairs paged at 16,384 rows,
every page planned from its own facts, against Parquet read with and without
its page index, against DuckDB on the same rows
([`experiments/results/duckdb/`](experiments/results/duckdb/)), and against
the same executor planning from an exact stored zone map.
[`experiments/results/paged_scale/`](experiments/results/paged_scale/) repeats
them on TPC-H SF10 lineitem on 1 to 64 threads.

| | |
|---|---|
| paged queries against uncompressed Parquet with page index | **0.34x** the time (median) |
| the same queries against DuckDB on its own storage, one thread | **0.68x** the time (median) |
| planned from facts against an exact stored zone map, same executor | **0.85x** the time (geometric mean) |
| page skips that bounds read from encodings recover | **96.8%** of what stored min/max allow |
| mutated plans that would answer wrongly, rejected by the checker | **379,753** of 379,753 |
| sorted key, 0.1% of 60 M rows, one thread | **0.05 ms** against Parquet's 0.56 ms |

Derived knowledge helps when it is present, cheap to reach, and not erased by the
storage layer beneath it. A plain scan was always correct anyway.

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
