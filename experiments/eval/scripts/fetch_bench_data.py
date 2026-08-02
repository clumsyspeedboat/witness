#!/usr/bin/env python3
"""Fetch the canonical Parquet-benchmark datasets (advisor request, 2026-07):

  TPC-H SF1 lineitem  -> eval/data/tpch/lineitem__c<i>_<name>.csv
  ClickBench hits     -> eval/data/clickbench/hits__c<i>_<name>.csv
                         (one partition of the official partitioned parquet,
                          capped at CAP rows - documented subset, like Public BI)

File format matches the Public BI single-column layout consumed by
`datasets.rs::column_from_tokens`: one raw token per line, empty line = NULL.
Decimal columns keep their decimal text so the verified fixed-point scaling
path decides exactness. Timestamps are exported as epoch seconds so they enter
the timestamp path.

Stdlib + duckdb. Idempotent: skips work if the target directory is populated.
"""

from pathlib import Path
import sys

import duckdb

ROOT = Path(__file__).resolve().parent.parent.parent
DATA = ROOT / "eval" / "data"
CAP = 1_000_000  # ClickBench cap (documented subset); TPC-H SF1 is used whole

CLICKBENCH_URL = (
    "https://datasets.clickhouse.com/hits_compatible/athena_partitioned/hits_0.parquet"
)

# (duckdb expression, column file suffix) — mixed structure on purpose:
# monotone keys, small domains, decimals, and a real event timestamp.
TPCH_COLS = [
    ("l_orderkey", "l_orderkey"),
    ("l_partkey", "l_partkey"),
    ("l_suppkey", "l_suppkey"),
    ("l_linenumber", "l_linenumber"),
    ("l_quantity", "l_quantity"),
    ("l_extendedprice", "l_extendedprice"),
    ("l_discount", "l_discount"),
    ("l_tax", "l_tax"),
    ("epoch(l_shipdate)::BIGINT", "timestamp"),
]

CLICKBENCH_COLS = [
    ("EventTime", "timestamp"),  # already epoch seconds in athena-compatible files
    ("CounterID", "CounterID"),
    ("RegionID", "RegionID"),
    ("ResolutionWidth", "ResolutionWidth"),
    ("ResolutionHeight", "ResolutionHeight"),
    ("WindowClientWidth", "WindowClientWidth"),
    ("CodeVersion", "CodeVersion"),
    ("SearchEngineID", "SearchEngineID"),
    ("SendTiming", "SendTiming"),
]


def export(con, table_expr, cols, out_dir, prefix, cap=None):
    out_dir.mkdir(parents=True, exist_ok=True)
    limit = f" LIMIT {cap}" if cap else ""
    for i, (expr, name) in enumerate(cols):
        path = out_dir / f"{prefix}__c{i}_{name}.csv"
        if path.exists():
            print(f"  keep {path.name}")
            continue
        rows = con.execute(f"SELECT {expr} FROM {table_expr}{limit}").fetchall()
        with open(path, "w") as f:
            for (v,) in rows:
                f.write("" if v is None else f"{v}")
                f.write("\n")
        print(f"  wrote {path.name} ({len(rows)} rows)")


def main():
    con = duckdb.connect()

    tpch_dir = DATA / "tpch"
    if not any(tpch_dir.glob("*.csv")):
        print("TPC-H SF1 lineitem (duckdb dbgen)...")
        con.execute("INSTALL tpch; LOAD tpch; CALL dbgen(sf=1);")
        export(con, "lineitem", TPCH_COLS, tpch_dir, "lineitem")
    else:
        print("tpch/ already populated")

    cb_dir = DATA / "clickbench"
    if not any(cb_dir.glob("*.csv")):
        raw = DATA / "clickbench_hits_0.parquet"
        if not raw.exists():
            print(f"downloading ClickBench partition -> {raw.name} ...")
            import urllib.request

            urllib.request.urlretrieve(CLICKBENCH_URL, raw)
            print(f"  {raw.stat().st_size / 1e6:.0f} MB")
        print(f"ClickBench hits (first {CAP} rows of partition 0)...")
        export(con, f"read_parquet('{raw}')", CLICKBENCH_COLS, cb_dir, "hits", cap=CAP)
    else:
        print("clickbench/ already populated")

    print("done.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
