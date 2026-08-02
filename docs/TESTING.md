# Testing and Validation

Witness tests three different claims separately:

1. **Semantic correctness:** an encoded or selective operator returns the same
   answer as a simple row-wise reference implementation.
2. **Authorization soundness:** a non-scan algorithm is emitted only when an
   authenticated invariant is sufficient for that algorithm.
3. **Physical accounting:** every field, prerequisite, frame, alignment unit,
   and storage read charged to a plan is represented in its byte counters.

Passing these tests supports the implemented finite calculus and the measured
cases. It is not a machine-checked proof for arbitrary codecs, hardware, or
queries. The final section lists those boundaries explicitly.

## 1. Fast all-features suite

Run the complete Rust suite with:

~~~bash
cargo test --locked --all-features
~~~

At this revision, the command runs 56 named tests. They are divided by
failure domain so that a failure identifies whether the problem is semantic,
physical, generated-code, experimental, or documentary.

| Layer | File | Cases | Primary oracle |
|---|---|---:|---|
| library units | modules under `src/` | 10 | local invariants and deterministic examples |
| invariant calculus | `tests/invariant_calculus.rs` | 17 | derived fact versus decoded values and authorization checks |
| Decoder/Layout/Plan IR | `tests/access_compiler_ir.rs` | 9 | validated DAGs, closure fixed points, and typed guarantees |
| interpreted execution | `tests/access_compiler_exec.rs` | 4 | brute-force answers and malformed-input behavior |
| generated execution | `tests/access_compiler_generated.rs` | 3 | interpreter, monomorphized control, and frozen generated Rust |
| sidecar certificates | `tests/certificate_plans.rs` | 4 | brute-force membership with candidate refinement |
| physical study formats | `tests/study_formats.rs` | 3 | round-trip values, null positions, and reference windows |
| generated documentation | `tests/documentation_example.rs` | 2 | byte-for-byte regeneration and manifest-bound prose |
| predicate workload units | `src/bin/predicate_pipeline/workload.rs` | 3 | row-wise truth and deterministic predicate construction |
| crossover partition | `src/bin/access_crossover/measure.rs` | 1 | fixed 10-case training and 5-case evaluation split |

### Invariant cases

`tests/invariant_calculus.rs` deliberately separates facts that are easy to
conflate:

- Unsigned delta establishes non-decreasing order within its valid domain;
  signed delta does not.
- Multiple restart anchors establish piecewise order, not global order across
  restart seams.
- A sorted dictionary establishes an order-preserving value-to-code mapping;
  it does not establish row order in the ID stream.
- Nulls-first and nulls-last contracts can preserve boundary search. Arbitrary
  null placement cannot.
- Patch structure does not inherit order when an exception can break it.
- RLE run structure may authorize count-by-run; the same count on an
  unstructured stream falls back to value inspection.
- Extreme `i64` spans exercise widened arithmetic.
- Property-generated columns compare every derived root fact with decoded
  values and compare run-count plans with brute force.

Three negative authorization tests are load-bearing. They construct a fast
step with no cited fact, cite a fact absent from the column, or try to use a
run index as evidence for boundary search. All three must be rejected.

### IR and physical-closure cases

`tests/access_compiler_ir.rs` checks:

- Decoder nodes form a topologically valid DAG.
- A persisted invariant is accepted only with a valid descriptor checksum.
- A legacy descriptor without a certificate falls back to scan behavior.
- Repeated layout prerequisites converge to a fixed point.
- Delivery rounds to the declared physical granularity.
- Reading any byte inside an opaque frame charges the complete frame.
- Runtime-refined patch plans declare every field they may later open.
- Every plan node carries a row domain, required fields, byte closure, and
  output guarantee.

These tests distinguish logical bytes from delivered bytes. They do not claim
that the operating system or a device transfers exactly the delivered count;
storage experiments measure that third quantity separately.

### Execution and malformed-input cases

`tests/access_compiler_exec.rs` evaluates all held-out codec compositions
over GET, SUM, BETWEEN, and filter shapes. Answers are compared with a
row-wise reference. A full-page access control must charge the complete
serialized page.

Malformed descriptors, missing dependencies, invalid dictionary IDs, and
arbitrary byte strings up to 8 KiB must return an error rather than panic.
This is robustness testing, not a complete security audit.

### Generated-code cases

`tests/access_compiler_generated.rs` checks four independent properties:

- The exact source fingerprint matches the frozen generated source.
- Regeneration is byte-for-byte stable.
- Generated answers match brute force for every held-out composition/query.
- Generated field access is a subset of the declared plan closure.

Corrupt nullable, patch, and frame data must fail closed. A separate bundle
test verifies that kernels read serialized pages correctly when each page
starts at a nonzero bundle offset.

### Certificate cases

`tests/certificate_plans.rs` treats Bloom filters, min/max summaries, and
sparse fences as sidecars rather than implicit codec capabilities.

- Bloom misses and disjoint min/max ranges may reject a block.
- Bloom hits and min/max overlaps remain candidates until value refinement.
- Property-generated equality queries verify that no true match is dropped.
- Sparse fences are tested with duplicates and absent values.
- A fence is rejected when the column lacks an order certificate.

The oracle is always the exact set of matching rows from a direct scan.

### Policy-selection case

The unit test
`selection_policies_match_their_declared_contracts` uses a nullable
synthetic column and checks the names used throughout the artifact:

- `size_selected` equals the first candidate after sorting the fixed menu by
  complete serialized bytes and recipe name.
- `access_ready` is unframed.
- Every access-ready dependency is bounded by an index or fixed-width block.

This establishes selection within the declared finite menus. It does not
claim a globally size-optimal encoding.

