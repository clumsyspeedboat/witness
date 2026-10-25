#!/usr/bin/env python3
"""Refresh numerical prose from the same manifest as the paper claims.

Templates identify complete phrases or table rows. Their numeric slots match
old values, so reruns update numbers without globally replacing similar text.
An edited or duplicated phrase fails explicitly rather than silently drifting.
"""

import csv
from pathlib import Path
import re


ROOT = Path(__file__).resolve().parents[1]
SLOT = re.compile(r"\{\{(Wit\w+)(?::(grouped))?\}\}")


def table(label, value):
    return f"| {label} | {value} |"


README = [
    "A census of **{{WitCensusColumns}}** columns from **{{WitCensusSources}}** sources, and a query study of **{{WitCells}}** cells over **{{WitSources}}** sources at {{WitRowsPerColumn:grouped}} rows per column.",
    table("non-decreasing, constant columns included", "**{{WitCensusMonotoneColumns}}%** of columns"),
    table("where evidence exists and is reachable", "**{{WitSourceDirectBoundaryMedian}}x** a sortedness-aware Parquet reference"),
    table("price of keeping that access", "**{{WitAccessPremium}}x** the bytes"),
    table("cold disk, fewest-bytes plan", "**{{WitColdPageOverFull}}x** slower than reading the whole file ({{WitColdPageCalls:grouped}} scattered reads against one)"),
    table("paged queries against the faster Parquet read (with or without its page index)", "**{{WitPagedFactsOverBestPlainMedian}}x** the time (median)"),
    table("the same queries against DuckDB on its own storage, one thread", "**{{WitIntDuckMedian}}x** the time (median)"),
    table("planned from facts against an exact stored zone map, same executor", "**{{WitPagedFactsOverZonemapGeo}}x** the time (geometric mean)"),
    table("string range and prefix predicates against the zone map, same executor", "**{{WitStringFactsOverZonemapGeo}}x** the time (geometric mean)"),
    table("string and float predicates against DuckDB, one thread", "**{{WitStringDuckMedian}}x** and **{{WitFloatDuckMedian}}x** the time (median)"),
    table("TPC-H Q6 on SF10 lineitem against DuckDB, one thread", "**{{WitQSixFactsOverDuckOne}}x** the time ({{WitQSixFactsOverDuckOneRunLow}}–{{WitQSixFactsOverDuckOneRunHigh}}x across runs)"),
    table("page skips that bounds read from encodings recover", "**{{WitPagedBoundSkipRecovery}}%** of what stored min/max allow"),
    table("pages skipped beyond an exact zone map, where a sorted dictionary holds no value in the range", "**{{WitPagedFactsExtraSkipPercent}}%** more"),
    table("a tenth of the rows deleted at random, every plan unchanged", "**{{WitDeleteRandomTenOverNone}}x** the time (median), every answer exact"),
    table("mutated plans that would answer wrongly, rejected by the checker", "**{{WitCheckerHarmfulRejected}}** of {{WitCheckerHarmful}}"),
    table("sorted key, 0.1% of 60 M rows, one thread", "**{{WitScaleOrderkeyTenthMs}} ms** against Parquet's {{WitScaleOrderkeyTenthParquetMs}} ms ({{WitScaleOrderkeyTenthParquetMsRunLow}}–{{WitScaleOrderkeyTenthParquetMsRunHigh}} ms across runs)"),
]

