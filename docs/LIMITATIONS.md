# Limitations and claim boundaries

These constraints are part of the result.

## Research claim

- Witness is an exploratory study of representation-derived facts, generated
  query paths, and physical access cost. It is not a production storage
  engine, a universal format replacement, or a performance claim about every
  Parquet implementation and workload.
- The census contains 1,784 columns from 93 public sources: every exact
  column of the first table of each Public BI workbook, of TPC-H and TPC-DS
  SF1, of the ClickBench hits partition, and of the NAB, household-power, and
  taxi series. It is not a random sample of all database workloads. Constant
  columns are non-decreasing too; the summary reports them separately, and
  global order over non-constant columns is rare outside time series and
  benchmark keys.
- The single-page generated-kernel study contains 40 selector/value pairs and
  152 predicate cells. Sources, not cells or columns, are its resampling unit.
  The separate paged study compares complete queries across multiple pages;
  its medians summarize the fixed workload rather than a population sample.
- Access-ready Witness versus the Parquet boundary-search path has source
  median 0.14x with a 95% bootstrap interval [0.08, 0.14]. The interval sits
  below parity, but the 32 sources are not 32 independent draws: NAB
  contributes several columns per archive family. A two-stage cluster bootstrap
  (families, then sources within each) gives [0.06, 0.30]. Collapsing each
  family to a single draw instead gives 0.27x [0.10, 1.05], which spans parity,
  but that weights a one-column family like a ten-column one and the singleton
  families are the worst cases. Distribution-free, 9 of the 12 families favour
  the compiled plan: a Wilcoxon signed-rank test on the log ratios gives a
  two-sided p=0.012. The weaker sign test, which discards magnitude, gives a
  one-sided p=0.073 on the same data. Read the separation as evidence for the
  mechanism on this corpus, not as a population-level claim.
- Monotone and no-fact classes are imbalanced. The no-fact class has six
  sources and includes identifier-like and transactional columns.

## Workload scope

- The claim-bearing query shape is a selector predicate followed by an aligned
  SUM, with quantile-derived selectivities from roughly 0.1% to 50%.
- GET, SUM, BETWEEN, selected-range SUM, and equality/membership controls test
  individual mechanisms. Joins, group-by, top-k, updates, and multi-predicate
  planning are not evaluated.
- The two additional plans are feasibility evidence, not precise ratios, but for
  different reasons. Dictionary range translation reuses the main predicate
  study's per-cell ratios, with the same harness and answer checks; its figure is
  a median over only 14 cells, so it tracks decoder revisions far more sharply
  than the 152-cell aggregate does, having read 0.27 and 0.48 across revisions
  where the full-study scan ratio moved only 0.73 to 0.66. Run-length counting
  uses a separate lightweight harness, a median of seven repetitions with no
  warmup iteration over 21 columns; its measured value has in practice been
  stable to within a few percent. In both cases read the leading digit and treat
  the plans as demonstrated rather than benchmarked.
- The aggregation leg usually performs fused encoded traversal. It avoids
  materializing an output vector but is not metadata-only unless an exact
  aggregate certificate exists.
- Piecewise monotone search performs independent binary searches per restart
  block. It has no block-level pruning without separate bounds.

## Storage and hardware scope

- The single-page studies are single-threaded over resident serialized
  buffers. The paged studies also run pages on up to 64 threads, but only
  within one query; concurrent queries are not measured.
- A separate mechanism study measures local XFS, root-volume ext4, and tmpfs,
  including per-page, sorted, coalesced, and full-read schedules.
  It does not establish behavior on NVMe, remote filesystems, object stores,
  cloud caches, or direct I/O.
- The cold XFS result is one host and one bundle: 20.44 MiB of required closure
  in a 27.68 MiB file. Minimal per-page reads issued 1,083 calls and took
  1.27x a full read; coalescing issued 119 calls and reached 1.00x. This is a
  mechanism result, not a device-universal constant.
- "Cold" in that experiment means the OS page cache was evicted with
  `posix_fadvise(POSIX_FADV_DONTNEED)` before each repeat. That call is
  advisory and reaches only the page cache; the backing device is behind a RAID
  controller with its own DRAM cache, which it cannot evict. The measurement is
  therefore OS-cold, not device-cold. The practical consequence is visible in
  the data: the three repeats within a run agree to about 0.1% (p25 and p75 are
  indistinguishable from the median), yet the per-page ratio has varied by more
  than ten percent across repeated runs on the same host and the same bundle.
  Treat the tight within-run percentiles as precision, not as reproducibility.
  Only the committed run is reproducible from this repository; the cross-run
  spread was observed during development and is recorded here as a caution
  rather than as a measurement.
