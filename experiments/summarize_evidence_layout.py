#!/usr/bin/env python3
"""Validate and summarize the isolated evidence-layout diagnostic."""

import csv
import json
import math
from pathlib import Path
import statistics
import sys


LAYOUTS = ("direct", "compact", "whole", "whole_4k", "payload", "payload_4k")
PLANNERS = ("facts", "scan", "zonemap", "zonemapplus")


def geometric(values):
    return math.exp(statistics.mean(math.log(value) for value in values))


def summarize(directory):
    directory = Path(directory)
    with (directory / "freeze.csv").open() as stream:
        freeze = dict(csv.reader(stream))
    if freeze.get("status") != "complete":
        raise ValueError("measurement did not complete")
    with (directory / "layouts.csv").open() as stream:
        layouts = list(csv.DictReader(stream))
    with (directory / "queries.csv").open() as stream:
        queries = list(csv.DictReader(stream))
    sizes = {}
    for row in layouts:
        key = (row["pair"], row["layout"])
        if key in sizes or row["layout"] not in LAYOUTS:
            raise ValueError(f"duplicate or unknown layout: {key}")
        if int(row["bytes"]) != int(row["selector_bytes"]) + int(row["value_bytes"]):
            raise ValueError(f"inconsistent byte accounting: {key}")
        sizes[key] = row
    cells = {}
    for row in queries:
        key = (row["pair"], row["predicate"], row["layout"], row["planner"])
        if key in cells or row["planner"] not in PLANNERS:
            raise ValueError(f"duplicate or unknown query: {key}")
        times = [float(row[name]) for name in ("p25_ns", "median_ns", "p75_ns")]
        if not all(math.isfinite(value) and value > 0 for value in times) or times != sorted(times):
            raise ValueError(f"invalid timing quartiles: {key}")
        layout = sizes[row["pair"], row["layout"]]
        if any(row[name] != layout[name] for name in ("group", "source", "split")):
            raise ValueError(f"layout/query provenance mismatch: {key}")
        if sum(int(row[name]) for name in ("skipped", "accepted", "searched", "translated", "scanned")) != int(layout["pages"]):
            raise ValueError(f"incomplete page actions: {key}")
        cells[key] = row
    query_ids = sorted({key[:2] for key in cells})
    if not query_ids:
        raise ValueError("no measured queries")
    pairs = {pair for pair, _ in query_ids}
    if len(sizes) != len(pairs) * len(LAYOUTS) or len(cells) != len(query_ids) * len(LAYOUTS) * len(PLANNERS):
        raise ValueError("incomplete layout/planner matrix")
    sources = {}
    for pair, predicate in query_ids:
        direct = cells[pair, predicate, "direct", "facts"]
        source = (direct["group"], direct["source"])
        if sources.setdefault(source, direct["split"]) != direct["split"]:
            raise ValueError("a source crossed the development/heldout boundary")
        for layout in LAYOUTS:
            for planner in PLANNERS:
                row = cells[pair, predicate, layout, planner]
                if any(row[name] != direct[name] for name in ("matched_rows", "expected_sum", "requested", "split")):
                    raise ValueError("answer or workload differs across arms")
    if int(freeze["pairs"]) != len(pairs) or int(freeze["sources"]) != len(sources):
        raise ValueError("freeze counts disagree with results")
    rows = []
    for split in sorted({row["split"] for row in queries}):
        selected = [key for key in query_ids if cells[*key, "direct", "facts"]["split"] == split]
        split_pairs = {pair for pair, _ in selected}
        for layout in LAYOUTS:
            ratios = []
            by_source = {}
            strong = []
            cross_strong = []
            for pair, predicate in selected:
                row = cells[pair, predicate, layout, "facts"]
                direct = cells[pair, predicate, "direct", "facts"]
                ratio = float(row["median_ns"]) / float(direct["median_ns"])
                ratios.append(ratio)
                by_source.setdefault((row["group"], row["source"]), []).append(ratio)
                strong.append(float(row["median_ns"]) / float(cells[pair, predicate, layout, "zonemapplus"]["median_ns"]))
                cross_strong.append(float(row["median_ns"]) / float(cells[pair, predicate, "direct", "zonemapplus"]["median_ns"]))
            byte_ratio = sum(int(sizes[pair, layout]["bytes"]) for pair in split_pairs) / sum(int(sizes[pair, "direct"]["bytes"]) for pair in split_pairs)
            latency_ratio = geometric(ratios)
            rows.append({
                "split": split, "layout": layout, "queries": len(selected),
                "sources": len(by_source), "bytes_over_direct": byte_ratio,
                "facts_over_direct_facts_geo": latency_ratio,
                "source_weighted_latency_ratio": geometric([geometric(values) for values in by_source.values()]),
                "facts_over_same_layout_zonemapplus_geo": geometric(strong),
                "facts_over_direct_zonemapplus_geo": geometric(cross_strong),
                "regressions_over_20pct": sum(value > 1.2 for value in ratios),
                "worst_latency_ratio": max(ratios),
                "first_query_over_direct_geo": geometric([int(sizes[pair, layout]["first_query_ns"]) / int(sizes[pair, "direct"]["first_query_ns"]) for pair in split_pairs]),
                "passes_point_thresholds": byte_ratio <= .8 and latency_ratio <= 1.05,
            })
    is_smoke = freeze["smoke"] == "true"
    candidates = [] if is_smoke else [row["layout"] for row in rows if row["split"] == "heldout" and row["layout"] in ("payload", "payload_4k") and row["passes_point_thresholds"]]
    decision = {
        "status": "complete", "correctness": "all SUM/count checks passed",
        "promote_to_broader_rerun": bool(candidates), "candidates": candidates,
        "byte_ratio_threshold": .8, "latency_ratio_threshold": 1.05,
        "smoke": is_smoke, "comparisons": rows,
        "scope": "resident diagnostic; warm query timings, separate first-query costs; no cold-I/O claim",
    }
    with (directory / "summary.csv").open("w") as stream:
        writer = csv.DictWriter(stream, fieldnames=rows[0].keys())
        writer.writeheader()
        writer.writerows(rows)
    (directory / "decision.json").write_text(json.dumps(decision, indent=2) + "\n")
    lines = ["# Evidence-layout diagnostic", "", decision["correctness"] + ".", "",
             "Ratios below one favor the candidate. Bytes pool selector/value pairs; latency is the geometric mean over complete queries.", "",
             "| Split | Layout | Bytes/direct | Time/direct | Source-weighted time | Time/same-layout +shortcuts | >20% regressions | Worst time ratio |",
             "|---|---|---:|---:|---:|---:|---:|---:|"]
    for row in rows:
        lines.append(f"| {row['split']} | {row['layout']} | {row['bytes_over_direct']:.3f} | {row['facts_over_direct_facts_geo']:.3f} | {row['source_weighted_latency_ratio']:.3f} | {row['facts_over_same_layout_zonemapplus_geo']:.3f} | {row['regressions_over_20pct']} | {row['worst_latency_ratio']:.2f} |")
    lines += ["", f"Promote to broader rerun: **{bool(candidates)}**. Candidates: {', '.join(candidates) or 'none'}.", "",
              "The frozen engineering gate requires bytes/direct <=0.80 and Facts/direct Facts time <=1.05 on held-out sources. Smoke results cannot promote a candidate. The four matched menus share logical codec choices, but size selection can choose different recipes per page; placement/chunking comparisons include that selection effect. Compact additionally introduces delta codecs.", "",
              "The first query includes initially uncached dictionary-order checks; encoding is separate. The timing table uses warm queries. Instrumented per-page planning counts do not measure full-query I/O. First-query ratios, sizes, frame counts, recipes, and all baseline arms are in the CSVs. No claim-bearing results were replaced."]
    (directory / "summary.md").write_text("\n".join(lines) + "\n")
    print("\n".join(lines))
    return decision


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: summarize_evidence_layout.py RESULT_DIR")
    summarize(sys.argv[1])
