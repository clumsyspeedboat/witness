#[allow(dead_code)]
#[path = "predicate_pipeline/workload.rs"]
mod workload;

#[path = "paged_study/attested_faults.rs"]
mod attested_faults;
#[path = "paged_study/checker.rs"]
mod checker;
#[path = "paged_study/common.rs"]
mod common;
#[path = "paged_study/corpus.rs"]
mod corpus;
#[path = "paged_study/deletes.rs"]
mod deletes;
#[path = "paged_study/faults.rs"]
mod faults;
#[path = "paged_study/measure.rs"]
mod measure;
#[path = "paged_study/menus.rs"]
mod menus;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    measure::run()
}
