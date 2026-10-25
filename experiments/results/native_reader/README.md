# Native reader and ordered regions

The implementation and diagnostic are documented in
[`docs/NATIVE_FILES.md`](../../../docs/NATIVE_FILES.md).

[`20261025T111957Z/summary.md`](20261025T111957Z/summary.md) records:

- Exact answers from newly reconstructed native columns on 15 sampled corpus
  sources. No writer objects or oracle vectors are passed to the reader.
- Container descriptors add 10.44% to pooled page bytes in this 1,024-row-page
  sample. Median initialization is 0.155 ms per sampled column, measured once,
  including full-file checksum, parsing, dictionary-order checks, and fact
  reconstruction. This is not a repeated initialization benchmark.
- A controlled four-batch workload shows 47.19x lower warm query time at
  4,096 pages and 0.1% selectivity, admitting eight pages for planning.
  The zone-map-plus-shortcuts control uses the same regional certificates.
  The 36 timing cells cover three page counts and four selectivities.

The regional speedups are synthetic, and warm timings exclude building
certificates. The census screen finds adjacent individually ordered pages in
110 otherwise unordered columns at 16K rows per page; its saved CSV does not
contain the seam values needed to certify actual regions. It establishes
eligibility for further examination, not a natural-workload speedup.

`20261025T111957Z/VALIDATION.md` records the full 219-test Rust regression pass and the
new subprocess, parser, oracle-free execution, region, and I/O-accounting
checks. That timestamped directory is the earlier diagnostic and remains
unchanged. The completed full confirmation subsequently remeasured the main
benchmark and produced [`confirmed/summary.md`](confirmed/summary.md): 0.150 ms
median resident initialization and 48.64x selective synthetic regional speedup.
Its CSVs supply the scoped extension claims in the canonical manifest; the
main ablation remains separate. Original baseline evidence is archived in the
full-run workspace, and the portable confirmation report is under
[`full_runs/20261025T113643Z/`](../full_runs/20261025T113643Z/comparison.md).
Generated Rust and the numerical walkthrough match the confirmed execution
contract. Full remeasurement regenerates the extension before claim generation.
