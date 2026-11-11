# Witness

Research artifact for *What an Encoding Knows*.

Fixed ICDE 2027 submission snapshot: **[v1.0.0](https://github.com/clumsyspeedboat/witness/releases/tag/v1.0.0)**.
Use `--branch v1.0.0` instead of `--branch main` in the clone command below
to reproduce the submission version independently of future updates.

Witness derives bounds, order, and dictionary mappings through composed codecs,
tracks their row scope and evidence source, and checks the query actions that
use them. The artifact includes the implementation, tests, frozen generated
kernels, and canonical measurements. Manuscripts, review notes, and downloaded
papers are excluded.

Structural guarantees assume valid decoding and correct rules. Verified facts
assume unchanged bytes; writer flags and cached summaries retain their stated
trust requirements. The checker catches supported plan inconsistencies, not
arbitrary corruption or false writer assertions. Unsigned deltas guarantee order
between restart anchors, not across them. The main planner uses fixed priorities;
optional cost/prepared-context diagnostics are separate from the main ablation.

## Start here: fresh clone

Use Linux with Rust (via rustup), Python 3.10+, and a C/C++ build toolchain.
On Ubuntu/Debian, install the system prerequisites:

```bash
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libssl-dev curl git make \
  python3 python3-venv util-linux ripgrep unzip
```

Install Rust using [rustup](https://rustup.rs/) if needed; Cargo automatically
selects the pinned toolchain in `rust-toolchain.toml`. Initial builds need
network access for Rust dependencies. From a fresh clone:

```bash
git clone --depth 1 --branch main https://github.com/clumsyspeedboat/witness.git
cd witness
python3 -m venv .venv
source .venv/bin/activate
python3 -m pip install -r experiments/requirements.txt
make verify
```

`make verify` runs formatting, Clippy, Rust/Python tests, and regeneration of the
claim manifest, checking it against the committed evidence. It downloads no
corpora and runs no full benchmark suite. It may take several minutes or longer
on the first build. **Start with this path**, then choose remeasurement below.

```bash
# Reconstruct the exact reported values from committed CSVs; no corpus needed.
make claims

# Remeasure all claim-bearing studies; downloads inputs and replaces local results.
./reproduce.sh
```

A full run can take several hours or longer depending on hardware and downloads.
Allow at least 60 GB free disk (more is prudent for build intermediates) and
substantial RAM for SF10 data. Run on an otherwise idle Linux machine with network
access to the documented data sources and DuckDB extensions. The cold study
attempts OS-cache eviction but cannot flush storage-controller caches; timings
will differ across machines. Do not interpret remeasurement as exact recovery
of the authors' timing values. Use a separate clone for new runs if you want to
keep the committed measurements untouched.

A data-free execution smoke test is available after verification:

```bash
./experiments/run_query_optimization.sh --smoke
```

It writes a new timestamped diagnostic directory and checks exact answers. It
never promotes smoke data to the confirmed natural-data results. Full
replication details and scoped extension measurements are in
[Replication](docs/REPLICATION.md) and
[Query optimizations](docs/QUERY_OPTIMIZATIONS.md).

### Supporting whole-column diagnostics

These earlier diagnostics are separate from the main paged-query ablation below.
In the whole-column census, **0.0%** combine structural unsigned-delta order
with cheap direct access under the tested policies. Facts and cheap access are
separate requirements; a licensed search is not necessarily the cheapest path.



A census of **1784** columns from **93** sources, and a query study of **152**
cells over **32** sources at 131,072 rows per column.

| | |
|---|---|
| non-decreasing, constant columns included | **10.4%** of columns |
| where evidence exists and is reachable | **0.15x** a sortedness-aware Parquet reference |
| price of keeping that access | **2.10x** the bytes |
| cold disk, fewest-bytes plan | **1.26x** slower than reading the whole file (1,083 scattered reads against one) |

The paged study ([`experiments/results/paged_study/`](experiments/results/paged_study/))
runs **1,166** selective SUM queries over 364 column pairs paged at 16,384 rows,
every page planned from its own facts, against Parquet read with and without
its page index (footer and page index parsed once per file), against DuckDB on the same rows
([`experiments/results/duckdb/`](experiments/results/duckdb/)), and against
the same executor planning from an exact stored zone map.
[`experiments/results/paged_scale/`](experiments/results/paged_scale/) repeats
them on TPC-H SF10 lineitem on 1 to 64 threads.

| | |
|---|---|
| paged queries against the faster Parquet read (with or without its page index) | **0.40x** the time (median) |
| the same queries against DuckDB on its own storage, one thread | **0.57x** the time (median) |
| planned from facts against an exact stored zone map, same executor | **0.94x** the time (geometric mean) |
| string range and prefix predicates against the zone map, same executor | **0.62x** the time (geometric mean) |
| string and float predicates against DuckDB, one thread | **0.12x** and **0.25x** the time (median) |
| TPC-H Q6 on SF10 lineitem against DuckDB, one thread | **0.75x** the time (0.70–0.75x across runs) |
| page skips that bounds read from encodings recover | **96.5%** of what stored min/max allow |
| pages skipped beyond an exact zone map, where a sorted dictionary holds no value in the range | **25%** more |
| a tenth of the rows deleted at random, every plan unchanged | **1.04x** the time (median), every answer exact |
| queries a zone map with a neighbour's or truncated statistics answers wrongly | **88** and **645** of 1,166; planned from facts, none |
| mutated plans that would answer wrongly, rejected by the checker | **484,951** of 484,951 |
| sorted key, 0.1% of 60 M rows, one thread | **0.04 ms** against Parquet's 0.31 ms (0.28–0.31 ms across runs) |

The integer median against the plain zone map is unchanged. A zone map given
the same shortcuts performs similarly: these comparisons isolate bound source
and shortcut availability, rather than establishing a faster search algorithm.
String gains use dictionary order; the matched-shortcut control removes almost
all of the difference. All result tables above are regenerated from the manifest.

Derived knowledge helps when it is present, cheap to reach, and not erased by the
storage layer beneath it. A plain scan was always correct anyway.

- [Runnable companion notebook](docs/notebooks/witness_companion.ipynb): the
  study cell by cell, from one page's bytes to the paged results. Toy
  examples compute live; measured results load from the canonical CSVs.
- [Walkthrough](docs/WALKTHROUGH.md): the idea without implementation detail.
- [Generated worked example](docs/generated/END_TO_END_EXAMPLE.md): 16 rows,
  five columns, real serialized files, exact offsets, plans, and byte counts.
- [Architecture](docs/ARCHITECTURE.md): decoder, layout, fact, and plan layers.
- [Replication](docs/REPLICATION.md): inputs, commands, outputs, and timing scope.
- [Testing](docs/TESTING.md): test cases, oracles, adversarial inputs, and gates.
- [Encoding scope](docs/ENCODINGS.md): measured formats and comparison limits.
- [Limitations](docs/LIMITATIONS.md): explicit claim boundaries.
- [Native files and ordered regions](docs/NATIVE_FILES.md): opening files
  without writer state and retaining order across appended batches.
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
