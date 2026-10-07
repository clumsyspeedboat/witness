# Witness walkthrough

## 1. A representation can be evidence

Suppose a column contains:

```text
1000, 1002, 1004, 1007, 1009, 1011, 1013, 1015, 1018, 1020, 1022, 1025
```

A conventional reader treats the encoded bytes as a compact obstacle between
the query and these values. Witness asks a different question: does the chosen
encoding itself prove something useful?

Store the first value and unsigned deltas:

```text
anchor = 1000
deltas = 2, 2, 3, 2, 2, 2, 2, 3, 2, 2, 3
```

Because every code is unsigned, no represented step can be negative. Within
that restart segment, the representation proves that values are
non-decreasing. The proof is not a heuristic about likely data. It follows
from what the format can express.

That fact authorizes an exact boundary search for:

```sql
value BETWEEN 1004 AND 1015
```

The answer is rows `[2, 8)`. A scan is still correct, but it is no longer the
only sound algorithm.

## 2. Three descriptions, not one decoder function

A useful compressed operator needs more than the algebraic decoder.

### Decoder IR: what values mean

```text
UnsignedDelta(
  restarts,
  BitUnpack(delta_stream, width=2)
)
```

This states how logical values are reconstructed.

### Layout IR: where bytes live

```text
metadata       -> bytes [0, 320)
restarts       -> bytes [320, 328), granularity 64
delta stream   -> bytes [384, 480), granularity 96

delta stream requires:
  metadata
  the restart covering the requested segment
```

This states alignment, granularity, restart dependencies, indexes, and
enclosing compression frames.

### Invariants: what is proved

```text
PiecewiseNonDecreasing(max_rows=restart_interval)
evidence = Structural
assumptions = checked arithmetic
```

A separate encoder check may authenticate a stronger global
`NonDecreasing` fact when restart seams are also ordered. The distinction is
essential: unsigned deltas prove order inside each restart segment, not across
independent anchors.

## 3. From a query to a plan

The compiler consumes:

```text
(query, decoder IR, layout IR, checked facts)
```

and emits Plan IR nodes carrying:

```text
(row domain, required fields, byte closure, output guarantee)
```

For a globally monotone filter, generated code performs two binary searches.
For piecewise monotone data, it performs independent exact boundary searches
inside every restart block. It does not claim that one block can be skipped
from another block's values unless separate block bounds exist.

For a sorted unique dictionary, the compiler translates value bounds into ID
bounds, then evaluates encoded IDs. For RLE, patch, and nullable layouts, the
physical closure includes run indexes, patch positions and exceptions, or
validity and rank data.

When no fact permits a narrower algorithm, the plan becomes a fused scan.
Fallback is a correct compiler result, not a hidden failure.

## 4. Physical access closure

A logical request is not yet a device read. Let `A0` be the directly required
field spans. Apply layout prerequisites until a fixed point:

```text
A(k+1) = A(k) union Prerequisites(A(k))
stop when A(k+1) = A(k)
```

Examples:

- FOR point lookup needs the base and the intersecting packed bytes.
- Delta lookup also needs the preceding restart and intervening deltas.
- Dictionary lookup needs IDs and only the referenced dictionary entries.
- Patched FOR needs base data plus the relevant position/index/exception
  records.
- Nullable data needs validity and rank information before compact-value rows
  are known.
- Any field inside a Zstd frame forces delivery of the complete frame.

Witness reports three byte notions:

| Counter | Meaning |
|---|---|
| logical | bytes belonging to requested fields |
| delivered | bytes forced by blocks, granularity, alignment, or frames |
| transferred | newly moved bytes after overlap and cache reuse |

A plan can minimize logical bytes and still perform poorly if it creates many
small physical reads. The cold XFS experiment measures exactly this boundary:
per-page closure read fewer bytes but issued 1,083 calls and took 1.28x a full
read; coalescing reached 119 calls and 1.00x.

## 5. Output guarantees

Every plan declares what it returns:

| Guarantee | Meaning |
|---|---|
| exact scalar | final GET or SUM result |
| exact bitmap | final qualifying rows |
| candidate bitmap | a superset that requires refinement |
| materialized values | decoded boundary or fallback rows |
| fallback required | the plan language has no justified narrower path |

This prevents a common error with probabilistic or coarse metadata. A Bloom
filter miss is an exact empty result. A Bloom hit is only a candidate. A
min/max overlap is only a candidate. A sparse fence narrows a sorted search
interval but does not prove the row itself matches.

## 6. Aggregates are not automatically metadata-only

For values `x(i) = g(i) + r(i)`:

```text
SUM(x) = SUM(g) + SUM(r)
```

The identity is true, but it does not make `SUM(r)` free. Unless an exact
residual sum or aggregate certificate is stored, residuals still need to be
visited. Witness distinguishes:

- metadata-only aggregate, when a certified summary exists;
- fused encoded traversal, which avoids materializing a decoded array but
  still reads encoded values;
- full decode/materialization fallback.

The current claim-bearing prototype generally uses fused traversal for SUM.
It does not relabel “no materialized array” as “no decoding.”

## 7. One real generated example

The canonical worked example is
[END_TO_END_EXAMPLE.md](generated/END_TO_END_EXAMPLE.md). Rust generates:

- a 16-row, five-column logical table;
- five actual `.acp` pages with exact offsets and stored field values;
- one real CSV, two real Parquet tables, one real ORC table, and
  representative raw/Zstd/PCodec artifacts;
- generated GET, SUM, monotone filter, dictionary filter, nullable, and patch
  query paths;
- Bloom, min/max, and sparse-fence candidate/refinement examples;
- exact logical, delivered, and transferred bytes;
- a snapshot read from the generated claim manifest.

The small example is for mechanics, not compression or latency ranking.
Fixed headers dominate 16 rows, and the document says so next to the numbers.

## 8. What the measured study says

The generated [claim manifest](../experiments/results/claim_manifest.csv) is
the authoritative mapping from reported claim names to exact displayed
values. The final section of the
[worked example](generated/END_TO_END_EXAMPLE.md) renders the headline values
from that same manifest.

The result is conditional: representation-derived facts can remove discovery
work when useful evidence is present and physically accessible. The same
mechanism abstains when the representation proves no narrower algorithm.
