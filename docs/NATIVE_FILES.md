# Self-contained native files and ordered regions

`access_compiler::native_file` adds an experimental resident file reader for
nullable integer columns. It reconstructs the decoder and derives facts from
the file. Writer objects, oracle values, saved fact sets, and saved page ends
are not required. This reader uses the existing Rust decoder/checker; it is
not a second, independently implemented verifier.

```rust
use witness::access_compiler::{native_file, paged::{self, Planning}};

let mut selector = native_file::open("selector.wnf")?;
let value = native_file::open("value.wnf")?;
selector.column.enable_ordered_regions();
let (sum, matched, counts) = paged::sum_where(
    &selector.column, &value.column, 100, 199, Planning::Facts, 1,
)?;
```

The `native_query` binary opens files in a fresh process:

```bash
cargo run --locked --release --features experiment --bin native_query -- \
  --regions selector.wnf value.wnf 100 199
```

## File and initialization scope

Version 1 is a resident container, separate from the legacy `paged_file`
container used by the published cold-read experiment. Opening reads the
whole file, checks its checksum, parses descriptions and page layouts,
verifies declared dictionary order, and derives page facts. It reconstructs
ordered-page ends from encoded first/last values. These costs belong to
initialization; query timing alone does not include them.

The file consists of:

1. Magic `WITNCOL1`, nominal rows per page, and page count.
2. Length-delimited page records containing recipe length, decoder length,
   optional exact statistics, non-null count, recipe description, decoder
   description, and the existing serialized page bytes.
3. An FNV-1a checksum over all preceding bytes.

Integers are little-endian, with 64-bit length/identifier fields and 128-bit
bound values. Recipe descriptions preserve construction labels, including
framing. Execution and fact derivation consume the decoder and page layout.
No logical values are re-encoded when opening a file. `descriptor_bytes`
counts all container bytes beyond the existing page bytes, including recipe
and decoder descriptions, statistics, lengths, and the checksum. It is
additional storage and is not credited as free encoding evidence.

The parser checks lengths, node topology, recipe/decoder agreement, field
extents, and known tags. This version supports at most 64 decoder nodes,
128 recipe levels, u32 page-row domains, and 256 MiB of decoded frame data per
page. The container has an implicit integer logical type; names, decimal
scales, string dictionaries outside integer pages, and table schema evolution
are outside its scope. Intermediate partial pages from separate appends are
preserved.

Checksums detect accidental damage. They do not establish an honest writer:
page-order/null-placement flags and valid dictionary codes retain the existing
writer trust assumptions. All declared sorted dictionary tables are checked
at initialization; the root dictionary's successful check is cached for
subsequent planning. This does not decode every id to recheck code validity.
Run lengths and index contents likewise retain their existing validity
assumptions; structural parsing is not a full logical audit of every value.

`EncodedColumn::truth` is now only an optional writer/test oracle. Interpreted,
static, generated, and paged query paths obtain row counts from the validated
decoder. Loaded native pages leave the oracle empty.

## Independent ordered regions

The default column certificate still requires an ordered column. Calling
`enable_ordered_regions` additionally builds maximal page ranges in which
every page has dense all-row order evidence and every adjacent seam is
non-decreasing. An unordered/nullable page ends a region and remains a
candidate for page-by-page planning. A decreasing seam starts another region.
Appending can extend the last region or start a new one, without moving rows.

Each certified region is binary-searched independently. Queries retain the
union of its candidate ranges and all uncertified gaps. Conjunctions intersect
their candidate page sets; SUM, COUNT, SUM-product, and deletion vectors keep
their existing row semantics. Facts and zone-map-plus-shortcuts planners can
both use the same region certificates. Other planners remain controls.

Boundary confirmations read at most two excluded pages per region and count
their positioned reads and bytes. This also corrects the previous omission
of the whole-column certificate's confirming reads from I/O accounting.
Certificate rebuilding follows forged-premise mutations used by fault tests;
it does not strengthen the trust placed in writer-attested page order.

Candidate pruning costs depend on the number and lengths of regions, and
uncertified gaps still require planning. Complete queries also validate page
alignment, so binary-search complexity is not a claim of logarithmic total
query time. Region metadata occupies resident memory. It is reconstructed
after opening and is not serialized as an extra index in this version.

## Validation and measurement

```bash
cargo test --locked --features experiment --test native_reader --test ordered_regions
WITNESS_NATIVE_RESULT_DIR=experiments/results/native_reader/NEW_RUN \
  cargo run --locked --release --features experiment --bin native_reader_study
```

The tests include a fresh subprocess after dropping the writer, all codec
families, nullable/extreme/partial pages, descriptor faults, randomized native
queries, sparse region intersections, appends, deletes, and confirmation-read
accounting. Generated/static execution is tested after clearing its oracle.

The diagnostic reports initialization and container storage for deterministic
source samples from the census. A separate controlled experiment uses four
appended sorted batches at 64, 512, and 4,096 pages, with four selectivities.
It compares ordinary Facts, regional Facts, and a zone map given the same
regional shortcuts. The confirmed exports supply the manuscript's scoped
extension claims, separately from the cold and paged ablations. Full
remeasurement runs this study before generating claims; standalone runs
write a fresh directory and do not replace the confirmed exports. Timing calibrates toward
2 ms per sample and reports nine interleaved rounds; it does not guarantee
that every sample reaches that duration.

Core source changes require regenerating frozen kernels. Each generator now
supports `--source-only`, which refreshes generated Rust without relabeling
or overwriting the frozen benchmark CSVs. The documentation walkthrough can
then be regenerated with `documentation_example`.
The existing measurement fingerprints and checksum manifests describe their
original recorded runs. New diagnostics record the current source fingerprint
and hashes; a kernel refresh does not turn old timings into new measurements.
