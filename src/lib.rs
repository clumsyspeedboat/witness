//! **Witness** — a research demonstrator for query execution driven by facts
//! that a compressed column layout already proves about its data.
//!
//! A column is encoded as a composition of small standard codecs (bit-packing,
//! FOR, delta, dictionary, RLE, patching, null stripping). The composition is
//! not only a size reduction: it *witnesses* invariants. Unsigned deltas prove
//! segment monotonicity; a sorted deduplicated dictionary proves an
//! order-preserving code mapping; restart, run, and rank indexes bound the
//! bytes any row range can require. The claim-bearing layers are:
//!
//! * **Invariant calculus** ([`access_compiler::derive_invariants`]): typed
//!   facts (value / mapping / access) with explicit evidence and assumptions,
//!   derived soundly from the decoder tree and a checksummed descriptor.
//! * **Query compiler** ([`access_compiler::compile`]): derived facts
//!   authorize algorithms (boundary search vs. scan), byte closures state
//!   declared physical prerequisites, and generated kernels execute the plan.
//!
//! The default core is dependency-free. Everything that touches Arrow,
//! Parquet, or comparison codecs lives behind the `experiment` feature.

#[cfg(feature = "experiment")]
pub mod access_compiler;
#[cfg(feature = "experiment")]
pub mod experiment;
