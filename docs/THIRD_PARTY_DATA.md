# Third-party data

None of these corpora are redistributed in this repository. `make fetch-study`
downloads them into `experiments/eval/data/`, which is git-ignored. Every file
the study consumes is checksum-pinned in [`INPUTS.sha256`](../INPUTS.sha256), and
`reproduce.sh` verifies those hashes before it measures anything, so an upstream
file that changes content fails the run instead of silently changing a reported
number.

Licences below are as published by each source at the time of access
(2026-08). Check the current terms before redistributing anything yourself.

| Corpus | Used for | Source | Licence | Redistributed |
|---|---|---|---|---|
| NAB (Numenta Anomaly Benchmark) | sensor/timestamp columns | `github.com/numenta/NAB` | AGPL-3.0 (data files); pinned by revision | No |
| UCI Household Power Consumption | household power column | `archive.ics.uci.edu` (dataset 235) | CC BY 4.0 | No |
| NYC TLC Trip Records | taxi timestamp/value columns | `d37ci6vzurychx.cloudfront.net/trip-data/` | NYC TLC terms of use | No |
| ClickBench (`hits`) | web-analytics columns | `datasets.clickhouse.com` | Apache-2.0 (benchmark) | No |
| Public BI Benchmark | Tableau Public workbook columns | Public BI benchmark repository | See benchmark repository | No |
| TPC-H `lineitem` | generated relational columns | locally generated | TPC EULA — generated, not distributed | No |

## Notes

- **NAB** is the only source pinned by upstream revision as well as content
  hash; the others are pinned by content hash only. A moved or re-published URL
  therefore breaks acquisition loudly rather than degrading quietly.
- **TPC-H** columns are generated locally with `dbgen`; no TPC data is shipped.
- Corpus attribution follows the original source rather than a secondary
  benchmark description.
