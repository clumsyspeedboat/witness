# Evidence-preserving compression pilot

The 25 October 2026 pilot does **not** meet the frozen promotion rule. The
canonical benchmark results and submitted manuscript are unchanged.

| Candidate | Held-out bytes/direct | Held-out Facts time/direct | Decision |
|---|---:|---:|---|
| Exposed metadata, one frame per packed stream | 0.783 | 2.302 | do not promote |
| Exposed metadata, 4 KiB packed-stream chunks | 0.812 | 3.143 | do not promote |

The target was at least 20% fewer pooled selector/value-pair bytes with no more
than 5% higher geometric-mean complete-query time than the direct layout.
Exposed metadata improves substantially over compressing whole compositions,
but compressed value access remains expensive on this resident workload.
This pilot does not establish the same tradeoff for cold or remote storage.

- [`20261025T095711Z-smoke/`](20261025T095711Z-smoke/summary.md): full pipeline
  smoke test on three synthetic pairs, twelve predicates, six layouts, and
  four planners. All SUM/count checks pass. Smoke cannot promote a candidate.
- [`20261025T095812Z-pilot/`](20261025T095812Z-pilot/summary.md): 24 real sources,
  one pair each; eight development and sixteen held-out sources. There are
  26 development and 50 held-out predicates, and 1,824 layout/planner timing
  cells. All SUM/count checks pass. Nine rotating interleaved rounds per cell.

The whole/payload and one-frame/chunked controls share logical codec choices;
size-based selection can choose different recipes per page. They therefore
compare selected menus, including this selection effect. Each layout gives
the exact zone-map baseline the same encoding shortcuts and executor. The
CSV summaries also report source-weighted times, same-layout strong controls,
first-query costs, and individual regressions.

`PROVENANCE.txt` records the premeasurement state and measurement source hashes.
`ANALYSIS.sha256` records the final summary validator, whose later changes add
an explicit smoke guard and clarify interpretation without changing the
measurements, thresholds, or result. Source hashes of the recipe transformer
and timed benchmark remain identical to their premeasurement values. The
full initial decision protocol is in
[`src/bin/evidence_layout/README.md`](../../../src/bin/evidence_layout/README.md).

Reproduce with `./experiments/run_evidence_layout.sh --smoke`, then
`./experiments/run_evidence_layout.sh`. Each invocation uses a fresh result
directory. The query worker is pinned to one CPU; the host remains shared.
