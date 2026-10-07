#!/usr/bin/env python3
"""Fetch the breadth corpora for the invariant census.

Exports every exact-numeric and temporal column of

  publicbi    the first table of each of the 46 Public BI workbooks,
  tpch        every TPC-H SF1 table with at least MIN_ROWS rows (duckdb dbgen),
  tpcds       every TPC-DS SF1 table with at least MIN_ROWS rows (duckdb dsdgen),
  clickbench  every integer and temporal column of the hits partition 0,

into eval/data/census/<group>/<Table>__c<i>_<name>.csv, where <i> is the
column's physical position in its table. The file layout is the one
`datasets.rs::dir_columns` reads: one raw token per line, an empty line is
NULL. Temporal columns are exported as epoch seconds and carry a `_timestamp`
suffix; decimal columns keep their decimal text so the Rust side's verified
fixed-point scaling decides exactness.

Rows are capped at CAP per table. Public BI tables are streamed from their
bz2 archives and the download stops at the cap. Each completed table appends
one line to the group's PROVENANCE.tsv; tables already listed there are
skipped, so the script is idempotent and resumable.

Stdlib + duckdb.
"""

import bz2
import concurrent.futures
import datetime
import hashlib
import json
import re
import sys
import threading
import urllib.request
from pathlib import Path

import duckdb

ROOT = Path(__file__).resolve().parent.parent.parent
DATA = ROOT / "eval" / "data"
OUT = DATA / "census"
CAP = 1_000_000
MIN_ROWS = 1_024

PUBLIC_BI = "https://raw.githubusercontent.com/cwida/public_bi_benchmark/master/benchmark"
PUBLIC_BI_LIST = "https://api.github.com/repos/cwida/public_bi_benchmark/contents/benchmark"
CLICKBENCH = DATA / "clickbench_hits_0.parquet"

INTEGER = re.compile(r"^(tinyint|smallint|integer|int|bigint)\b", re.I)
DECIMAL = re.compile(r"^(decimal|numeric|double|real|float)\b", re.I)
TEMPORAL = re.compile(r"^(date|timestamp)\b", re.I)
TIME_FORMATS = (
    "%Y-%m-%d %H:%M:%S",
    "%Y-%m-%d %H:%M:%S.%f",
    "%Y-%m-%d",
    "%m/%d/%Y %H:%M:%S",
    "%m/%d/%Y",
)

LOCK = threading.Lock()


def safe(name):
    return re.sub(r"[^A-Za-z0-9]+", "_", name)[:40].strip("_") or "col"


def done_tables(group):
    path = OUT / group / "PROVENANCE.tsv"
    if not path.exists():
        return set()
    return {line.split("\t", 1)[0] for line in path.read_text().splitlines()[1:]}


def record(group, table, rows, columns, digest, source):
    path = OUT / group / "PROVENANCE.tsv"
    with LOCK:
        new = not path.exists()
        with open(path, "a") as out:
            if new:
                out.write("table\trows\tcolumns\tsha256_12\tsource\n")
            out.write(f"{table}\t{rows}\t{columns}\t{digest}\t{source}\n")


def get(url, timeout=300):
    request = urllib.request.Request(url, headers={"User-Agent": "witness-census"})
    return urllib.request.urlopen(request, timeout=timeout)


def epoch_seconds(token):
    for fmt in TIME_FORMATS:
        try:
            moment = datetime.datetime.strptime(token, fmt)
        except ValueError:
            continue
        return str(int(moment.replace(tzinfo=datetime.timezone.utc).timestamp()))
    return ""


# ---------------------------------------------------------------- Public BI


def public_bi_schema(workbook, table):
    sql = get(f"{PUBLIC_BI}/{workbook}/tables/{table}.table.sql").read().decode("utf-8", "replace")
    body = sql[sql.find("(") + 1 : sql.rfind(")")]
    columns = []
    for line in body.split("\n"):
        match = re.match(r'\s*"([^"]+)"\s+([A-Za-z]+)', line) or re.match(
            r"\s*([^\s,]+)\s+([A-Za-z]+)", line
        )
        if match:
            columns.append((match.group(1).strip(), match.group(2)))
    return columns


