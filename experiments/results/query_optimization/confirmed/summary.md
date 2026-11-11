# Prepared queries, access costs, and surviving order

Validated 1823 shapes, 64262 warm timing cells, and 84384 measured end-to-end cells. All exact SUM/count checks agree.

Ratios below one favor the named optimization. Every selected natural case is included, including zero-useful-region cases. Synthetic cases are separate. No canonical measurement is replaced automatically.

## Natural region prevalence

| Split | Rows/page | Pairs | Globally ordered | Broken order with multi-page regions | No multi-page regions | Longest region (pages) |
|---|---:|---:|---:|---:|---:|---:|
| development | 1,024 | 71 | 0 | 17 | 54 | 734 |
| development | 16,384 | 71 | 0 | 7 | 64 | 45 |
| heldout | 1,024 | 293 | 11 | 73 | 209 | 1024 |
| heldout | 16,384 | 293 | 11 | 25 | 257 | 64 |

## Warm query comparisons

| Split | Rows/page | Layout | Comparison | Geo. ratio | Source-weighted ratio | >20% regressions | Worst ratio |
|---|---:|---|---|---:|---:|---:|---:|
| development | 1,024 | direct | cost_facts | 1.019 | 1.023 | 1/242 | 1.21 |
| development | 1,024 | direct | cost_zonemapplus | 1.000 | 1.000 | 0/242 | 1.02 |
| development | 1,024 | direct | prepared_facts | 0.991 | 0.989 | 0/242 | 1.01 |
| development | 1,024 | direct | prepared_regional_facts | 0.972 | 0.967 | 0/242 | 1.03 |
| development | 1,024 | direct | prepared_regional_zonemapplus | 0.988 | 0.986 | 0/242 | 1.03 |
| development | 1,024 | direct | prepared_zonemapplus | 0.986 | 0.983 | 0/242 | 1.01 |
| development | 1,024 | direct | regions_facts | 0.959 | 0.942 | 0/242 | 1.12 |
| development | 1,024 | direct | regions_zonemapplus | 0.974 | 0.962 | 0/242 | 1.04 |
| development | 16,384 | compact | cost_facts | 1.002 | 1.002 | 0/242 | 1.02 |
| development | 16,384 | compact | cost_zonemapplus | 1.000 | 1.000 | 0/242 | 1.01 |
| development | 16,384 | compact | prepared_facts | 0.999 | 0.999 | 0/242 | 1.01 |
| development | 16,384 | compact | prepared_regional_facts | 0.997 | 0.997 | 0/242 | 1.01 |
| development | 16,384 | compact | prepared_regional_zonemapplus | 0.999 | 0.999 | 0/242 | 1.02 |
| development | 16,384 | compact | prepared_zonemapplus | 0.999 | 0.999 | 0/242 | 1.01 |
| development | 16,384 | compact | regions_facts | 0.999 | 0.999 | 0/242 | 1.01 |
| development | 16,384 | compact | regions_zonemapplus | 1.001 | 1.001 | 0/242 | 1.10 |
| development | 16,384 | direct | cost_facts | 1.004 | 1.005 | 0/242 | 1.17 |
| development | 16,384 | direct | cost_zonemapplus | 1.002 | 1.002 | 0/242 | 1.10 |
| development | 16,384 | direct | prepared_facts | 0.999 | 0.999 | 0/242 | 1.01 |
| development | 16,384 | direct | prepared_regional_facts | 0.996 | 0.996 | 0/242 | 1.00 |
| development | 16,384 | direct | prepared_regional_zonemapplus | 0.999 | 0.998 | 0/242 | 1.01 |
| development | 16,384 | direct | prepared_zonemapplus | 0.998 | 0.998 | 0/242 | 1.01 |
| development | 16,384 | direct | regions_facts | 0.999 | 0.999 | 0/242 | 1.00 |
| development | 16,384 | direct | regions_zonemapplus | 1.000 | 1.000 | 0/242 | 1.01 |
| development | 16,384 | payload | cost_facts | 1.002 | 1.002 | 0/242 | 1.02 |
| development | 16,384 | payload | cost_zonemapplus | 1.000 | 1.000 | 0/242 | 1.01 |
| development | 16,384 | payload | prepared_facts | 0.999 | 0.999 | 0/242 | 1.01 |
| development | 16,384 | payload | prepared_regional_facts | 0.996 | 0.996 | 0/242 | 1.01 |
| development | 16,384 | payload | prepared_regional_zonemapplus | 0.999 | 0.998 | 0/242 | 1.01 |
| development | 16,384 | payload | prepared_zonemapplus | 0.998 | 0.998 | 0/242 | 1.01 |
| development | 16,384 | payload | regions_facts | 1.001 | 1.002 | 0/242 | 1.19 |
| development | 16,384 | payload | regions_zonemapplus | 1.002 | 1.003 | 1/242 | 1.22 |
| development | 16,384 | whole | cost_facts | 1.002 | 1.002 | 0/242 | 1.01 |
| development | 16,384 | whole | cost_zonemapplus | 1.000 | 1.000 | 0/242 | 1.01 |
| development | 16,384 | whole | prepared_facts | 0.999 | 0.999 | 0/242 | 1.01 |
| development | 16,384 | whole | prepared_regional_facts | 0.997 | 0.996 | 0/242 | 1.01 |
| development | 16,384 | whole | prepared_regional_zonemapplus | 0.999 | 0.999 | 0/242 | 1.01 |
| development | 16,384 | whole | prepared_zonemapplus | 0.999 | 0.998 | 0/242 | 1.00 |
| development | 16,384 | whole | regions_facts | 0.999 | 0.999 | 0/242 | 1.01 |
| development | 16,384 | whole | regions_zonemapplus | 1.001 | 1.002 | 0/242 | 1.13 |
| heldout | 1,024 | direct | cost_facts | 1.025 | 1.026 | 9/924 | 1.53 |
| heldout | 1,024 | direct | cost_zonemapplus | 1.000 | 1.000 | 2/924 | 1.34 |
| heldout | 1,024 | direct | prepared_facts | 0.987 | 0.987 | 0/924 | 1.01 |
| heldout | 1,024 | direct | prepared_regional_facts | 0.963 | 0.962 | 0/924 | 1.01 |
| heldout | 1,024 | direct | prepared_regional_zonemapplus | 0.980 | 0.980 | 0/924 | 1.02 |
| heldout | 1,024 | direct | prepared_zonemapplus | 0.979 | 0.979 | 0/924 | 1.01 |
| heldout | 1,024 | direct | regions_facts | 0.968 | 0.951 | 0/924 | 1.12 |
| heldout | 1,024 | direct | regions_zonemapplus | 0.978 | 0.967 | 0/924 | 1.08 |
| heldout | 16,384 | compact | cost_facts | 1.002 | 1.002 | 0/924 | 1.02 |
| heldout | 16,384 | compact | cost_zonemapplus | 1.001 | 1.001 | 0/924 | 1.05 |
| heldout | 16,384 | compact | prepared_facts | 0.999 | 0.999 | 0/924 | 1.01 |
| heldout | 16,384 | compact | prepared_regional_facts | 0.996 | 0.996 | 0/924 | 1.01 |
| heldout | 16,384 | compact | prepared_regional_zonemapplus | 0.998 | 0.998 | 0/924 | 1.01 |
| heldout | 16,384 | compact | prepared_zonemapplus | 0.998 | 0.998 | 0/924 | 1.01 |
| heldout | 16,384 | compact | regions_facts | 1.000 | 1.000 | 0/924 | 1.01 |
| heldout | 16,384 | compact | regions_zonemapplus | 1.000 | 1.001 | 1/924 | 1.22 |
| heldout | 16,384 | direct | cost_facts | 1.006 | 1.007 | 5/924 | 1.84 |
| heldout | 16,384 | direct | cost_zonemapplus | 1.003 | 1.004 | 5/924 | 1.85 |
| heldout | 16,384 | direct | prepared_facts | 0.998 | 0.998 | 0/924 | 1.01 |
| heldout | 16,384 | direct | prepared_regional_facts | 0.995 | 0.995 | 0/924 | 1.02 |
| heldout | 16,384 | direct | prepared_regional_zonemapplus | 0.998 | 0.998 | 0/924 | 1.02 |
| heldout | 16,384 | direct | prepared_zonemapplus | 0.997 | 0.997 | 0/924 | 1.01 |
| heldout | 16,384 | direct | regions_facts | 1.000 | 1.000 | 0/924 | 1.04 |
| heldout | 16,384 | direct | regions_zonemapplus | 1.000 | 1.000 | 0/924 | 1.07 |
| heldout | 16,384 | payload | cost_facts | 1.003 | 1.003 | 0/924 | 1.04 |
| heldout | 16,384 | payload | cost_zonemapplus | 1.000 | 1.000 | 0/924 | 1.02 |
| heldout | 16,384 | payload | prepared_facts | 0.999 | 0.998 | 0/924 | 1.01 |
| heldout | 16,384 | payload | prepared_regional_facts | 0.995 | 0.995 | 0/924 | 1.00 |
| heldout | 16,384 | payload | prepared_regional_zonemapplus | 0.998 | 0.998 | 0/924 | 1.00 |
| heldout | 16,384 | payload | prepared_zonemapplus | 0.998 | 0.998 | 0/924 | 1.01 |
| heldout | 16,384 | payload | regions_facts | 1.002 | 1.004 | 3/924 | 1.84 |
| heldout | 16,384 | payload | regions_zonemapplus | 1.002 | 1.005 | 3/924 | 2.22 |
| heldout | 16,384 | whole | cost_facts | 1.002 | 1.003 | 0/924 | 1.06 |
| heldout | 16,384 | whole | cost_zonemapplus | 1.000 | 1.000 | 0/924 | 1.02 |
| heldout | 16,384 | whole | prepared_facts | 0.998 | 0.998 | 0/924 | 1.01 |
| heldout | 16,384 | whole | prepared_regional_facts | 0.996 | 0.996 | 0/924 | 1.00 |
| heldout | 16,384 | whole | prepared_regional_zonemapplus | 0.998 | 0.998 | 0/924 | 1.01 |
| heldout | 16,384 | whole | prepared_zonemapplus | 0.998 | 0.998 | 0/924 | 1.01 |
| heldout | 16,384 | whole | regions_facts | 1.000 | 0.999 | 0/924 | 1.02 |
| heldout | 16,384 | whole | regions_zonemapplus | 1.001 | 1.002 | 2/924 | 1.50 |

