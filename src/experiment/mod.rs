//! Corpus loading, physical formats, and reusable study measurements.
//!
//! Study binaries orchestrate this library and the access compiler; this
//! module keeps data preparation and measurement logic out of entry points.

/// Frozen codec-menu selection for the real access-compiler corpus.
pub mod access_real;
/// Local NAB CSV, synthetic, and Yellow Taxi Parquet column loaders.
pub mod datasets;
/// Deterministic five-column example used to generate the public walkthrough.
pub mod documentation;
/// Source-aware semantic, mapping, and physical invariant census.
pub mod invariant_census;
/// Self-contained physical artifacts used by the canonical study.
pub mod study_formats;
/// Exact timestamp-window SUM and instrumented selective Parquet controls.
pub mod time_window;
/// Shared experiment column representation and pinned input paths.
pub mod types;
