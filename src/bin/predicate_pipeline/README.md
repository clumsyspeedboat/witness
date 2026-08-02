# Predicate Pipeline Study

```mermaid
flowchart TD
    ENTRY["predicate study"] --> MEASURE["measure.rs"]
    CORPUS["selector / value<br/>corpus"] --> MEASURE
    GENERATED["frozen kernels"] --> ENGINE["engine.rs"]
    WORK["workload.rs<br/>predicates + truth"] --> MEASURE
    ENGINE --> MEASURE

    ENGINE --> FILTER["selector filter<br/>compiled or scan"]
    FILTER --> RANGES["row ranges"]
    RANGES --> VALUE["value SUM<br/>selective or fused"]
    VALUE --> ANSWER["complete answer"]

    MEASURE --> RAW["raw result CSVs"]
    RAW --> SUMMARY["summary.rs<br/>cell metrics"]
    RAW --> SOURCE["source_summary.rs<br/>source medians"]
    SUMMARY --> OUT["summaries +<br/>diagnostics"]
    SOURCE --> OUT
```

- Every measured query is checked against `workload.rs` truth before timing is retained.
- Selector discovery and value aggregation are measured separately and end to end.
