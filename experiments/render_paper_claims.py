#!/usr/bin/env python3
"""Refresh numerical public prose and existing local paper claim files."""

import csv
import subprocess
import sys
from pathlib import Path

from render_docs_claims import render_docs

ROOT = Path(__file__).resolve().parents[1]


def main():
    with (ROOT / "experiments/results/claim_manifest.csv").open(newline="") as source:
        rows = list(csv.DictReader(source))
    render_docs(ROOT, {row["claim"]: row["value"] for row in rows})
    lines = ["% Generated from experiments/results/claim_manifest.csv; do not edit."]
    for row in rows:
        value = row["value"]
        if value.isdigit() and len(value) > 3:
            value = f"{int(value):,}"
        lines.append(f"\\newcommand{{\\{row['claim']}}}{{{value}}}")
    content = "\n".join(lines) + "\n"
    # The manuscript is optional and excluded from the public artifact.
    for destination in sorted((ROOT / "paper").rglob("claim.tex")):
        if "notes" in destination.relative_to(ROOT / "paper").parts:
            continue
        destination.write_text(content)
        print(f"updated {destination.relative_to(ROOT)} ({len(rows)} claims)")
        # A local manuscript may derive table layouts from these same claims.
        formatter = destination.parent / "make_claims.py"
        if formatter.is_file():
            subprocess.run([sys.executable, str(formatter)], check=True)


if __name__ == "__main__":
    main()
