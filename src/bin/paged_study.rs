#[allow(dead_code)]
#[path = "predicate_pipeline/workload.rs"]
mod workload;

#[path = "paged_study/common.rs"]
mod common;
#[path = "paged_study/corpus.rs"]
mod corpus;
#[path = "paged_study/measure.rs"]
mod measure;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    measure::run()
}
