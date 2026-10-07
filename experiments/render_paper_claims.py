#!/usr/bin/env python3
"""Refresh existing local paper claim files from the canonical manifest."""

import csv
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def main():
    with (ROOT / "experiments/results/claim_manifest.csv").open(newline="") as source:
        rows = list(csv.DictReader(source))
    lines = ["% Generated from experiments/results/claim_manifest.csv; do not edit."]
    for row in rows:
        value = row["value"]
        if value.isdigit() and len(value) > 3:
            value = f"{int(value):,}"
        lines.append(f"\\newcommand{{\\{row['claim']}}}{{{value}}}")
    content = "\n".join(lines) + "\n"
    # The manuscript is optional and excluded from the public artifact.
    for destination in sorted((ROOT / "paper").rglob("claim.tex")):
        destination.write_text(content)
        print(f"updated {destination.relative_to(ROOT)} ({len(rows)} claims)")


if __name__ == "__main__":
    main()
