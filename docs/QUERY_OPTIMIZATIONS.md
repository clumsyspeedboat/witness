# Prepared queries, access costs, and surviving order

This study evaluates three opt-in changes independently of the frozen main
ablation. A fresh output directory records its source fingerprint, workload,
initialization costs, complete timing matrices, and exact SUM/count checks.
It never rewrites canonical claims or uses measured queries to tune the gate.

```bash
./experiments/run_query_optimization.sh --smoke
./experiments/run_query_optimization.sh --pilot
./experiments/run_query_optimization.sh --all
```

The worker is pinned to one available CPU and takes the same cooperative lock
as the other engineering diagnostics. The host is shared. Native serialized
buffers are resident; filesystem reads and writer encoding are excluded.

## Frozen workload and controls

The pilot reuses the canonical paged-study corpus and quantile-window rules:
at most 1,048,576 rows per column, original input order, and centered 0.1%, 1%,
10%, and 50% windows, merging identical predicates. FNV-1a of group, a zero
byte, and source assigns whole sources to development (hash modulo three
equals zero) or held out. Before timing, eight development and sixteen
held-out sources are selected evenly from their sorted names, with one
deterministically selected pair per source. `--all` instead measures every
canonical pair; no pair or query is excluded because its regions are absent,
short, or unhelpful.

Direct pages use 1,024 and 16,384 rows. At the latter size, matched whole-frame,
exposed-payload-frame, and compact menus also test cost-aware choice. The first
two framing menus retain the same logical direct recipes, but serialized-size
selection may choose a different recipe on a page; compact additionally permits
delta recipes. Results therefore compare selected menus, not a fixed-recipe
causal framing intervention. The main ablation's direct menu is unchanged.

Eleven warm arms separate default/prepared execution, default/regional order,
Facts/the same-shortcuts zone map, and deterministic cost choice. Prepared
scan is a further control. Every arm shares decoder, checker, and aggregation
kernels. Initialization is complete and dictionary order verified before warm
timing. Nine rounds rotate arm order and calibrate toward 2 ms per sample,
capped at 1,000 iterations; that target is not a guaranteed duration. Shared
answer checks remain inside timed closures.

Synthetic globally sorted, nullable, and unordered cases form the smoke test.
Four appended sorted batches at 64, 512, and 4,096 pages separately expose
prepared-context scaling after global order breaks. Synthetic values are
never included in natural-source prevalence or pooled natural timing ratios.

## Initialization and amortization

Each shape reports nine samples of reconstructing both selector and value
columns from resident native buffers, including checksum verification,
description parsing, dictionary-order verification, and fact construction.
Separate samples measure region construction and full alignment preparation.
Region entry bytes are the used span payload, excluding vector spare capacity
and allocator bookkeeping; they are not total resident process memory.

For direct pages, the study directly measures bytes-to-answer totals at
1, 2, 4, 8, 16, and 32 identical query executions. Each timed closure opens
both columns, optionally constructs regions and/or prepares the query,
executes the queries, checks exact answers, and drops its temporary reader
objects. Six arms compare default, prepared, and regional-prepared Facts and
the corresponding three same-shortcuts zone-map variants. Common opening
cost is charged to every arm. These are resident load-to-answer measurements,
not cold storage or a mixed-query workload.

The summary reports measured total ratios at every tested repetition count,
including cases that never win. A modeled incremental region break-even
divides region/context setup overhead by warm savings. The first measured
win means only the first tested Q with a lower median; it does not imply
monotonic benefit, statistical certainty, or a general workload threshold.

## Decision protocol fixed before measurement

Prepared contexts require exactness in all regression tests, at least 10%
warm improvement on the selective four-batch 4,096-page regional query, and
held-out direct-layout source-weighted time at most 1.05x the ordinary API.
The synthetic comparison changes preparation alone while keeping regions
enabled in both arms; it does not credit preparation with region pruning.

A positive cost-gate claim requires held-out pooled geometric-mean time at
most 0.95x, source-weighted time at most 1.00x, and no more than 5% of query
cells regressing by over 20%. The identical gate is supplied to both Facts
and the same-shortcuts zone map; both outcomes and every layout are reported.
Otherwise it remains an experimental negative result. The pooled ratio weights
the predeclared page/layout/query matrix, not a production workload mix.

These are engineering gates, not significance tests. Paired source-level
bootstrap intervals use 1,000 resamples with seed 20261025. Queries and layouts
from a source move together. Query-weighted and equal-source-weighted ratios
and intervals are both retained, alongside tail regressions. A single-source
interval cannot express cross-source uncertainty.

Natural-region evidence is reviewed separately for prevalence, initialization,
memory scope, strong controls, and measured amortization. A scoped negative
or limited-use finding may be useful to the paper; this study does not
automatically promote an optimization or infer novelty from a speedup.

## Confirmed all-pairs result

The confirmed `--all` run is stored under
`experiments/results/query_optimization/confirmed/`. It covers 364 natural
selector/value pairs from 63 sources, plus three synthetic scaling cases, and
records 64,262 warm query cells and 84,384 repeated-query total cells. Every
SUM and COUNT matched the exact oracle; the strict summarizer and recorded file
checksums passed.

Prepared queries passed the frozen engineering gate. In the 50 held-out
sources (293 pairs), the source-weighted geometric-mean warm-time ratio was
0.992 versus ordinary Facts (source-bootstrap 95% interval 0.991–0.994).
On the selective four-batch, 4,096-page synthetic case, prepared regional
Facts took 0.472 times the warm query time of unprepared regional Facts.
These are resident warm-query effects; the total cells separately charge
reader setup, region construction, context preparation, repeated queries,
answer checks, and teardown.

The identical heuristic access-cost gate was tested for Facts and the strong
zone-map control. Neither improved held-out timing enough to pass the frozen
gate; retain these planners as experimental options and do not promote them as
a paper performance claim. Natural multi-page regions appear in 73 of 293
held-out pairs at 1,024 rows per page and 25 of 293 at 16,384 rows per page.
The full result has its own source fingerprint, `c32bdf9a1fb23e74`; it is not
relabelled as a measurement from the frozen main ablation.