## 2. Differential baselines

The compiler study evaluates six paths on the same serialized values:

| Path | Purpose |
|---|---|
| full decode and materialize | allocation-bearing reference |
| fused full-stream decode | no intermediate output vector |
| interpreted selective plan | semantic Plan IR control |
| generated full-page access | generated code with selective I/O removed |
| generated selective plan | compiler output under evaluation |
| handwritten monomorphized control | composition-specific Rust control |

All six must return the same answer checksum. The controls separate gains due
to selective access from gains due only to fusion or monomorphization.

Run:

~~~bash
./experiments/run_access_compiler.sh
~~~

The current source defines 19 compositions and seven query shapes, for 133
composition-query cells. Each cell has six baselines and seven timing
repetitions, so `benchmark.csv` has
`19 * 7 * 6 * 7 + 1 = 5,587` rows including its header. The script also
checks:

- 19 serialized pages and 799 summary rows;
- generated median runtime no more than 1.5x the handwritten control per cell;
- generated delivered bytes equal generated transferred bytes in this
  memory-level experiment;
- generated access never exceeds the handwritten control's declared access;
- every framed generated cell charges one complete frame;
- generated Rust contains no `DecoderNode` or recursive decoder-tree access.

These are executable gates, not manually inspected expectations.

## 3. Cost-model and crossover controls

Run:

~~~bash
./experiments/run_access_crossover.sh
~~~

Cases 0 through 9 fit the cost model; cases 10 through 14 are held out.
The study writes 4,080 selective/fused curve cells, evaluates 680 held-out
decisions, and runs 12 multipage policy/storage combinations.

Two feature modes are kept distinct:

- `preflight_free_estimate` uses request size, layout, and structure counts
  available without executing an exact-closure preflight.
- `exact_closure_features` is a diagnostic using measured closure features.

Only the first is used by the executable multipage cost policy. Its held-out
accuracy, regret bounds, checksum agreement, and per-tier comparison with
always-fused execution are enforced by the script.

## 4. Claim-bearing experiment gates

The full pipeline invokes these scripts after verifying input hashes.

| Script | Enforced checks |
|---|---|
| `run_invariant_census.sh` | at least 100 columns, 30 sources, at least eight candidates per column, authenticated order evidence, and nonzero global/page order incidence |
| `run_predicate_scale.sh` | identical frozen rule fingerprint at 16,384 and 131,072 rows |
| `run_predicate_pipeline.sh` | at least 30 selector/value pairs, at least 60 logical columns, two kernels per column, exact answer agreement across four known-selection and ten complete-query arms, and all expected physical files |
| `run_certificate_study.sh` | at least 1,700 cells, five plan classes, four query classes, and no non-scan candidate set larger than the scan domain |
| `run_real_access.sh` | 14 columns, 178 candidates, 336 query cells, 72 storage scans, policy/query consistency, cross-policy checksum equality, and storage-counter sanity |
| `claim_manifest` | fixed-seed source bootstrap, claim extraction from canonical CSV cells, and no hand-entered scalar values |
| `documentation_example` | deterministic pages, layouts, plans, CSVs, and Markdown generated from actual 16-row files |

The scripts use cardinality checks because a partially written CSV can
otherwise look plausible. They also compare answer checksums across policies;
a fast but wrong arm cannot survive by reporting only latency.

## 5. Reproduction levels

### Code and semantics

~~~bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-features
~~~

### Exact reported values from committed evidence

~~~bash
make claims
git diff --exit-code experiments/results/claim_manifest.csv
~~~

This does not remeasure timings. It reconstructs every displayed claim from
the committed canonical CSVs with a fixed bootstrap seed.

### Worked physical example

~~~bash
make walkthrough
git diff --exit-code docs/generated
~~~

This regenerates the 16-row files, offsets, field previews, plans, byte
closures, certificate behavior, and explanatory Markdown.

### Complete remeasurement

~~~bash
./reproduce.sh
~~~

The script fetches inputs, verifies `INPUTS.sha256`, runs the claim-bearing
studies, regenerates the walkthrough, records the environment, and writes
`CHECKSUMS.sha256`.

Structural facts, serialized bytes, answers, and claim extraction are
deterministic. Runtime samples, page-cache effects, and OS counters are
host-dependent; a new machine should not be expected to reproduce identical
nanoseconds.

## 6. Interpreting failures

| Failure | Likely meaning |
|---|---|
| rule fingerprint mismatch | claim-bearing primitive or generator source changed without regeneration |
| answer/checksum mismatch | semantic error in a codec, plan, generated kernel, or baseline |
| closure-subset failure | generated code touched an undeclared field or byte range |
| frame-delivery failure | logical selection bypassed physical frame granularity |
| authorization failure | a fast algorithm lacks sufficient authenticated evidence |
| cardinality failure | a case was added/removed or a study stopped before completing |
| generated-doc drift | the checked-in walkthrough does not match current code/results |
| claim-prose drift | a hand-written summary no longer matches its named manifest cells |
| input checksum failure | downloaded corpus bytes differ from the pinned study inputs |

A changed cardinality should be explained from source structure before a gate
is updated. For example, adding one composition changes several CSV counts;
editing the expected number alone is not sufficient.

## 7. Boundaries of the test evidence

The suite does not establish:

- formal soundness for decoder primitives outside the implemented finite IR;
- memory safety of external C/C++ codec libraries;
- correctness under concurrent mutation or transactions;
- identical cold-cache behavior across filesystems and devices;
- workload prevalence beyond the pinned corpora;
- globally optimal codec selection or read scheduling;
- protection against every hostile serialized input.

Claims in code and documentation should remain within the narrower behavior
that the relevant test, property, or experiment directly checks.
