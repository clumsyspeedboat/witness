#!/usr/bin/env python3
"""Fetch the string corpora for the string-page study.

For every table of

  tpch        TPC-H SF1 (duckdb dbgen),
  tpcds       TPC-DS SF1 (duckdb dsdgen),
  clickbench  the hits partition 0,
  publicbi    the first table of each of the 46 Public BI workbooks,

that has at least MIN_ROWS rows and a string column, writes
eval/data/strings/<group>/<table>.parquet: every string column of the table
(as bytes, so no text is re-encoded) and, as `measure`, the table's first
integer column, in the table's own row order, capped at CAP rows. Keeping a
table's columns in one file keeps their rows aligned.

Each completed table appends one line to the group's PROVENANCE.tsv; tables
already listed there are skipped, so the script is idempotent and resumable.

Stdlib + duckdb.
"""

import bz2
import concurrent.futures
import hashlib
import json
import re
import sys
import tempfile
import threading
import urllib.request
from pathlib import Path

import duckdb

ROOT = Path(__file__).resolve().parent.parent.parent
DATA = ROOT / "eval" / "data"
OUT = DATA / "strings"
CAP = 1_000_000
MIN_ROWS = 16_384

PUBLIC_BI = "https://raw.githubusercontent.com/cwida/public_bi_benchmark/master/benchmark"
PUBLIC_BI_LIST = "https://api.github.com/repos/cwida/public_bi_benchmark/contents/benchmark"
CLICKBENCH = DATA / "clickbench_hits_0.parquet"

STRING = re.compile(r"^(varchar|char|text|string|blob|bytea)\b", re.I)
INTEGER = re.compile(r"^(u?tinyint|u?smallint|u?integer|int|u?bigint)\b", re.I)

LOCK = threading.Lock()


def safe(name):
    return re.sub(r"[^A-Za-z0-9_]+", "_", name).strip("_") or "column"


def done_tables(group):
    provenance = OUT / group / "PROVENANCE.tsv"
    if not provenance.exists():
        return set()
    return {line.split("\t", 1)[0] for line in provenance.read_text().splitlines() if line}


def record(group, table, rows, columns, source):
    with LOCK:
        (OUT / group).mkdir(parents=True, exist_ok=True)
        with open(OUT / group / "PROVENANCE.tsv", "a") as provenance:
            provenance.write(f"{table}\t{rows}\t{columns}\t{source}\n")


def get(url, timeout=300):
    return urllib.request.urlopen(url, timeout=timeout)


# ------------------------------------------------------------ DuckDB sources


def export_relation(con, group, table, relation, source):
    if table in done_tables(group):
        print(f"{group} {table}: kept", flush=True)
        return
    rows = con.execute(f"SELECT count(*) FROM {relation}").fetchone()[0]
    described = con.execute(f"DESCRIBE SELECT * FROM {relation}").fetchall()
    strings = [
        (index, name, kind.lower().startswith(("blob", "bytea")))
        for index, (name, kind, *_) in enumerate(described)
        if STRING.match(kind)
    ]
    measure = next((name for name, kind, *_ in described if INTEGER.match(kind)), None)
    if rows < MIN_ROWS or not strings:
        record(group, table, rows, 0, source)
        return
    # Text becomes its UTF-8 bytes; binary columns are kept as they are.
    columns = [
        f'"{name}" AS "c{index}_{safe(name)}"'
        if binary
        else f'encode("{name}") AS "c{index}_{safe(name)}"'
        for index, name, binary in strings
    ]
    columns.append(f'"{measure}"::BIGINT AS measure' if measure else "NULL::BIGINT AS measure")
    directory = OUT / group
    directory.mkdir(parents=True, exist_ok=True)
    target = directory / f"{table}.parquet"
    con.execute(
        f"COPY (SELECT {', '.join(columns)} FROM {relation} LIMIT {CAP}) TO '{target}' "
        "(FORMAT parquet, COMPRESSION zstd)"
    )
    record(group, table, min(rows, CAP), len(strings), source)
    print(f"{group} {table}: {len(strings)} string columns x {min(rows, CAP)} rows", flush=True)


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


# ------------------------------------------------------------ Public BI


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
    strings = [(index, name) for index, (name, kind) in enumerate(schema) if STRING.match(kind)]
    measure = next((index for index, (_, kind) in enumerate(schema) if INTEGER.match(kind)), None)
    if not strings:
        record("publicbi", table, 0, 0, url)
        return f"{workbook}: no string columns"
    picked = [index for index, _ in strings] + ([measure] if measure is not None else [])
    width = len(schema)
    digest = hashlib.sha256()
    decompressor = bz2.BZ2Decompressor()
    buffer = b""
    rows = 0
    # Fields are stored as escaped tokens in a temporary file DuckDB reads.
    with tempfile.NamedTemporaryFile("wb", suffix=".tsv", delete=False) as staged:
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
                    tokens = []
                    for index in picked:
                        token = fields[index]
                        if token.strip().lower() in (b"", b"null"):
                            tokens.append(b"\\N")
                        else:
                            tokens.append(token.replace(b"\\", b"\\\\").replace(b"\t", b"\\t"))
                    staged.write(b"\t".join(tokens) + b"\n")
                    rows += 1
    if rows < MIN_ROWS:
        Path(staged.name).unlink()
        record("publicbi", table, rows, 0, url)
        return f"{workbook}: {table} too small"
    names = [f"c{index}_{safe(name)}" for index, name in strings]
    columns = {name: "VARCHAR" for name in names}
    if measure is not None:
        columns["measure"] = "VARCHAR"
    con = duckdb.connect()
    directory = OUT / "publicbi"
    directory.mkdir(parents=True, exist_ok=True)
    unescape = lambda name: (  # noqa: E731
        f"encode(replace(replace(\"{name}\", '\\t', chr(9)), '\\\\', '\\')) AS \"{name}\""
    )
    selected = [unescape(name) for name in names]
    selected.append("TRY_CAST(measure AS BIGINT) AS measure" if measure is not None else "NULL::BIGINT AS measure")
    con.execute(
        f"COPY (SELECT {', '.join(selected)} FROM read_csv('{staged.name}', delim='\\t', "
        f"header=false, quote='', escape='', nullstr='\\N', columns={json.dumps(columns)})) "
        f"TO '{directory / (table + '.parquet')}' (FORMAT parquet, COMPRESSION zstd)"
    )
    Path(staged.name).unlink()
    record("publicbi", table, rows, len(strings), url)
    return f"{workbook}: {table} {len(strings)} string columns x {rows} rows ({digest.hexdigest()[:12]})"


def fetch_public_bi(workers):
    workbooks = sorted(entry["name"] for entry in json.load(get(PUBLIC_BI_LIST)))
    with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as pool:
        futures = {pool.submit(public_bi_table, workbook): workbook for workbook in workbooks}
        for future in concurrent.futures.as_completed(futures):
            try:
                print("publicbi", future.result(), flush=True)
            except Exception as error:  # one failed workbook must not stop the rest
                print(f"publicbi {futures[future]}: FAILED {error}", flush=True)


def main(argv):
    groups = set(argv) or {"tpch", "tpcds", "clickbench", "publicbi"}
    OUT.mkdir(parents=True, exist_ok=True)
    if "tpch" in groups:
        fetch_generated("tpch", "tpch", "dbgen")
    if "tpcds" in groups:
        fetch_generated("tpcds", "tpcds", "dsdgen")
    if "clickbench" in groups:
        fetch_clickbench()
    if "publicbi" in groups:
        fetch_public_bi(workers=8)


if __name__ == "__main__":
    main(sys.argv[1:])
