#[allow(dead_code)]
mod generated {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/experiments/generated/access_compiler/generated.rs"
    ));
}

#[path = "access_crossover/measure.rs"]
mod measure;
#[path = "access_crossover/model.rs"]
mod model;
#[path = "access_crossover/scan.rs"]
mod scan;
#[path = "access_crossover/storage.rs"]
mod storage;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    measure::run()
}
