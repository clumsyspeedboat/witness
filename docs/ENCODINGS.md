# Encodings, evidence, and comparators

This document describes the current Witness study. The older `.gen/.genz`
format is legacy and is not the claim-bearing representation.

## 1. Current executable primitives

| Primitive | Serialized fields | Logical reconstruction | Fact or mapping available | Access prerequisite |
|---|---|---|---|---|
| BitPack | fixed-width miniblocks | unpack unsigned codes | code width/range | intersecting miniblock bytes |
| FOR | i64 base + child codes | `base + child` | affine, order-preserving shift | base + child closure |
| signed Delta | restart anchors + ZigZag child | prefix sum per restart | no monotonicity proof | preceding restart + deltas through target |
| unsigned Delta | restart anchors + unsigned child | prefix sum per restart | segment monotonicity; global only with one segment or checked seams | same restart closure |
| Dictionary | sorted unique i64 table + ID child | `dictionary[id]` | injective and order-preserving ID mapping | ID bytes + referenced table entries |
| RLE | run values + u32 lengths + sparse run index | repeat each run value | run span semantics | run index + intersecting lengths + values |
| Patch | main child + positions/index/exceptions | replace listed rows | exact exception semantics | position index + relevant positions/exceptions + main body |
| Nullable | validity + rank index + compact child | insert nulls around compact values | non-null ordering; full ordering depends on null placement | validity/rank + compact child |
| Frame | one Zstd frame over child fields | unchanged after frame decode | no new value fact | entire compressed frame |

The serializer writes named fields, exact lengths, alignment, read granularity,
frame locations, and dependency rules into an authenticated `ACPAGE01`
descriptor. The generated example lists every field and physical offset for
five real pages.

## 2. Current selection policies

The study uses two deterministic menus over the same primitive set.

### Size-selected

For each column, encode all applicable direct and Zstd-framed candidates and
select the smallest complete serialized page. Current base candidates are
FOR/bitpack, signed delta, unsigned delta, FOR-over-delta, RLE-over-FOR,
dictionary/bitpack, and dictionary-over-RLE.

### Access-ready

Select the smallest direct candidate with bounded local dependencies. Opaque
frames are excluded, and delta restart intervals are reduced to bound point
and selective-range reconstruction.

This policy difference is intentional. It creates the measured storage/access
frontier: the access-ready layout uses 2.10x the bytes of the size-selected
layout at the canonical aggregate, while its source-level delivered-access
median is 0.18x.

These are finite policies, not an exhaustive global optimizer.

## 3. Query behavior by primitive

| Query | Narrow path when justified | Otherwise |
|---|---|---|
| GET(i) | local bit range plus bases, restart, run, patch, rank, dictionary dependencies | decode the required composition |
| SUM(l,r) | fused visit over encoded spans; RLE multiplies value by span length | full fused traversal |
| BETWEEN(l,r,a,b) | translated dictionary ID bounds where mapping is proved | fused compare over selected rows |
| FILTER BETWEEN(a,b) | global binary search for checked monotone data; independent per-block search for piecewise monotone data; dictionary-bound translation | full fused scan |
| EQ / IN control | Bloom, min/max, or sparse-fence candidate blocks, then refinement | scan |

SUM is not metadata-only in the current claim-bearing pages unless an
independent exact aggregate certificate exists. Avoiding an output array is
not the same as avoiding residual or value decoding.

## 4. Physical artifacts executed in this repository

| Artifact | What is real here | Unit in canonical studies | Query role |
|---|---|---|---|
| CSV | actual UTF-8 bytes and parser | table or column, explicitly labeled | unindexed text baseline/example |
| RAWI64V1 | actual validity + dense little-endian i64 file | self-contained column | transparent byte floor |
| raw + Zstd | actual Zstd frame over RAWI64V1 | self-contained column | entropy-frame boundary |
| PCodec | real PCO payload with a tested validity wrapper | self-contained column | compression-first reference |
| Parquet | Arrow/Parquet writer and reader; dictionary/Snappy and delta/Zstd; three page-size query arms | physical table/column file | mature general-purpose reference |
| ORC | ORC-Rust writer/reader using RLEv2; no outer compression exposed by this writer version | physical table/column file | format and index prior-art control |
| Witness | actual `.acp` pages, generated kernels, memory/file-backed reads | self-contained column pages and measured bundles | invariant/access compiler prototype |

The 16-row generated example produces and round-trips all of these. Its byte
table is a mechanics check, not a ratio result: table containers and sums of
independent column files pay different fixed metadata, and 16 rows are header
dominated.

## 5. Measured comparison boundaries

### Parquet

The claim-bearing predicate experiment includes:

- full decode;
- exact row-filter execution;
- boundary-order page search using Parquet metadata;
- three page sizes, including an equal-granularity comparison;
- complete predicate-to-SUM answers checked for equality before timing.

The source-bootstrap interval for access-ready Witness versus the Parquet
boundary path is [0.10, 0.51], below parity.

### PCodec and ORC

PCodec and ORC are real artifacts in the physical format studies and the
generated mechanics example. They are not used as fabricated selective-query
implementations. PCodec represents a compression-first point. ORC documents
that indexes, Bloom filters, and stored aggregates are established format
techniques.

### Bloom, min/max, sparse fence

These are separately budgeted control certificates. They do not become
“free” Witness capabilities. The canonical Bloom experiment reports:

| Probe class | candidate fraction | modeled bytes / scan |
|---|---:|---:|
| absent | 0.000 | 0.172 |
| rare | 0.064 | 0.242 |
| frequent | 1.000 | 1.172 |
| mixed IN | 1.000 | 1.172 |

A candidate result is refined against values before becoming an exact answer.

## 6. Literature-only systems

The following systems are important conceptual neighbors, but this repository
does not build their artifacts or report local timings:

| System | Relevant documented idea | Scope here |
|---|---|---|
| FastLanes | high-throughput decoding and composable layouts | related work only |
| BtrBlocks | lightweight adaptive columnar compression | related work only |
| LeCo | learned serial correlation with random access | related work only |
| White-box Compression | learned table expressions and execution | related work only |
| Vortex | extensible nested encodings and compute | related work only |

They are literature-only context: no symbolic byte picture is presented as
if it were a local measurement.

## 7. What Witness is testing

Witness does not claim that compressed execution, dictionaries, zone maps,
Bloom filters, restart points, or materialized aggregates are new in
isolation.

The tested contribution is the composition:

```text
decoder semantics
+ authenticated facts
+ physical dependencies
+ output guarantees
        |
        v
sound generated query path
+ explicit byte closure
+ explicit fallback
```

The research question is whether this derivation can remove enough work on
real columns to justify its storage and physical-access cost. Current results
say “sometimes,” with the confidence interval and negative classes reported
rather than hidden.