## Measured resident load-to-answer totals

| Split | Rows/page | Repeated queries | Comparison | Geo. ratio | Source-weighted ratio |
|---|---:|---:|---|---:|---:|
| development | 1,024 | 1 | prepared_facts | 1.000 | 1.000 |
| development | 1,024 | 1 | prepared_zonemapplus | 1.000 | 1.000 |
| development | 1,024 | 1 | regions_facts | 0.998 | 0.998 |
| development | 1,024 | 1 | regions_zonemapplus | 1.000 | 0.999 |
| development | 1,024 | 2 | prepared_facts | 0.999 | 0.999 |
| development | 1,024 | 2 | prepared_zonemapplus | 1.000 | 1.000 |
| development | 1,024 | 2 | regions_facts | 0.997 | 0.995 |
| development | 1,024 | 2 | regions_zonemapplus | 0.999 | 0.999 |
| development | 1,024 | 4 | prepared_facts | 1.000 | 0.999 |
| development | 1,024 | 4 | prepared_zonemapplus | 0.999 | 1.000 |
| development | 1,024 | 4 | regions_facts | 0.995 | 0.994 |
| development | 1,024 | 4 | regions_zonemapplus | 0.998 | 0.997 |
| development | 1,024 | 8 | prepared_facts | 0.999 | 0.999 |
| development | 1,024 | 8 | prepared_zonemapplus | 0.999 | 0.999 |
| development | 1,024 | 8 | regions_facts | 0.992 | 0.989 |
| development | 1,024 | 8 | regions_zonemapplus | 0.997 | 0.995 |
| development | 1,024 | 16 | prepared_facts | 1.000 | 0.999 |
| development | 1,024 | 16 | prepared_zonemapplus | 0.999 | 0.999 |
| development | 1,024 | 16 | regions_facts | 0.987 | 0.982 |
| development | 1,024 | 16 | regions_zonemapplus | 0.994 | 0.992 |
| development | 1,024 | 32 | prepared_facts | 0.999 | 0.999 |
| development | 1,024 | 32 | prepared_zonemapplus | 0.999 | 0.999 |
| development | 1,024 | 32 | regions_facts | 0.981 | 0.973 |
| development | 1,024 | 32 | regions_zonemapplus | 0.991 | 0.987 |
| development | 16,384 | 1 | prepared_facts | 1.000 | 1.000 |
| development | 16,384 | 1 | prepared_zonemapplus | 1.000 | 1.000 |
| development | 16,384 | 1 | regions_facts | 1.000 | 1.000 |
| development | 16,384 | 1 | regions_zonemapplus | 1.000 | 1.000 |
| development | 16,384 | 2 | prepared_facts | 1.000 | 1.000 |
| development | 16,384 | 2 | prepared_zonemapplus | 1.000 | 1.000 |
| development | 16,384 | 2 | regions_facts | 1.000 | 1.000 |
| development | 16,384 | 2 | regions_zonemapplus | 1.000 | 1.000 |
| development | 16,384 | 4 | prepared_facts | 1.000 | 1.000 |
| development | 16,384 | 4 | prepared_zonemapplus | 1.000 | 1.000 |
| development | 16,384 | 4 | regions_facts | 1.000 | 1.000 |
| development | 16,384 | 4 | regions_zonemapplus | 1.000 | 1.000 |
| development | 16,384 | 8 | prepared_facts | 1.000 | 1.000 |
| development | 16,384 | 8 | prepared_zonemapplus | 1.000 | 1.000 |
| development | 16,384 | 8 | regions_facts | 1.000 | 1.000 |
| development | 16,384 | 8 | regions_zonemapplus | 1.000 | 1.000 |
| development | 16,384 | 16 | prepared_facts | 1.000 | 1.000 |
| development | 16,384 | 16 | prepared_zonemapplus | 1.000 | 1.000 |
| development | 16,384 | 16 | regions_facts | 1.000 | 1.000 |
| development | 16,384 | 16 | regions_zonemapplus | 1.000 | 1.000 |
| development | 16,384 | 32 | prepared_facts | 1.000 | 1.000 |
| development | 16,384 | 32 | prepared_zonemapplus | 1.000 | 1.000 |
| development | 16,384 | 32 | regions_facts | 1.000 | 1.000 |
| development | 16,384 | 32 | regions_zonemapplus | 1.000 | 1.000 |
| heldout | 1,024 | 1 | prepared_facts | 1.000 | 1.000 |
| heldout | 1,024 | 1 | prepared_zonemapplus | 1.000 | 1.000 |
| heldout | 1,024 | 1 | regions_facts | 0.999 | 0.999 |
| heldout | 1,024 | 1 | regions_zonemapplus | 1.000 | 1.000 |
| heldout | 1,024 | 2 | prepared_facts | 0.999 | 0.999 |
| heldout | 1,024 | 2 | prepared_zonemapplus | 1.000 | 1.000 |
| heldout | 1,024 | 2 | regions_facts | 0.998 | 0.997 |
| heldout | 1,024 | 2 | regions_zonemapplus | 1.000 | 0.999 |
| heldout | 1,024 | 4 | prepared_facts | 0.999 | 0.999 |
| heldout | 1,024 | 4 | prepared_zonemapplus | 1.000 | 1.000 |
| heldout | 1,024 | 4 | regions_facts | 0.996 | 0.995 |
| heldout | 1,024 | 4 | regions_zonemapplus | 0.998 | 0.998 |
| heldout | 1,024 | 8 | prepared_facts | 0.999 | 0.999 |
| heldout | 1,024 | 8 | prepared_zonemapplus | 0.999 | 0.999 |
| heldout | 1,024 | 8 | regions_facts | 0.993 | 0.991 |
| heldout | 1,024 | 8 | regions_zonemapplus | 0.997 | 0.996 |
| heldout | 1,024 | 16 | prepared_facts | 0.999 | 0.998 |
| heldout | 1,024 | 16 | prepared_zonemapplus | 0.999 | 0.999 |
| heldout | 1,024 | 16 | regions_facts | 0.988 | 0.985 |
| heldout | 1,024 | 16 | regions_zonemapplus | 0.995 | 0.993 |
| heldout | 1,024 | 32 | prepared_facts | 0.998 | 0.997 |
| heldout | 1,024 | 32 | prepared_zonemapplus | 0.999 | 0.998 |
| heldout | 1,024 | 32 | regions_facts | 0.983 | 0.978 |
| heldout | 1,024 | 32 | regions_zonemapplus | 0.992 | 0.989 |
| heldout | 16,384 | 1 | prepared_facts | 1.000 | 1.000 |
| heldout | 16,384 | 1 | prepared_zonemapplus | 1.000 | 1.000 |
| heldout | 16,384 | 1 | regions_facts | 1.000 | 1.000 |
| heldout | 16,384 | 1 | regions_zonemapplus | 1.000 | 1.000 |
| heldout | 16,384 | 2 | prepared_facts | 1.000 | 1.000 |
| heldout | 16,384 | 2 | prepared_zonemapplus | 1.000 | 1.000 |
| heldout | 16,384 | 2 | regions_facts | 1.000 | 1.000 |
| heldout | 16,384 | 2 | regions_zonemapplus | 1.000 | 1.000 |
| heldout | 16,384 | 4 | prepared_facts | 1.000 | 1.000 |
| heldout | 16,384 | 4 | prepared_zonemapplus | 1.000 | 1.000 |
| heldout | 16,384 | 4 | regions_facts | 1.000 | 1.000 |
| heldout | 16,384 | 4 | regions_zonemapplus | 1.000 | 1.000 |
| heldout | 16,384 | 8 | prepared_facts | 1.000 | 1.000 |
| heldout | 16,384 | 8 | prepared_zonemapplus | 1.000 | 1.000 |
| heldout | 16,384 | 8 | regions_facts | 1.000 | 1.000 |
| heldout | 16,384 | 8 | regions_zonemapplus | 1.000 | 1.000 |
| heldout | 16,384 | 16 | prepared_facts | 1.000 | 1.000 |
| heldout | 16,384 | 16 | prepared_zonemapplus | 1.000 | 1.000 |
| heldout | 16,384 | 16 | regions_facts | 1.000 | 1.000 |
| heldout | 16,384 | 16 | regions_zonemapplus | 1.000 | 1.000 |
| heldout | 16,384 | 32 | prepared_facts | 0.999 | 0.999 |
| heldout | 16,384 | 32 | prepared_zonemapplus | 1.000 | 1.000 |
| heldout | 16,384 | 32 | regions_facts | 1.000 | 1.000 |
| heldout | 16,384 | 32 | regions_zonemapplus | 1.000 | 1.000 |

