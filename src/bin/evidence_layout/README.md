# Evidence-preserving compression diagnostic

This pilot compares six size-selected page menus on the same columns:

| Menu | Codec choices | Compression |
|---|---|---|
| direct | canonical four direct recipes | none |
| compact | canonical compact menu, including deltas | whole composition |
| whole | direct recipes | whole composition, one frame |
| whole_4k | direct recipes | whole composition, independent 4 KiB decoded chunks |
| payload | direct recipes | packed streams only, one frame per stream |
| payload_4k | direct recipes | packed streams only, independent 4 KiB decoded chunks |

All four added menus retain the direct recipes as fallbacks. Selection minimizes
complete serialized size, including metadata, frame descriptors, and padding.
Payload placement leaves bases, dictionary tables, validity, and indexes outside
frames. No measured-query timing participates in encoding selection.

## Frozen decision rule

Before measuring, the promotion threshold is at least 20% fewer pooled pair
bytes than direct and at most 1.05x direct Facts geometric-mean query time on
held-out sources. Report individual regressions and the source-weighted result
alongside this gate. Passing warrants a broader rerun; failure leaves canonical
measurements and manuscript claims untouched. The smoke workload cannot promote
a candidate. These are engineering targets, not statistical guarantees.

FNV-1a of group, a zero byte, and source assigns whole sources: hash modulo three
equal to zero is development, all others held out. The pilot evenly selects
eight development and sixteen held-out sources from their sorted names, and
chooses one pair per source by its hash. This selection is independent of
timings. `--all` evaluates all canonical pairs with the same partition.

The 2x2 comparison of whole/payload placement and whole/4 KiB frames holds the
logical codec menu fixed. Pages can select different recipes when placement
changes their serialized sizes; the comparison includes that selection effect,
and the CSVs record the chosen recipes. Compact is a reference rather than a causal framing control,
because it adds delta codecs. Every layout runs Facts, scan, exact zone map,
and zone map with the same shortcuts, on the same executor. Source data, row
caps, page size, predicates, and exact SUM/count oracle match the paged study.

Each pair reports encoding time and the first Facts query with initially
uncached dictionary-order verification. Warm query timings use nine interleaved
rounds with rotating arm order and a 2 ms calibration target (not a guaranteed
minimum), capped at 1,000 iterations. Outputs include quartiles and page actions.
The instrumented planning-frame count visits every page independently; it is
not a count of complete-query I/O. This resident pilot makes no cold-I/O claim.

Run `experiments/run_evidence_layout.sh --smoke` first, then run without arguments.
Results go to a fresh timestamped directory beneath
`experiments/results/evidence_layout/`; no claim-bearing CSV is overwritten.
`summary.md` and `decision.json` record the gate and its limitations.
