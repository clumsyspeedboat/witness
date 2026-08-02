# Real Access Study

```mermaid
flowchart TD
    ENTRY["real-access study"] --> MEASURE["measure.rs"]
    CORPUS["real columns"] --> MEASURE
    GENERATED["frozen kernels"] --> MEASURE
    SHARED["shared storage<br/>tiers"] --> MEASURE

    MEASURE --> CELL["selective / fused<br/>cells"]
    CELL --> QUERYCSV["query CSVs"]
    MEASURE --> SCAN["scan.rs<br/>1,024 pages"]
    SHARED --> SCAN
    SCAN --> CLOSURE["page closures"]
    CLOSURE --> COALESCE["coalesced<br/>read schedules"]
    COALESCE --> DEVICES["workspace / tmp<br/>memory-backed"]
    DEVICES --> OUT["storage_scan.csv"]
```

- `storage.rs` is intentionally shared with the crossover study through an explicit path import.
- Scan answers must match across every access order, storage tier, and coalescing policy.
