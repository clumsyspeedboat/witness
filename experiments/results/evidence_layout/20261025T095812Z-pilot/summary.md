# Evidence-layout diagnostic

all SUM/count checks passed.

Ratios below one favor the candidate. Bytes pool selector/value pairs; latency is the geometric mean over complete queries.

| Split | Layout | Bytes/direct | Time/direct | Source-weighted time | Time/same-layout +shortcuts | >20% regressions | Worst time ratio |
|---|---|---:|---:|---:|---:|---:|---:|
| development | direct | 1.000 | 1.000 | 1.000 | 1.058 | 0 | 1.00 |
| development | compact | 0.752 | 2.817 | 2.390 | 1.180 | 24 | 20.36 |
| development | whole | 0.771 | 2.565 | 2.209 | 1.179 | 23 | 17.92 |
| development | whole_4k | 0.783 | 2.223 | 1.993 | 1.078 | 21 | 6.92 |
| development | payload | 0.925 | 1.512 | 1.418 | 1.037 | 18 | 3.15 |
| development | payload_4k | 0.939 | 1.623 | 1.513 | 1.053 | 17 | 3.74 |
| heldout | direct | 1.000 | 1.000 | 1.000 | 1.080 | 0 | 1.00 |
| heldout | compact | 0.632 | 5.854 | 5.010 | 1.362 | 50 | 159.16 |
| heldout | whole | 0.655 | 4.486 | 3.939 | 1.221 | 50 | 46.93 |
| heldout | whole_4k | 0.675 | 4.209 | 3.778 | 1.157 | 50 | 31.09 |
| heldout | payload | 0.783 | 2.302 | 2.220 | 1.053 | 46 | 7.48 |
| heldout | payload_4k | 0.812 | 3.143 | 2.926 | 1.064 | 50 | 20.88 |

Promote to broader rerun: **False**. Candidates: none.

The frozen engineering gate requires bytes/direct <=0.80 and Facts/direct Facts time <=1.05 on held-out sources. Smoke results cannot promote a candidate. The four matched menus share logical codec choices, but size selection can choose different recipes per page; placement/chunking comparisons include that selection effect. Compact additionally introduces delta codecs.

The first query includes initially uncached dictionary-order checks; encoding is separate. The timing table uses warm queries. Instrumented per-page planning counts do not measure full-query I/O. First-query ratios, sizes, frame counts, recipes, and all baseline arms are in the CSVs. No claim-bearing results were replaced.
