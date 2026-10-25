#!/usr/bin/env python3
"""Validate exports from the native-reader/ordered-region diagnostic."""

import csv
import json
import math
from pathlib import Path
import statistics
import sys


def summarize(directory):
    directory = Path(directory)
    with (directory / "freeze.csv").open() as stream:
        freeze = dict(csv.reader(stream))
    if freeze.get("status") != "complete":
        raise ValueError("diagnostic did not finish")
    with (directory / "reader.csv").open() as stream:
        readers = list(csv.DictReader(stream))
    with (directory / "regions.csv").open() as stream:
        regions = list(csv.DictReader(stream))
    if not readers or len(readers) != int(freeze["reader_columns"]):
        raise ValueError("reader coverage disagrees with the freeze record")
    for row in readers:
        if int(row["file_bytes"]) != int(row["page_bytes"]) + int(row["descriptor_bytes"]):
            raise ValueError("invalid native byte accounting")
    cells = {}
    for row in regions:
        key = (int(row["pages"]), row["requested"], row["arm"])
        if key in cells:
            raise ValueError("duplicate region timing cell")
        times = [float(row[name]) for name in ("p25_ns", "median_ns", "p75_ns")]
        if times != sorted(times) or any(not math.isfinite(x) or x <= 0 for x in times):
            raise ValueError("invalid region timing quartiles")
        if not 0 <= int(row["candidate_pages"]) <= int(row["pages"]):
            raise ValueError("invalid region candidate count")
        cells[key] = row
    comparisons = []
    for pages in (64, 512, 4096):
        for selectivity in ("0.001", "0.010", "0.100", "0.500"):
            original = cells[pages, selectivity, "original"]
            regional = cells[pages, selectivity, "regions"]
            control = cells[pages, selectivity, "zonemapplus_regions"]
            if any(row[name] != original[name] for row in (regional, control) for name in ("sum", "matched", "batches")):
                raise ValueError("region results disagree across arms")
            if int(regional["regions"]) != 4 or int(control["regions"]) != 4:
                raise ValueError("expected four appended ordered regions")
            comparisons.append({
                "pages": pages, "selectivity": float(selectivity),
                "candidate_pages": int(regional["candidate_pages"]),
                "speedup_over_original": float(original["median_ns"]) / float(regional["median_ns"]),
                "facts_over_same_shortcuts": float(regional["median_ns"]) / float(control["median_ns"]),
            })
    if len(cells) != 36:
        raise ValueError("region timing matrix is incomplete")
    descriptor_bytes = sum(int(row["descriptor_bytes"]) for row in readers)
    page_bytes = sum(int(row["page_bytes"]) for row in readers)
    result = {
        "status": "complete", "reader_columns": len(readers),
        "reader_sources": len({(row["group"], row["source"]) for row in readers}),
        "descriptor_bytes": descriptor_bytes,
        "descriptor_over_page_bytes": descriptor_bytes / page_bytes,
        "median_open_ms": statistics.median(int(row["open_ns"]) for row in readers) / 1e6,
        "region_comparisons": comparisons,
        "scope": "resident reader initialization; synthetic appended-batch query diagnostic",
    }
    lines = ["# Native reader and ordered regions", "",
             f"All exact SUM/count checks passed on {len(readers)} sampled corpus columns and 36 controlled region timing cells.", "",
             f"Container descriptors add {100 * descriptor_bytes / page_bytes:.2f}% to pooled existing page bytes. Median reader initialization is {result['median_open_ms']:.3f} ms per sampled column, including checksum, description parsing, dictionary-order verification, and fact reconstruction. Initialization is measured once per column and is not a repeated benchmark.", "",
             "| Pages | Selectivity | Candidate pages | Speedup over original Facts | Facts / same regional shortcuts |",
             "|---:|---:|---:|---:|---:|"]
    for row in comparisons:
        lines.append(f"| {row['pages']:,} | {row['selectivity']:.1%} | {row['candidate_pages']} | {row['speedup_over_original']:.2f}x | {row['facts_over_same_shortcuts']:.2f}x |")
    lines += ["", "The region workload contains four appended sorted batches with overlapping value ranges; it is synthetic. Ordinary Facts plans each page after global order breaks. Both regional planners use the same certificates and executor. Warm timings exclude native initialization and region construction. Candidate counts describe pages admitted for planning, not physical reads. Full queries still validate page alignment.", "",
              "The reader loads/checksums the whole file into memory. No cold-I/O or general-corpus region-speedup claim follows. Metadata storage and initialization are charged explicitly. Main cold/paged ablation measurements remain separate; confirmed exports supply the scoped extension claims."]
    (directory / "summary.json").write_text(json.dumps(result, indent=2) + "\n")
    (directory / "summary.md").write_text("\n".join(lines) + "\n")
    print("\n".join(lines))


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: summarize_native_reader.py RESULT_DIR")
    summarize(sys.argv[1])
