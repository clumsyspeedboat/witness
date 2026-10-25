//! Controlled diagnostic; writes separately from the claim-bearing studies.

#[allow(dead_code)]
#[path = "paged_study/common.rs"]
mod common;
#[path = "paged_study/corpus.rs"]
mod corpus;
#[path = "evidence_layout/measure.rs"]
mod measure;
#[allow(dead_code)]
#[path = "predicate_pipeline/workload.rs"]
mod workload;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    measure::run()
}