RULES = {
    "README.md": README,
    "docs/LIMITATIONS.md": [
        "median {{WitSourceDirectBoundaryMedian}}x with a 95% bootstrap interval [{{WitSourceDirectBoundaryCiLow}}, {{WitSourceDirectBoundaryCiHigh}}]",
        "(families, then sources within each) gives [{{WitClusterDirectBoundaryCiLow}}, {{WitClusterDirectBoundaryCiHigh}}]",
        "family to a single draw instead gives {{WitFamilyDirectBoundaryMedian}}x [{{WitFamilyDirectBoundaryCiLow}}, {{WitFamilyDirectBoundaryCiHigh}}]",
        "{{WitFamilyWins}} of the {{WitFamilies}} families favour",
        "two-sided p={{WitFamilyWilcoxonP}}",
        "one-sided p={{WitFamilySignP}}",
        "{{WitColdRequiredMb}} MiB of required closure in a {{WitColdFileMb}} MiB file. Minimal per-page reads issued {{WitColdPageCalls:grouped}} calls and took {{WitColdPageOverFull}}x a full read; coalescing issued {{WitColdCoalescedCalls}} calls and reached {{WitColdCoalescedOverFull}}x",
        "The access-ready byte premium is {{WitAccessPremium}}x in the canonical aggregate.",
    ],
    "docs/ENCODINGS.md": [
        "the access-ready layout uses {{WitAccessPremium}}x the bytes of the size-selected layout at the canonical aggregate",
        "against the Parquet boundary reader is {{WitSourceDirectBoundaryMedian}}x.",
        "boundary path is [{{WitSourceDirectBoundaryCiLow}}, {{WitSourceDirectBoundaryCiHigh}}], below parity.",
        table("absent", "{{WitBloomAbsentCandidate}} | {{WitBloomAbsentModeledBytes}}"),
        table("rare", "{{WitBloomRareCandidate}} | {{WitBloomRareModeledBytes}}"),
        table("frequent", "{{WitBloomFrequentCandidate}} | {{WitBloomFrequentModeledBytes}}"),
        table("mixed IN", "{{WitBloomInCandidate}} | {{WitBloomInModeledBytes}}"),
    ],
    "docs/WALKTHROUGH.md": [
        "per-page closure read fewer bytes but issued {{WitColdPageCalls:grouped}} calls and took {{WitColdPageOverFull}}x a full read; coalescing reached {{WitColdCoalescedCalls}} calls and {{WitColdCoalescedOverFull}}x.",
    ],
}


def render_phrase(template, claims):
    def value(match):
        result = claims[match[1]]
        return f"{int(result.replace(',', '')):,}" if match[2] else result

    return SLOT.sub(value, template)


def phrase_pattern(template):
    pieces = []
    previous = 0
    for match in SLOT.finditer(template):
        literal = template[previous:match.start()]
        pieces.extend(r"\s+" if word.isspace() else re.escape(word)
                      for word in re.split(r"(\s+)", literal))
        pieces.append(r"([0-9][0-9.,]*)")
        previous = match.end()
    pieces.extend(r"\s+" if word.isspace() else re.escape(word)
                  for word in re.split(r"(\s+)", template[previous:]))
    return "".join(pieces)


def render_docs(root, claims):
    updates = {}
    for relative, templates in RULES.items():
        path = root / relative
        text = path.read_text()
        for template in templates:
            slots = [render_phrase(slot[0], claims) for slot in SLOT.finditer(template)]

            def replace(match):
                # Preserve the author's whitespace and line wrapping, updating
                # only the numeric captures within the matched phrase.
                result = match[0]
                for index in range(len(slots), 0, -1):
                    start, end = match.span(index)
                    start -= match.start()
                    end -= match.start()
                    result = result[:start] + slots[index - 1] + result[end:]
                return result

            text, count = re.subn(phrase_pattern(template), replace, text)
            if count != 1:
                raise ValueError(f"{relative}: expected one claim phrase, found {count}: {template}")
        updates[path] = text
    # Validate every phrase before writing any file.
    for path, text in updates.items():
        if path.read_text() != text:
            path.write_text(text)
            print(f"updated numerical prose in {path.relative_to(root)}")


def main():
    with (ROOT / "experiments/results/claim_manifest.csv").open() as stream:
        claims = {row["claim"]: row["value"] for row in csv.DictReader(stream)}
    render_docs(ROOT, claims)


if __name__ == "__main__":
    main()