- Logical and delivered bytes are reader/layout accounting. Transferred bytes
  are measured by the prototype's positioned-read schedule, not hardware
  performance counters.
- Explicit SIMD specialization, concurrent readers, asynchronous I/O, cache
  contention, and NUMA are out of scope.
- The generic executor widens values to `i128`; specialized code lanes operate
  on 64-bit codes and widen aggregate results. Their costs differ, so generic
  per-value overhead does not characterize every execution path.
- Aggregation over a size-selected layout still trails Parquet row selection
  at the median, because a Zstd frame enclosing the whole column must be
  decompressed for any read; chunked frames remove that cost at about 2% more
  bytes. The former deficit on directly addressable layouts was per-value
  overhead, removed by batch decoding.
- Latency ratios here are not comparable to those in earlier revisions of this
  artifact. The release
  tag and source fingerprint identify which decoder produced a given number.
- Results were measured with `lto = "fat"` and `codegen-units = 1`. Both the
  Witness arms and the Parquet/Arrow reference are linked into the same study
  binary, so the profile applies to them identically.

## Fact calculus scope

- The calculus covers value, mapping, and access facts over bitpack, FOR,
  signed/unsigned delta, dictionary, RLE, patch, nullable, and framed
  compositions.
- Doubles are queried through order-preserving integer keys
  (`access_compiler::floats`), with -0.0 folded into 0.0 and NaN stored as
  null; strings through sorted per-page dictionaries
  (`access_compiler::strings`), whose order is checked evidence from the
  writer. Pattern predicates other than prefixes are not planned.
- It does not derive bounded disorder, cross-column dependencies,
  approximate facts, or arbitrary decoder programs.
- The paged cold-read study (`paged_cold_study`) evicts the OS page cache
  and confirms eviction with `mincore`, but cannot clear the RAID
  controller's cache, so it too is OS-cold, not device-cold.
- Structural facts assume checked arithmetic. Checked facts trust the encoder
  and an authenticated descriptor checksum. The checksum detects accidental
  or test tampering; it is not a cryptographic trust system.
- Certificates are generated and consumed by the same Rust implementation.
  An independent reader implementation has not yet validated the format.
- The rule grammar is finite. “Optimal” means the selected plan under this
  fact/plan language, not the globally strongest equivalent program.

## Certificate controls

- Bloom filters, block min/max, and sparse fences are separate experimental
  sidecars. They are not integrated into the serialized Witness page menu or
  credited as free page capabilities.
- Bloom positives and min/max overlaps are candidate guarantees and require
  value refinement.
- The canonical Bloom control helps absent and rare probes but not frequent or
  mixed-IN probes. At frequent selectivity it selects every block and its
  modeled bytes exceed scan (1.172x).
- Sparse fences require globally sorted non-null data.

## Format and policy scope

- The access-ready and size-selected menus are fixed finite policies, not an
  exhaustive codec search. Access-ready excludes opaque frames and uses
  shorter delta restart intervals.
- The access-ready byte premium is 2.10x in the canonical aggregate. The
  smaller delivered closure is therefore purchased, not free.
- Witness stores columns as independently encoded pages, appended and sealed,
  with a checked order certificate across pages; pages are never updated in
  place or deleted. It has no production multi-column table container, schema evolution,
  encryption, checksummed payload blocks, transactions, crash recovery, or
  concurrent writer protocol.
- Tiny artifacts in the generated walkthrough are mechanics examples.
  Sixteen rows are dominated by format headers and cannot support compression
  or timing claims.

## Comparator scope

- Parquet, PCodec, ORC, CSV, raw i64, and Zstd artifacts are generated and
  round-trip checked where claimed.
- The Parquet query reference uses the current Arrow/Parquet implementation
  and explicit page-size/configuration controls. It is not every Parquet
  engine.
- ORC-Rust 0.8.0 does not expose outer compression in the study writer, so the
  ORC arm is labeled RLEv2 without outer compression.
- FastLanes, BtrBlocks, LeCo, White-box Compression, and Vortex are
  literature-only comparisons. No local timings or fabricated physical byte
  layouts are assigned to them.

## Reproducibility scope

- Structural outputs are deterministic for fixed inputs and dependency
  versions. Timing values are host-dependent.
- Generated source is guarded by an exact source fingerprint and Plan IR
  signatures. Documentation drift tests compare checked source and `.acp`
  pages with regenerated bytes.
- The release tag, exact source fingerprint, input hashes, and generated
  artifact checksums jointly identify the evaluated state.
