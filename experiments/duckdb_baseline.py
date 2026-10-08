#!/usr/bin/env python3
"""DuckDB on the paged queries.

Runs the same SELECT SUM(value), COUNT(*) ... WHERE selector BETWEEN low AND
high over the same rows that the paged study and the scale study measure, and
the conjunctive TPC-H Q6 queries of the conjunctive study, in two ways: loaded into DuckDB's own compressed storage, whose row groups
carry min/max zone maps, and read straight from the Parquet files the
studies export. Every answer is checked against the study's exact result.

Timing follows the Rust harness: one warm-up, a probe that sizes a batch to
at least 2 ms, then five batches; p25, median and p75 of the per-query
time, measured at the Python client. The client round trip of a query that
reads nothing is recorded once as a floor. One profiled run records the
rows DuckDB's scans read, which shows how much its zone maps pruned.

Usage: experiments/duckdb_baseline.py [paged|scale|conjunctive ...]
"""

import csv
import json
import os
import shutil
import sys
import tempfile
import time

import duckdb

PAGED = os.environ.get("WITNESS_PAGED_RESULT_DIR", "experiments/results/paged_study")
SCALE = os.environ.get("WITNESS_PAGED_SCALE_RESULT_DIR", "experiments/results/paged_scale")
CONJUNCTIVE = os.environ.get(
    "WITNESS_PAGED_CONJUNCTIVE_RESULT_DIR", "experiments/results/paged_conjunctive"
)
OUTPUT = os.environ.get("WITNESS_DUCKDB_RESULT_DIR", "experiments/results/duckdb")
REPEATS = 5
TARGET_NS = 2_000_000
THREADS = {"paged": [1], "scale": [1, 64], "conjunctive": [1, 64]}
HEADER = [
    "study",
    "table",
    "predicate",
    "engine",
    "threads",
    "p25_ns",
    "median_ns",
    "p75_ns",
    "rows_scanned",
    "rows",
    "matched_rows",
]


def between(low, high, column="timestamp"):
    return f"{column} BETWEEN {low} AND {high}"


def paged_queries():
    """(table, parquet file, [(label, where, aggregate, matched, sum)]) per pair."""
    tables = {}
    with open(f"{PAGED}/queries.csv", newline="") as source:
        for row in csv.DictReader(source):
            if row["arm"] != "witness_direct_facts":
                continue
            tables.setdefault(row["pair"], []).append(
                (
                    row["predicate"],
                    between(row["low"], row["high"]),
                    "SUM(value)",
                    int(row["matched_rows"]),
                    int(row["expected_sum"]),
                )
            )
    return [
        (f"pair_{pair}", f"{PAGED}/parquet/pair_{pair}.parquet", queries)
        for pair, queries in sorted(tables.items(), key=lambda item: int(item[0]))
    ]


def scale_queries():
    tables = {}
    with open(f"{SCALE}/scaling.csv", newline="") as source:
        for row in csv.DictReader(source):
            if row["arm"] != "parquet_plain_indexed":
                continue
            tables.setdefault(row["selector"], []).append(
                (
                    row["predicate"],
                    between(row["low"], row["high"]),
                    "SUM(value)",
                    int(row["matched_rows"]),
                    int(row["expected_sum"]),
                )
            )
    return [
        (selector, f"{SCALE}/parquet/{selector}.parquet", queries)
        for selector, queries in sorted(tables.items())
    ]


def conjunctive_queries():
    """The conjunctive study's queries over its one exported table."""
    queries = {}
    with open(f"{CONJUNCTIVE}/spec.csv", newline="") as source:
        for row in csv.DictReader(source):
            query = queries.setdefault(
                row["query"],
                ([], int(row["matched_rows"]), int(row["expected_sum"])),
            )
            query[0].append(between(row["low"], row["high"], row["column"]))
    return [
        (
            "lineitem",
            f"{CONJUNCTIVE}/parquet/lineitem.parquet",
            [
                (name, " AND ".join(where), "SUM(extendedprice * discount)", matched, expected)
                for name, (where, matched, expected) in queries.items()
            ],
        )
    ]


