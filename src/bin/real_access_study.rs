#[allow(dead_code, unused_imports)]
mod generated {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/experiments/generated/real_access/generated.rs"
    ));
}

#[path = "real_access/measure.rs"]
mod measure;
#[path = "real_access/scan.rs"]
mod scan;
#[path = "access_crossover/storage.rs"]
#[allow(dead_code)]
mod storage;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    measure::run()
}
