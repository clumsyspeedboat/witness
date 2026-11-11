# ICDE submission artifact validation

Validated on 11 November 2026 from a separate Git clone with no manuscript,
corpus cache, or local result symlinks. A shared Cargo build cache was reused;
the exported source was rebuilt and tested at the new checkout path.

- `make verify`: passed formatting, Clippy with warnings denied, 240 Rust tests,
  24 Python tests, and byte-identical regeneration of the claim manifest.
- Data-free execution smoke test: passed 16 shapes, 704 warm query cells,
  and 1,008 total-timing cells, with exact-answer checks.
- Python virtual-environment installation of pinned DuckDB 1.5.3: passed.
- Existing downloaded corpus inventory: all 2,221 pinned files verified.
- Confirmed prepared-query result checksums: passed.
- The full multi-hour benchmark suite and fresh acquisition of every corpus
  were not repeated during packaging. Existing run CSVs and provenance are
  retained; fresh timing values depend on the reviewer's machine.

The root README documents the tested fast path and the separate full run.
The `v1.0.0` tag identifies the final ICDE submission snapshot;
`icde2027-submission` preserves the preceding packaging snapshot. No
manuscript PDF, TeX source, review notes, or downloaded comparison papers are
included. Historical Git objects remain available; use the documented shallow
clone or the release source archive to avoid fetching development history.

Final manuscript audit: all 705 claim definitions match canonical values; 179
are referenced by the current manuscript. Independent calculations reproduce
headline ratios and mutation counts. The final PDF matches a fresh source build.
49 targeted code/paper tests passed. No full benchmark rerun was performed.
Independent asset/statistical checks also reproduce the five figure renders,
11 external table sources, Figure 4 group counts/ratios, and a consistent
source-cluster bootstrap interval. The audit is evidence reconciliation, not
a mechanical proof of all claims or an independent rerun of every experiment.
