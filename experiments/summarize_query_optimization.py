#!/usr/bin/env python3
"""Validate and summarize the opt-in query-optimization study.

Summaries include every selected source, including zero-useful-region cases.
Synthetic scaling is kept separate; no diagnostic automatically replaces a
canonical claim or chooses a cost-model threshold from measured queries.
"""

import csv
import json
import math
from pathlib import Path
import random
import statistics
import sys


ARMS = (
    "facts_default", "facts_prepared", "facts_regions", "facts_regions_prepared",
    "zonemapplus_default", "zonemapplus_prepared", "zonemapplus_regions",
    "zonemapplus_regions_prepared", "facts_cost_prepared", "zonemapplus_cost_prepared",
    "scan_prepared",
)
TOTAL_ARMS = (
    "facts_default", "facts_prepared", "facts_regions_prepared", "zonemapplus_default",
    "zonemapplus_prepared", "zonemapplus_regions_prepared",
)
REPETITIONS = (1, 2, 4, 8, 16, 32)
LAYOUTS = ("direct", "whole", "payload", "compact")


def read_csv(path):
    with Path(path).open(newline="") as stream:
        reader = csv.DictReader(stream)
        if not reader.fieldnames or len(reader.fieldnames) != len(set(reader.fieldnames)):
            raise ValueError(f"{path}: missing or duplicate CSV header")
        rows = list(reader)
    if any(None in row or any(value is None for value in row.values()) for row in rows):
        raise ValueError(f"{path}: malformed CSV row")
    return rows


def integer(row, name, minimum=0):
    value = int(row[name])
    if value < minimum:
        raise ValueError(f"{name} is below {minimum}")
    return value


def timing(row, prefix=""):
    values = [float(row[prefix + name + "_ns"]) for name in ("p25", "median", "p75")]
    if any(not math.isfinite(value) or value <= 0 for value in values) or values != sorted(values):
        raise ValueError("nonpositive, nonfinite, or unordered timing quartiles")
    return values[1]


def shape_key(row):
    return integer(row, "case"), integer(row, "page_rows", 1), row["layout"]


def source_hash(group, source):
    value = 0xCBF29CE484222325
    for byte in (group + "\0" + source).encode():
        value = ((value ^ byte) * 0x100000001B3) & ((1 << 64) - 1)
    return value


def geo(values):
    return math.exp(statistics.mean(math.log(value) for value in values))


def ratio_summary(values):
    ratios = [ratio for _, ratio in values]
    sources = {}
    for source, ratio in values:
        sources.setdefault(source, []).append(ratio)
    source_logs = [(sum(math.log(value) for value in items), len(items)) for items in sources.values()]
    rng = random.Random(20261025)
    query_bootstrap, source_bootstrap = [], []
    for _ in range(1000):
        sampled = rng.choices(source_logs, k=len(source_logs))
        query_bootstrap.append(math.exp(sum(item[0] for item in sampled) / sum(item[1] for item in sampled)))
        source_bootstrap.append(math.exp(statistics.mean(item[0] / item[1] for item in sampled)))
    query_bootstrap.sort()
    source_bootstrap.sort()
    return {
        "queries": len(ratios), "sources": len(sources),
        "geometric_mean": geo(ratios), "median": statistics.median(ratios),
        "source_weighted_geometric_mean": geo([geo(items) for items in sources.values()]),
        "geometric_mean_source_bootstrap_ci95": [query_bootstrap[24], query_bootstrap[974]],
        "source_weighted_source_bootstrap_ci95": [source_bootstrap[24], source_bootstrap[974]],
        "regressions_over_20_percent": sum(value > 1.20 for value in ratios),
        "improvements_over_10_percent": sum(value < .90 for value in ratios),
        "worst_ratio": max(ratios),
    }