QUERIES = {"paged": paged_queries, "scale": scale_queries, "conjunctive": conjunctive_queries}


def answer(con, sql):
    total, matched = con.execute(sql).fetchone()
    return int(total or 0), int(matched)


def benchmark(run):
    run()
    started = time.perf_counter_ns()
    run()
    probe = max(time.perf_counter_ns() - started, 1)
    iterations = min(max(TARGET_NS // probe, 1), 1_000)
    samples = []
    for _ in range(REPEATS):
        started = time.perf_counter_ns()
        for _ in range(iterations):
            run()
        samples.append((time.perf_counter_ns() - started) / iterations)
    samples.sort()
    return samples[1], samples[2], samples[3]


def rows_scanned(con, sql):
    """Rows DuckDB's table scans read for one run of `sql`."""
    con.execute("PRAGMA enable_profiling='no_output'")
    con.execute(sql).fetchall()
    info = json.loads(con.get_profiling_information(format="json"))
    con.execute("PRAGMA disable_profiling")
    return info["cumulative_rows_scanned"]


def measure(study, tables, writer, scratch):
    for table, parquet, queries in tables:
        if not os.path.exists(parquet):
            sys.exit(f"missing {parquet}; run the {study} study first")
        database = os.path.join(scratch, f"{table}.duckdb")
        con = duckdb.connect(database)
        con.execute(f"CREATE TABLE t AS SELECT * FROM read_parquet('{parquet}')")
        con.execute("CHECKPOINT")
        rows = con.execute("SELECT COUNT(*) FROM t").fetchone()[0]
        sources = {
            "duckdb_native": "t",
            "duckdb_parquet": f"read_parquet('{parquet}')",
        }
        for threads in THREADS[study]:
            con.execute(f"SET threads = {threads}")
            for label, where, aggregate, matched, expected in queries:
                for engine, source in sources.items():
                    sql = f"SELECT ({aggregate})::HUGEINT, COUNT(*) FROM {source} WHERE {where}"
                    got = answer(con, sql)
                    if got != (expected, matched):
                        sys.exit(
                            f"{engine} on {table} {label} returned {got}, "
                            f"expected {(expected, matched)}"
                        )
                    p25, median, p75 = benchmark(lambda: con.execute(sql).fetchone())
                    scanned = rows_scanned(con, sql)
                    writer.writerow(
                        [
                            study,
                            table,
                            label,
                            engine,
                            threads,
                            f"{p25:.1f}",
                            f"{median:.1f}",
                            f"{p75:.1f}",
                            int(scanned),
                            rows,
                            matched,
                        ]
                    )
        con.close()
        os.remove(database)
        print(f"{study}: {table}", file=sys.stderr)


def main():
    studies = sys.argv[1:] or ["paged", "scale", "conjunctive"]
    os.makedirs(OUTPUT, exist_ok=True)
    scratch = tempfile.mkdtemp(prefix="witness-duckdb-")
    try:
        for study in studies:
            tables = QUERIES[study]()
            with open(f"{OUTPUT}/{study}.csv", "w", newline="") as output:
                writer = csv.writer(output)
                writer.writerow(HEADER)
                measure(study, tables, writer, scratch)
        con = duckdb.connect()
        con.execute("SET threads = 1")
        _, floor, _ = benchmark(lambda: con.execute("SELECT 1").fetchone())
        con.close()
        with open(f"{OUTPUT}/freeze.csv", "w", newline="") as freeze:
            freeze.write(f"duckdb_version,client_floor_ns\n{duckdb.__version__},{floor:.1f}\n")
    finally:
        shutil.rmtree(scratch, ignore_errors=True)


if __name__ == "__main__":
    main()
