#!/usr/bin/env python3
"""Public BI subset fetcher (Task 1). Downloads ~5 numeric-heavy tables from the
cwida/public_bi_benchmark data hosts, extracts numeric columns (per the table
schema), caps rows, and writes one token-per-line file per column into
eval/data/publicbi/ for the datasets.rs single registry. Records provenance and
the exact table+column list. On total failure, writes PUBLICBI_MISSING.txt with
the URLs tried. Stdlib only (urllib, bz2)."""

import bz2
import hashlib
import os
import re
import sys
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
OUT = os.path.join(ROOT, "eval/data/publicbi")
DATA = os.path.join(ROOT, "eval/data")
RAW = "https://raw.githubusercontent.com/cwida/public_bi_benchmark/master/benchmark"

TABLES = ["Arade", "Bimbo", "CMSprovider", "Euro2016", "Generico"]
MAX_COLS = 6          # numeric columns per table
MAX_ROWS = 1_000_000  # row cap per column (≥ cold threshold, ~taxi/household scale)
NUMERIC = re.compile(r"\b(decimal|numeric|double|real|float|bigint|integer|smallint|tinyint|int)\b", re.I)


def get(url, timeout=240):
    req = urllib.request.Request(url, headers={"User-Agent": "witness-pbi"})
    return urllib.request.urlopen(req, timeout=timeout)


# the index we care about is the *physical column position*, so track all columns
def parse_columns(table):
    try:
        sql = get(f"{RAW}/{table}/tables/{table}_1.table.sql").read().decode("utf-8", "replace")
    except Exception as e:
        print(f"  {table}: schema fetch failed: {e}")
        return None
    body = sql[sql.find("(") + 1: sql.rfind(")")]
    cols = []
    for raw_line in body.split("\n"):
        m = re.match(r'\s*"?([^"]+?)"?\s+([A-Za-z][A-Za-z0-9]*)', raw_line)
        if m:
            cols.append((m.group(1).strip(), m.group(2).upper()))
    return cols


def data_url(table):
    try:
        u = get(f"{RAW}/{table}/data-urls.txt").read().decode().strip().splitlines()
        return u[0] if u else None
    except Exception:
        return None


def main():
    os.makedirs(OUT, exist_ok=True)
    tried, prov, ok_any = [], [], False
    for table in TABLES:
        cols = parse_columns(table)
        url = data_url(table)
        if not cols or not url:
            tried.append(f"{table}: schema/url unavailable")
            continue
        numeric_idx = [(i, n) for i, (n, t) in enumerate(cols) if NUMERIC.search(t)][:MAX_COLS]
        if not numeric_idx:
            tried.append(f"{table}: no numeric columns")
            continue
        tried.append(url)
        print(f"{table}: {len(cols)} cols, taking numeric {[n for _, n in numeric_idx]} from {url}")
        try:
            h = hashlib.sha256()
            writers = {i: open(os.path.join(OUT, f"{table}__c{i}_{_safe(n)}.csv"), "w")
                       for i, n in numeric_idx}
            idxs = list(writers)
            decomp = bz2.BZ2Decompressor()
            buf = b""
            rows = 0
            with get(url) as resp:
                while rows < MAX_ROWS:
                    chunk = resp.read(1 << 20)
                    if not chunk:
                        break
                    h.update(chunk)
                    buf += decomp.decompress(chunk)
                    *lines, buf = buf.split(b"\n")
                    for line in lines:
                        if rows >= MAX_ROWS:
                            break
                        f = line.split(b"|")
                        if len(f) <= idxs[-1]:
                            continue
                        for i in idxs:
                            tok = f[i].decode("utf-8", "replace").strip()
                            writers[i].write(("" if tok.lower() in ("", "null") else tok) + "\n")
                        rows += 1
            for w in writers.values():
                w.close()
            prov.append((table, rows, [n for _, n in numeric_idx], h.hexdigest()[:12]))
            ok_any = True
            print(f"  wrote {len(numeric_idx)} cols × {rows} rows")
        except Exception as e:
            tried[-1] += f"  (FAILED: {e})"
            print(f"  {table}: download/extract failed: {e}")

    if not ok_any:
        with open(os.path.join(DATA, "PUBLICBI_MISSING.txt"), "w") as f:
            f.write("Public BI subset fetch FAILED.\nURLs/sources tried:\n" + "\n".join(tried) + "\n")
        print("Public BI: all attempts failed -> PUBLICBI_MISSING.txt")
        return 1

    with open(os.path.join(OUT, "PROVENANCE.md"), "w") as f:
        f.write("# Public BI subset (Task 1)\n\nSource: cwida/public_bi_benchmark data-urls "
                "(event.cwi.nl host). Capped: ≤%d numeric cols/table, ≤%d rows.\n\n" % (MAX_COLS, MAX_ROWS))
        f.write("| table | rows | numeric columns | bz2 sha256(12) |\n|---|---|---|---|\n")
        for t, r, ns, h in prov:
            f.write(f"| {t} | {r} | {', '.join(ns)} | {h} |\n")
    print(f"Public BI: {len(prov)} tables extracted -> {OUT}")
    return 0


def _safe(s):
    return re.sub(r"[^A-Za-z0-9]+", "_", s)[:40].strip("_") or "col"



if __name__ == "__main__":
    sys.exit(main())
