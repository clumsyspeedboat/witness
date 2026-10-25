# Evidence-layout diagnostic

all SUM/count checks passed.

Ratios below one favor the candidate. Bytes pool selector/value pairs; latency is the geometric mean over complete queries.

| Split | Layout | Bytes/direct | Time/direct | Source-weighted time | Time/same-layout +shortcuts | >20% regressions | Worst time ratio |
|---|---|---:|---:|---:|---:|---:|---:|
| smoke | direct | 1.000 | 1.000 | 1.000 | 1.012 | 0 | 1.00 |
| smoke | compact | 0.131 | 9.481 | 9.481 | 1.030 | 12 | 24.73 |
| smoke | whole | 0.545 | 3.718 | 3.718 | 1.010 | 12 | 9.17 |
| smoke | whole_4k | 0.687 | 4.019 | 4.019 | 1.039 | 12 | 23.64 |
| smoke | payload | 0.552 | 3.409 | 3.409 | 1.005 | 8 | 9.15 |
| smoke | payload_4k | 0.697 | 3.090 | 3.090 | 1.029 | 12 | 19.46 |

Promote to broader rerun: **False**. Candidates: none.

The frozen engineering gate requires bytes/direct <=0.80 and Facts/direct Facts time <=1.05 on held-out sources. Smoke results cannot promote a candidate. The four matched menus share logical codec choices, but size selection can choose different recipes per page; placement/chunking comparisons include that selection effect. Compact additionally introduces delta codecs.

The first query includes initially uncached dictionary-order checks; encoding is separate. The timing table uses warm queries. Instrumented per-page planning counts do not measure full-query I/O. First-query ratios, sizes, frame counts, recipes, and all baseline arms are in the CSVs. No claim-bearing results were replaced.
