fn main() -> Result<(), Box<dyn std::error::Error>> {
    witness::experiment::invariant_census::run("experiments/results/invariant_census", 65_536)
}