def analyze(directory):
    directory = Path(directory)
    freeze_rows = read_csv(directory / "freeze.csv")
    freeze = {}
    for row in freeze_rows:
        if row["key"] in freeze and row["key"] != "status":
            raise ValueError("duplicate freeze entry")
        freeze[row["key"]] = row["value"]
    if freeze.get("status") != "complete":
        raise ValueError("study is incomplete")
    fingerprint = freeze.get("rule_fingerprint", "")
    if len(fingerprint.removeprefix("0x")) != 16:
        raise ValueError("missing or malformed source fingerprint")
    int(fingerprint.removeprefix("0x"), 16)
    protocol = "prepared_selective_synthetic_at_least10pct_gain_heldout_direct_sourceweighted_ratio_at_most1.05_cost_heldout_geo_at_most0.95_sourceweighted_at_most1_regressions20pct_at_most5pct"
    if freeze.get("decision_protocol") != protocol or freeze.get("bootstrap_seed") != "20261025" or freeze.get("bootstrap_replicates") != "1000":
        raise ValueError("decision protocol or bootstrap configuration changed")
    if freeze.get("mode") not in ("smoke", "pilot", "all") or int(freeze["rounds"]) != 9:
        raise ValueError("unexpected study mode or timing rounds")
    if freeze.get("cost_gate") != "no_measured_query_tuning" or int(freeze["arm_count"]) != len(ARMS):
        raise ValueError("unexpected cost policy or arm coverage")
    if freeze.get("warm_scope") != "resident_after_initialization_regions_and_preparation":
        raise ValueError("warm timing scope changed")
    if freeze.get("total_scope") != "resident_bytes_to_answer_including_both_columns_optional_regions_preparation_and_teardown":
        raise ValueError("total timing scope changed")
    if freeze.get("repetitions") != "|".join(map(str, REPETITIONS)):
        raise ValueError("repeated-query coverage changed")
    shapes = {}
    source_cases = {}
    for row in read_csv(directory / "shapes.csv"):
        if row.get("rule_fingerprint") != fingerprint:
            raise ValueError("shape source fingerprint disagrees with freeze")
        key = shape_key(row)
        if key in shapes:
            raise ValueError("duplicate shape")
        if row["layout"] not in LAYOUTS or key[1] not in (1024, 16384):
            raise ValueError("unknown page shape or layout")
        if key[1] == 1024 and row["layout"] != "direct":
            raise ValueError("unexpected small-page framed case")
        rows, pages = integer(row, "rows", 1), integer(row, "pages", 1)
        if pages != (rows + key[1] - 1) // key[1]:
            raise ValueError("shape row/page accounting disagrees")
        if row["global_ordered"] not in ("true", "false"):
            raise ValueError("invalid whole-column order flag")
        regions = integer(row, "regions")
        certified = integer(row, "certified_pages")
        longest = integer(row, "longest_region")
        multi = integer(row, "multi_page_regions")
        if not regions <= certified <= pages or not multi <= regions or not longest <= certified:
            raise ValueError("invalid regional coverage")
        if (regions == 0) != (certified == 0) or (regions == 0) != (longest == 0):
            raise ValueError("empty region accounting disagrees")
        if row["global_ordered"] == "true" and (regions != 1 or longest != pages):
            raise ValueError("whole-column order is not preserved as one region")
        if regions and integer(row, "region_entry_bytes") != regions * 16:
            raise ValueError("region descriptor payload accounting disagrees")
        if not regions and integer(row, "region_entry_bytes"):
            raise ValueError("empty regions have descriptor bytes")
        for field in ("selector_file_bytes", "value_file_bytes", "predicates"):
            integer(row, field, 1)
        if int(row["predicates"]) > 4:
            raise ValueError("too many quantile predicates")
        for prefix in ("init_", "region_", "prepare_", "region_prepare_"):
            timing(row, prefix)
        source = row["group"], row["source"]
        if not all(source) or not row["selector"] or not row["value"]:
            raise ValueError("empty source identity")
        if row["split"] == "synthetic":
            if row["group"] != "synthetic":
                raise ValueError("natural source labeled synthetic")
        else:
            expected_split = "development" if source_hash(*source) % 3 == 0 else "heldout"
            if row["split"] != expected_split:
                raise ValueError("source-heldout partition disagrees")
            if rows > int(freeze["row_cap"]):
                raise ValueError("natural column exceeds row cap")
        if key[0] in source_cases and source_cases[key[0]] != source:
            raise ValueError("case identity changes across layouts")
        source_cases[key[0]] = source
        shapes[key] = row
    if len(shapes) != int(freeze["cases"]) or not shapes:
        raise ValueError("shape coverage differs from completed freeze")
    # Every selected natural pair (and every ordinary smoke pair) has both
    # direct page sizes and all three framing comparisons. Scaling cases are
    # separate one-layout synthetic files whose names identify their size.
    for case, source in source_cases.items():
        if source[0] == "synthetic" and source[1].startswith("four_batches_"):
            expected = {(case, 1024, "direct")}
        else:
            expected = {(case, 1024, "direct")} | {(case, 16384, layout) for layout in LAYOUTS}
        if {key for key in shapes if key[0] == case} != expected:
            raise ValueError("incomplete page/layout coverage")
    main_cases = {case for case, source in source_cases.items() if not (source[0] == "synthetic" and source[1].startswith("four_batches_"))}
    if len(main_cases) != int(freeze["pairs"]) or len({source_cases[case] for case in main_cases}) != int(freeze["sources"]):
        raise ValueError("pair/source coverage differs from completed freeze")
    scaling = {source[1] for source in source_cases.values() if source[0] == "synthetic" and source[1].startswith("four_batches_")}
    expected_scaling = {"four_batches_64"} if freeze["mode"] == "smoke" else {"four_batches_64", "four_batches_512", "four_batches_4096"}
    if scaling != expected_scaling:
        raise ValueError("incomplete synthetic scaling coverage")
    queries, predicates = {}, {}
    for row in read_csv(directory / "queries.csv"):
        if row.get("rule_fingerprint") != fingerprint:
            raise ValueError("query source fingerprint disagrees with freeze")
        skey = shape_key(row)
        if skey not in shapes or row["arm"] not in ARMS:
            raise ValueError("query has unknown shape or arm")
        predicate = integer(row, "predicate")
        if predicate >= int(shapes[skey]["predicates"]):
            raise ValueError("predicate index exceeds shape coverage")
        pkey = skey + (predicate,)
        key = pkey + (row["arm"],)
        if key in queries:
            raise ValueError("duplicate query timing cell")
        timing(row)
        expected_sum, expected_count = int(row["expected_sum"]), integer(row, "expected_count", 1)
        if int(row["sum"]) != expected_sum or integer(row, "matched") != expected_count:
            raise ValueError("query answer disagrees with exact oracle")
        if expected_count > int(shapes[skey]["rows"]) or int(row["low"]) > int(row["high"]):
            raise ValueError("invalid predicate or match count")
        requested = row["requested"].split("|")
        if len(set(requested)) != len(requested) or set(requested) - {"0.001", "0.010", "0.100", "0.500"}:
            raise ValueError("unknown or duplicate selectivity")
        identity = row["requested"], row["low"], row["high"], expected_sum, expected_count
        if pkey in predicates and predicates[pkey] != identity:
            raise ValueError("arm predicates or oracle values disagree")
        predicates[pkey] = identity
        for name in ("candidate_pages", "skipped", "accepted", "searched", "translated", "scanned"):
            if integer(row, name) > int(shapes[skey]["pages"]):
                raise ValueError("query action/candidate count exceeds pages")
        if sum(int(row[name]) for name in ("skipped", "accepted", "searched", "translated", "scanned")) != int(shapes[skey]["pages"]):
            raise ValueError("query action counts do not cover every selector page")
        queries[key] = row
    expected_queries = {skey + (predicate, arm) for skey, row in shapes.items()
                        for predicate in range(int(row["predicates"])) for arm in ARMS}
    if queries.keys() != expected_queries:
        raise ValueError("incomplete warm timing matrix")
    for pkey in predicates:
        for ordinary, prepared in (("facts_default", "facts_prepared"), ("facts_regions", "facts_regions_prepared"),
                                   ("zonemapplus_default", "zonemapplus_prepared"), ("zonemapplus_regions", "zonemapplus_regions_prepared")):
            if any(queries[pkey + (ordinary,)][field] != queries[pkey + (prepared,)][field]
                   for field in ("candidate_pages", "skipped", "accepted", "searched", "translated", "scanned")):
                raise ValueError("preparation changed a query's page actions")
    totals = {}
    for row in read_csv(directory / "totals.csv"):
        if row.get("rule_fingerprint") != fingerprint:
            raise ValueError("total source fingerprint disagrees with freeze")
        skey = shape_key(row)
        predicate = integer(row, "predicate")
        pkey = skey + (predicate,)
        key = pkey + (integer(row, "repetitions", 1), row["arm"])
        if skey not in shapes or pkey not in predicates or skey[2] != "direct":
            raise ValueError("total has unknown direct-layout predicate")
        if key in totals:
            raise ValueError("duplicate end-to-end timing cell")
        timing(row)
        if (int(row["sum"]), integer(row, "matched")) != predicates[pkey][3:]:
            raise ValueError("end-to-end answer disagrees with exact oracle")
        totals[key] = row
    expected_totals = {pkey + (repetitions, arm) for pkey in predicates if pkey[2] == "direct"
                       for repetitions in REPETITIONS for arm in TOTAL_ARMS}
    if totals.keys() != expected_totals:
        raise ValueError("incomplete end-to-end timing matrix")

    comparisons = (
        ("prepared_facts", "facts_prepared", "facts_default"),
        ("prepared_zonemapplus", "zonemapplus_prepared", "zonemapplus_default"),
        ("regions_facts", "facts_regions_prepared", "facts_prepared"),
        ("regions_zonemapplus", "zonemapplus_regions_prepared", "zonemapplus_prepared"),
        ("cost_facts", "facts_cost_prepared", "facts_prepared"),
        ("cost_zonemapplus", "zonemapplus_cost_prepared", "zonemapplus_prepared"),
        ("prepared_regional_facts", "facts_regions_prepared", "facts_regions"),
        ("prepared_regional_zonemapplus", "zonemapplus_regions_prepared", "zonemapplus_regions"),
    )
    warm_groups = {}
    for pkey in predicates:
        shape = shapes[pkey[:3]]
        for label, numerator, denominator in comparisons:
            group = shape["split"], pkey[1], pkey[2], label
            ratio = float(queries[pkey + (numerator,)]["median_ns"]) / float(queries[pkey + (denominator,)]["median_ns"])
            warm_groups.setdefault(group, []).append(((shape["group"], shape["source"]), ratio))
    warm = [{"split": key[0], "page_rows": key[1], "layout": key[2], "comparison": key[3], **ratio_summary(values)}
            for key, values in sorted(warm_groups.items())]
    pooled_groups = {}
    for key, values in warm_groups.items():
        pooled_groups.setdefault((key[0], key[3]), []).extend(values)
    pooled = [{"split": key[0], "comparison": key[1], **ratio_summary(values)}
              for key, values in sorted(pooled_groups.items())]
    total_groups = {}
    for pkey in predicates:
        if pkey[2] != "direct":
            continue
        shape = shapes[pkey[:3]]
        for repetitions in REPETITIONS:
            for label, numerator, denominator in comparisons[:4]:
                group = shape["split"], pkey[1], repetitions, label
                ratio = float(totals[pkey + (repetitions, numerator)]["median_ns"]) / float(totals[pkey + (repetitions, denominator)]["median_ns"])
                total_groups.setdefault(group, []).append(((shape["group"], shape["source"]), ratio))
    end_to_end = [{"split": key[0], "page_rows": key[1], "repetitions": key[2], "comparison": key[3], **ratio_summary(values)}
                  for key, values in sorted(total_groups.items())]
    prevalence = []
    for split in ("development", "heldout", "synthetic"):
        for page_rows in (1024, 16384):
            cases = [row for row in shapes.values() if row["split"] == split and int(row["page_rows"]) == page_rows and row["layout"] == "direct"]
            if not cases:
                continue
            prevalence.append({
                "split": split, "page_rows": page_rows, "pairs": len(cases),
                "sources": len({(row["group"], row["source"]) for row in cases}),
                "globally_ordered_pairs": sum(row["global_ordered"] == "true" for row in cases),
                "broken_global_order_with_multi_page_regions": sum(row["global_ordered"] == "false" and int(row["multi_page_regions"]) > 0 for row in cases),
                "pairs_with_no_multi_page_regions": sum(int(row["multi_page_regions"]) == 0 for row in cases),
                "regions": sum(int(row["regions"]) for row in cases),
                "pages": sum(int(row["pages"]) for row in cases),
                "certified_pages": sum(int(row["certified_pages"]) for row in cases),
                "longest_region_pages": max(int(row["longest_region"]) for row in cases),
                "region_entry_bytes": sum(int(row["region_entry_bytes"]) for row in cases),
            })
    break_even = []
    for pkey in predicates:
        shape = shapes[pkey[:3]]
        if pkey[2] != "direct":
            continue
        for family in ("facts", "zonemapplus"):
            baseline = float(queries[pkey + (family + "_prepared",)]["median_ns"])
            regional = float(queries[pkey + (family + "_regions_prepared",)]["median_ns"])
            incremental = float(shape["region_median_ns"]) + float(shape["region_prepare_median_ns"]) - float(shape["prepare_median_ns"])
            modeled = max(1, math.ceil(max(0.0, incremental) / (baseline - regional))) if baseline > regional else None
            measured = next((q for q in REPETITIONS if float(totals[pkey + (q, family + "_regions_prepared")]["median_ns"]) < float(totals[pkey + (q, family + "_prepared")]["median_ns"])), None)
            total_ratios = {q: float(totals[pkey + (q, family + "_regions_prepared")]["median_ns"]) / float(totals[pkey + (q, family + "_prepared")]["median_ns"]) for q in REPETITIONS}
            sustained = next((q for q in REPETITIONS if all(ratio <= .95 for later, ratio in total_ratios.items() if later >= q)), None)
            break_even.append({"case": pkey[0], "page_rows": pkey[1], "predicate": pkey[3], "split": shape["split"], "group": shape["group"], "source": shape["source"], "family": family, "requested": predicates[pkey][0], "incremental_setup_ns": incremental, "incremental_setup_clamped_ns": max(0.0, incremental), "negative_incremental_setup_is_sampling_noise": incremental < 0, "modeled_incremental_break_even_queries": modeled, "first_measured_total_win_queries": measured, "first_sustained_5pct_total_win_queries": sustained})
    selective = []
    for pkey in predicates:
        shape = shapes[pkey[:3]]
        if shape["source"] == "four_batches_4096" and "0.001" in predicates[pkey][0].split("|"):
            selective.append(float(queries[pkey + ("facts_regions_prepared",)]["median_ns"]) / float(queries[pkey + ("facts_regions",)]["median_ns"]))
    heldout_direct = [values for key, values in warm_groups.items() if key[0] == "heldout" and key[2] == "direct" and key[3] == "prepared_facts"]
    prepared_direct = ratio_summary([item for values in heldout_direct for item in values]) if heldout_direct else None
    cost = next((row for row in pooled if row["split"] == "heldout" and row["comparison"] == "cost_facts"), None)
    cost_control = next((row for row in pooled if row["split"] == "heldout" and row["comparison"] == "cost_zonemapplus"), None)
    prepared_pass = bool(selective and max(selective) <= .90 and prepared_direct and prepared_direct["source_weighted_geometric_mean"] <= 1.05)
    cost_pass = bool(cost and cost_control and cost["geometric_mean"] <= .95 and cost["source_weighted_geometric_mean"] <= 1 and cost["regressions_over_20_percent"] / cost["queries"] <= .05)
    return {
        "status": "complete", "mode": freeze["mode"], "rule_fingerprint": fingerprint,
        "shapes": len(shapes), "query_cells": len(queries), "total_cells": len(totals),
        "natural_sources": len({source_cases[case] for case in source_cases if source_cases[case][0] != "synthetic"}),
        "prevalence": prevalence, "warm_comparisons": warm, "pooled_warm_comparisons": pooled, "total_comparisons": end_to_end,
        "break_even": break_even,
        "engineering_decision": {"prepared_synthetic_selective_ratio": selective, "prepared_heldout_direct": prepared_direct, "prepared_gate_passed": prepared_pass, "cost_gate_passed": cost_pass, "cost_heldout": cost, "cost_heldout_control": cost_control, "natural_region_claims_require_scoped_review": True, "scope": "Engineering gates, not statistical guarantees; no source or query removed by its timing."},
        "scope": "resident bytes; all selected natural cases retained; synthetic scaling separate; totals include both columns, optional setup, exact-answer checks, and teardown",
        "automatic_promotion": False,
    }


