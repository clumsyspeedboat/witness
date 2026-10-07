#!/usr/bin/env python3
"""Fetch the scale corpus: TPC-H SF10 lineitem at full length.

Exports the integer, decimal, and date columns of lineitem (59,986,052 rows)
into eval/data/scale/tpch_sf10/lineitem__c<i>_<name>.csv in the
one-value-per-line layout datasets.rs reads; dates become epoch seconds with a
`_timestamp` suffix. The paged scale study reads these columns whole, so
intra-query parallelism has thousands of pages to spread.

Stdlib + duckdb. Idempotent: an existing column file is kept.
"""

import re
import sys
from pathlib import Path

import duckdb

ROOT = Path(__file__).resolve().parent.parent.parent
OUT = ROOT / "eval" / "data" / "scale" / "tpch_sf10"


def safe(name):
    return re.sub(r"[^A-Za-z0-9]+", "_", name)[:40].strip("_") or "col"


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    con = duckdb.connect()
    con.execute("INSTALL tpch; LOAD tpch; CALL dbgen(sf=10);")
    described = con.execute("DESCRIBE SELECT * FROM lineitem").fetchall()
    for index, (name, kind, *_) in enumerate(described):
        kind = kind.lower()
        if kind.startswith("date"):
            expression = f'epoch("{name}")::BIGINT'
            file = f"lineitem__c{index}_{safe(name)}_timestamp.csv"
        elif re.match(r"^(integer|bigint|decimal)", kind):
            expression = f'"{name}"'
            file = f"lineitem__c{index}_{safe(name)}.csv"
        else:
            continue
        target = OUT / file
        if target.exists():
            print(f"keep {file}", flush=True)
            continue
        partial = OUT / f".{file}.part"
        con.execute(
            f"COPY (SELECT {expression} FROM lineitem) TO '{partial}' "
            "(FORMAT csv, HEADER false, NULLSTR '')"
        )
        partial.rename(target)
        print(f"wrote {file}", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
