# Experiment Library

```mermaid
flowchart TD
    DATA["datasets.rs<br/>pinned columns"] --> REAL["access_real.rs<br/>two page policies"]
    DATA --> CENSUS["invariant census"]
    DATA --> TIME["time_window.rs<br/>reference queries"]

    DOC["documentation.rs<br/>five-column example"] --> FORMATS["study_formats.rs<br/>file adapters"]
    REAL --> TIME
    REAL --> STUDIES["study binaries"]
    CENSUS --> STUDIES
    TIME --> STUDIES
    FORMATS --> STUDIES

    TYPES["types.rs<br/>column model"] -.-> DATA
    TYPES -.-> STUDIES
    STUDIES --> RESULTS["canonical CSVs<br/>physical artifacts"]
```

- Dataset bytes are fetched outside `src/` and verified against `INPUTS.sha256`.
- This layer owns corpus construction and measurements; query semantics remain in `access_compiler/`.