Totals are directly measured from already resident serialized bytes through reconstructing both selector and value columns, optional region construction, optional preparation, Q repeated identical queries, and teardown. They exclude filesystem reads and encoding. Nine rounds rotate arm order; warm queries use a 2 ms calibration target, not a guaranteed minimum. Timed closures include shared exact-answer checks.

The JSON gives every measured total ratio and per-predicate break-even. Modeled incremental break-even divides extra region/context setup by warm savings, clamping negative setup estimates to zero as sampling noise and requiring at least one query. First measured win reports only the first tested Q with lower median. The sustained 5% marker requires a ratio at most 0.95 at that Q and every later tested Q; it remains descriptive, not statistical certainty. Common initialization is included in measured totals rather than credited as free. Region entry bytes count used span payload, excluding vector spare capacity and allocator overhead. Same-shortcut zone-map arms receive the same preparation, regions, and cost gate.

This diagnostic does not establish a new search algorithm, a cold-read speedup, or representative prevalence beyond its declared source sample. The cost policy uses descriptions, not query timings. Paper inclusion requires review of effect sizes, repeated evidence, controls, and scope; smoke evidence cannot support natural-data claims.

## Frozen engineering decision

Prepared-context gate passed: **True**. Cost gate passed: **False**.

The prepared gate requires at least 10% warm gain on the selective 4,096-page synthetic regional query and held-out direct-layout source-weighted time at most 1.05x. The cost gate requires held-out pooled time at most 0.95x, source-weighted time at most 1.00x, and no more than 5% of query cells regressing over 20%. Gates are engineering criteria, not significance tests. Paired source-level bootstrap intervals use 1,000 resamples and seed 20261025; single-source intervals carry no cross-source uncertainty.
