use std::fs;

use witness::access_compiler::{encode, generate_rust_module, primitive_rule_fingerprint};
use witness::experiment::documentation::example_columns;

const GENERATED_DIR: &str = "experiments/generated/documentation_example";
const ARTIFACT_DIR: &str = "docs/generated/artifacts";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(GENERATED_DIR)?;
    fs::create_dir_all(ARTIFACT_DIR)?;
    let examples = example_columns();
    let columns = examples
        .iter()
        .map(|example| encode(&example.recipe, example.input()))
        .collect::<Result<Vec<_>, _>>()?;
    for (example, column) in examples.iter().zip(&columns) {
        column
            .page
            .write(format!("{ARTIFACT_DIR}/{}.acp", example.name))?;
    }
    fs::write(
        format!("{GENERATED_DIR}/generated.rs"),
        generate_rust_module(&columns)?,
    )?;
    println!(
        "generated {} documentation pages at rule fingerprint {:#018x}",
        columns.len(),
        primitive_rule_fingerprint()
    );
    Ok(())
}