def public_bi_table(workbook):
    urls = get(f"{PUBLIC_BI}/{workbook}/data-urls.txt").read().decode().split()
    url = sorted(urls)[0].replace("http://", "https://")
    table = url.rsplit("/", 1)[1].split(".", 1)[0]
    if table in done_tables("publicbi"):
        return f"{workbook}: kept"
    schema = public_bi_schema(workbook, table)
    picked = []
    for index, (name, kind) in enumerate(schema):
        if INTEGER.match(kind) or DECIMAL.match(kind):
            picked.append((index, f"{table}__c{index}_{safe(name)}.csv", False))
        elif TEMPORAL.match(kind):
            picked.append((index, f"{table}__c{index}_{safe(name)}_timestamp.csv", True))
    if not picked:
        record("publicbi", table, 0, 0, "-", url)
        return f"{workbook}: no numeric columns"
    directory = OUT / "publicbi"
    partial = {index: directory / f".{file}.part" for index, file, _ in picked}
    writers = {index: open(path, "w") for index, path in partial.items()}
    width = len(schema)
    digest = hashlib.sha256()
    decompressor = bz2.BZ2Decompressor()
    buffer = b""
    rows = 0
    with get(url) as response:
        while rows < CAP and not decompressor.eof:
            chunk = response.read(1 << 20)
            if not chunk:
                break
            digest.update(chunk)
            buffer += decompressor.decompress(chunk)
            *lines, buffer = buffer.split(b"\n")
            for line in lines:
                if rows >= CAP:
                    break
                fields = line.rstrip(b"\r").split(b"|")
                if len(fields) != width:
                    continue
                for index, _, temporal in picked:
                    token = fields[index].decode("utf-8", "replace").strip().strip('"')
                    if token.lower() in ("", "null"):
                        token = ""
                    elif temporal:
                        token = epoch_seconds(token)
                    writers[index].write(token + "\n")
                rows += 1
    for writer in writers.values():
        writer.close()
    for index, file, _ in picked:
        partial[index].rename(directory / file)
    record("publicbi", table, rows, len(picked), digest.hexdigest()[:12], url)
    return f"{workbook}: {table} {len(picked)} columns x {rows} rows"


def fetch_public_bi(workers):
    (OUT / "publicbi").mkdir(parents=True, exist_ok=True)
    workbooks = sorted(entry["name"] for entry in json.load(get(PUBLIC_BI_LIST)))
    with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as pool:
        futures = {pool.submit(public_bi_table, workbook): workbook for workbook in workbooks}
        for future in concurrent.futures.as_completed(futures):
            try:
                print("publicbi", future.result(), flush=True)
            except Exception as error:  # one failed workbook must not stop the rest
                print(f"publicbi {futures[future]}: FAILED {error}", flush=True)


# ------------------------------------------------------------ DuckDB sources


def export_relation(con, group, table, relation, source):
    if table in done_tables(group):
        print(f"{group} {table}: kept", flush=True)
        return
    directory = OUT / group
    directory.mkdir(parents=True, exist_ok=True)
    rows = con.execute(f"SELECT count(*) FROM {relation}").fetchone()[0]
    if rows < MIN_ROWS:
        record(group, table, rows, 0, "-", source)
        return
    picked = 0
    described = con.execute(f"DESCRIBE SELECT * FROM {relation}").fetchall()
    for index, (name, kind, *_) in enumerate(described):
        kind = kind.lower()
        if kind.startswith(("date", "timestamp")):
            expression = f'epoch("{name}")::BIGINT'
            file = f"{table}__c{index}_{safe(name)}_timestamp.csv"
        elif re.match(r"^(u?tinyint|u?smallint|u?integer|u?bigint|decimal)", kind):
            expression = f'"{name}"'
            file = f"{table}__c{index}_{safe(name)}.csv"
        else:
            continue
        target = directory / file
        con.execute(
            f"COPY (SELECT {expression} FROM {relation} LIMIT {CAP}) TO '{target}' "
            "(FORMAT csv, HEADER false, NULLSTR '')"
        )
        picked += 1
    record(group, table, min(rows, CAP), picked, "-", source)
    print(f"{group} {table}: {picked} columns x {min(rows, CAP)} rows", flush=True)


def fetch_generated(group, extension, generator):
    con = duckdb.connect()
    con.execute(f"INSTALL {extension}; LOAD {extension}; CALL {generator}(sf=1);")
    for (table,) in con.execute("SHOW TABLES").fetchall():
        export_relation(con, group, table, table, f"duckdb {generator}(sf=1)")


def fetch_clickbench():
    if not CLICKBENCH.exists():
        print("clickbench: partition missing; run fetch_bench_data.py first", flush=True)
        return
    con = duckdb.connect()
    export_relation(con, "clickbench", "hits", f"read_parquet('{CLICKBENCH}')", CLICKBENCH.name)


def main(argv):
    groups = set(argv) or {"publicbi", "tpch", "tpcds", "clickbench"}
    OUT.mkdir(parents=True, exist_ok=True)
    if "tpch" in groups:
        fetch_generated("tpch", "tpch", "dbgen")
    if "tpcds" in groups:
        fetch_generated("tpcds", "tpcds", "dsdgen")
    if "clickbench" in groups:
        fetch_clickbench()
    if "publicbi" in groups:
        fetch_public_bi(workers=8)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
