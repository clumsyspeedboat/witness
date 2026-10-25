# Full confirmation passed

All eight canonical reproduction stages, independent repeats, native-reader/ordered-region diagnostic, compression smoke pipeline, final release Rust tests, Python tests, and baseline comparisons completed successfully.

- **219 Rust tests passed** in the final complete release suite.
- **13 Python tests passed** after the four new prose-rendering regression tests.
- All 666 claim keys are present. Protected corpus, query, skip, and checker counts match.
- Exact answers and page decisions match across 23,261 integer/float/string query-arm rows.
- All six census CSVs are byte-identical to the frozen baseline.

Ratios below one favor the numerator. Timings vary between runs; these small movements do not establish new algorithmic gains.

| Metric | Frozen run | Confirmation run |
|---|---:|---:|
| Integer Facts / zone map, geometric mean | 0.95 | 0.94 |
| Integer Facts / zone map, median | 1.00 | 1.00 |
| Zone map + shortcuts / plain zone map | 0.93 | 0.93 |
| String Facts / zone map | 0.62 | 0.62 |
| String Facts / zone map + shortcuts | 1.01 | 1.01 |
| Float Facts / zone map | 0.92 | 0.92 |
| Integer ratio of total query times | 0.99 | 0.98 |
| Q6 Facts / zone map, one thread | 1.00 | 0.99 |
| Cold in-place Facts / scan, unclustered | 4.1 | 4.2 |
| Cold whole-page Facts / scan, unclustered | 1.02 | 1.00 |

Additional comparisons: Q6 Facts / DuckDB on one thread changed from 0.71 to 0.75; the full-length scaling Facts / zone-map geometric mean changed from 0.88 to 0.90. These are remeasurements, not correctness failures. The median integer Facts / zone-map ratio remains 1.00.

Two tooling issues found and fixed during confirmation: the diagnostic runners now honor custom Cargo output directories; numerical public prose now refreshes automatically from the manifest. Earlier failed attempts remain in run.log for audit. The successful canonical measurements were not rerun merely to repair these tooling steps.

Certificate boundary-confirmation reads now contribute to cold I/O accounting. The native reader is resident and the region benchmark is synthetic; neither establishes a general cold-read or natural-corpus region speedup.

The current source fingerprint is `f79bda4f741a544a`; frozen measurements retain `a3706a5cdcb90cf6`. Original canonical CSVs and manuscript are preserved. Full regenerated results live under `workspace/experiments/results/`; new diagnostics are in `native_reader/` and `evidence_layout_smoke/`.
