# Validation — 25 October 2026

- Formatting, Clippy across all targets/features, shell syntax, and Python compilation pass. All 9 Python tests pass.
- Full Rust suite: `cargo test --locked --all-features --no-fail-fast`, **219 passed, zero failed**, 49 target summaries.
- All native-reader and ordered-region integration tests pass, including fresh-process queries after writer objects are dropped.
- Interpreted, static, and generated operators are tested with empty oracle vectors.
- Randomized nullable queries, page-order permutations, appends, conservative evidence loss, conjunctions, deletion vectors, malformed descriptors, and positioned confirmation-read accounting pass.
- New diagnostic: exact answers on 15 real-source columns and 36 controlled timing cells. Exported ratios and pooled storage accounting were independently recomputed from CSVs.
- The source hashes recorded before measurement match the tested implementation.
- Generated kernels were refreshed using `--source-only`; the numerical walkthrough was rebuilt. No canonical result CSV or measurement fingerprint was overwritten.

The census screen counts individually ordered adjacent pages, not verified cross-page seams. Its 16K-row result (110 columns with adjacent eligible pages) motivates the extension but does not establish a natural-corpus speedup. The timed region experiment uses four synthetic appended batches.

Current core fingerprint: `f79bda4f741a544a`. Canonical measurements retain their original `a3706a5cdcb90cf6` fingerprint. The manuscript PDF and canonical claim manifest retain their pre-change hashes.
