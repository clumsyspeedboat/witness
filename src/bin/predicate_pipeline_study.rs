#[allow(dead_code, unused_imports)]
mod generated {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/experiments/generated/predicate_access/generated.rs"
    ));
}

#[path = "predicate_pipeline/engine.rs"]
mod engine;
#[path = "predicate_pipeline/measure.rs"]
mod measure;
#[path = "predicate_pipeline/source_summary.rs"]
mod source_summary;
#[path = "predicate_pipeline/summary.rs"]
mod summary;
#[path = "predicate_pipeline/workload.rs"]
mod workload;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    measure::run()
}