def write_summary(directory, report):
    directory = Path(directory)
    (directory / "summary.json").write_text(json.dumps(report, indent=2) + "\n")
    lines = ["# Prepared queries, access costs, and surviving order", "",
             f"Validated {report['shapes']} shapes, {report['query_cells']} warm timing cells, and {report['total_cells']} measured end-to-end cells. All exact SUM/count checks agree.", "",
             "Ratios below one favor the named optimization. Every selected natural case is included, including zero-useful-region cases. Synthetic cases are separate. No canonical measurement is replaced automatically.", "",
             "## Natural region prevalence", "",
             "| Split | Rows/page | Pairs | Globally ordered | Broken order with multi-page regions | No multi-page regions | Longest region (pages) |", "|---|---:|---:|---:|---:|---:|---:|"]
    for row in report["prevalence"]:
        if row["split"] == "synthetic":
            continue
        lines.append(f"| {row['split']} | {row['page_rows']:,} | {row['pairs']} | {row['globally_ordered_pairs']} | {row['broken_global_order_with_multi_page_regions']} | {row['pairs_with_no_multi_page_regions']} | {row['longest_region_pages']} |")
    lines += ["", "## Warm query comparisons", "", "| Split | Rows/page | Layout | Comparison | Geo. ratio | Source-weighted ratio | >20% regressions | Worst ratio |", "|---|---:|---|---|---:|---:|---:|---:|"]
    for row in report["warm_comparisons"]:
        if row["split"] == "synthetic":
            continue
        lines.append(f"| {row['split']} | {row['page_rows']:,} | {row['layout']} | {row['comparison']} | {row['geometric_mean']:.3f} | {row['source_weighted_geometric_mean']:.3f} | {row['regressions_over_20_percent']}/{row['queries']} | {row['worst_ratio']:.2f} |")
    lines += ["", "## Measured resident load-to-answer totals", "", "| Split | Rows/page | Repeated queries | Comparison | Geo. ratio | Source-weighted ratio |", "|---|---:|---:|---|---:|---:|"]
    for row in report["total_comparisons"]:
        if row["split"] == "synthetic":
            continue
        lines.append(f"| {row['split']} | {row['page_rows']:,} | {row['repetitions']} | {row['comparison']} | {row['geometric_mean']:.3f} | {row['source_weighted_geometric_mean']:.3f} |")
    lines += ["", "Totals are directly measured from already resident serialized bytes through reconstructing both selector and value columns, optional region construction, optional preparation, Q repeated identical queries, and teardown. They exclude filesystem reads and encoding. Nine rounds rotate arm order; warm queries use a 2 ms calibration target, not a guaranteed minimum. Timed closures include shared exact-answer checks.", "", "The JSON gives every measured total ratio and per-predicate break-even. Modeled incremental break-even divides extra region/context setup by warm savings, clamping negative setup estimates to zero as sampling noise and requiring at least one query. First measured win reports only the first tested Q with lower median. The sustained 5% marker requires a ratio at most 0.95 at that Q and every later tested Q; it remains descriptive, not statistical certainty. Common initialization is included in measured totals rather than credited as free. Region entry bytes count used span payload, excluding vector spare capacity and allocator overhead. Same-shortcut zone-map arms receive the same preparation, regions, and cost gate.", "", "This diagnostic does not establish a new search algorithm, a cold-read speedup, or representative prevalence beyond its declared source sample. The cost policy uses descriptions, not query timings. Paper inclusion requires review of effect sizes, repeated evidence, controls, and scope; smoke evidence cannot support natural-data claims."]
    decision = report["engineering_decision"]
    lines += ["", "## Frozen engineering decision", "", f"Prepared-context gate passed: **{decision['prepared_gate_passed']}**. Cost gate passed: **{decision['cost_gate_passed']}**.", "", "The prepared gate requires at least 10% warm gain on the selective 4,096-page synthetic regional query and held-out direct-layout source-weighted time at most 1.05x. The cost gate requires held-out pooled time at most 0.95x, source-weighted time at most 1.00x, and no more than 5% of query cells regressing over 20%. Gates are engineering criteria, not significance tests. Paired source-level bootstrap intervals use 1,000 resamples and seed 20261025; single-source intervals carry no cross-source uncertainty."]
    (directory / "summary.md").write_text("\n".join(lines) + "\n")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: summarize_query_optimization.py RESULT_DIR")
    result = analyze(sys.argv[1])
    write_summary(sys.argv[1], result)
    print(json.dumps({key: result[key] for key in ("status", "mode", "shapes", "query_cells", "total_cells", "natural_sources")}, indent=2))
