# Access Compiler Flow

```mermaid
flowchart TD
    API["mod.rs API<br/>recipe + query"] --> ENC["encode.rs"]
    ENC --> DEC["decoder.rs<br/>meaning"]
    ENC --> LAY["layout.rs<br/>reachability"]
    ENC --> PAGE["format.rs<br/>bytes + checked facts"]

    DEC --> FACT["invariants.rs<br/>derived facts"]
    LAY --> FACT
    PAGE --> FACT
    FACT --> COMP["compiler.rs<br/>authorize"]
    LAY --> COMP
    PAGE --> COMP
    API --> COMP
    COMP --> PLAN["plan.rs<br/>typed plan"]

    PLAN --> EXEC["interpreter.rs<br/>codegen.rs<br/>static_baseline.rs"]
    PAGE --> RUN["runtime.rs<br/>read session"]
    LAY --> RUN
    EXEC --> RUN
    RUN --> ANSWER["answer +<br/>access metrics"]

    CERT["certificates.rs<br/>Bloom / minmax / fence"] --> REFINE["candidate blocks"]
    COST["cost_model.rs"] --> POLICY["selective / fused"]
    SCHED["schedule.rs"] --> READS["coalesced reads"]
    HELD["heldout.rs<br/>test compositions"] -.-> ENC
    HELD -.-> EXEC
    SUPPORT["support.rs<br/>kernel helpers"] -.-> EXEC
    IDS["ids.rs<br/>typed handles"] -.-> DEC
    IDS -.-> LAY
    IDS -.-> PLAN
```

- A fast plan must cite an authenticated fact; otherwise compilation falls back to scanning or decoding.
- `ByteClosure` separates logical, delivered, and transferred bytes.
- `Static` in `static_baseline.rs` means Rust type-level monomorphization,
  not a fixed query or access range.
