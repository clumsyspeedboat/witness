use std::fs::{self, File};
use std::io::{BufWriter, Write};

use witness::access_compiler::{generate_rust_module, primitive_rule_fingerprint};
use witness::experiment::access_real::{predicate_access_corpus, real_access_rows};

const GENERATED_DIR: &str = "experiments/generated/predicate_access";
const DEFAULT_RESULT_DIR: &str = "experiments/results/predicate_pipeline";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(GENERATED_DIR)?;
    let result_dir =
        std::env::var("WITNESS_PREDICATE_RESULT_DIR").unwrap_or_else(|_| DEFAULT_RESULT_DIR.into());
    fs::create_dir_all(&result_dir)?;
    let page_dir = format!("{result_dir}/pages");
    if std::path::Path::new(&page_dir).exists() {
        fs::remove_dir_all(&page_dir)?;
    }
    fs::create_dir_all(&page_dir)?;
    let (columns, pairs) = predicate_access_corpus()?;
    let mut manifest = BufWriter::new(File::create(format!("{result_dir}/columns.csv"))?);
    writeln!(
        manifest,
        "column,group,source,name,rows,nulls,recipe,bytes,direct_recipe,direct_bytes,non_decreasing"
    )?;
    for (index, column) in columns.iter().enumerate() {
        writeln!(
            manifest,
            "{},{},{},{},{},{},{},{},{},{},{}",
            index,
            quote(&column.group),
            quote(&column.source),
            quote(&column.name),
            column.size_selected.truth.len(),
            column.nulls,
            quote(&column.size_selected.recipe.name()),
            column.size_selected.page.bytes().len(),
            quote(&column.access_ready.recipe.name()),
            column.access_ready.page.bytes().len(),
            column.size_selected.page.invariants().non_decreasing,
        )?;
        column
            .size_selected
            .page
            .write(format!("{result_dir}/pages/column_{index:03}_storage.acp"))?;
        column
            .access_ready
            .page
            .write(format!("{result_dir}/pages/column_{index:03}_direct.acp"))?;
    }
    manifest.flush()?;
    let encoded = columns
        .iter()
        .map(|column| column.size_selected.clone())
        .chain(columns.iter().map(|column| column.access_ready.clone()))
        .collect::<Vec<_>>();
    fs::write(
        format!("{GENERATED_DIR}/generated.rs"),
        generate_rust_module(&encoded)?,
    )?;
    fs::write(
        format!("{result_dir}/freeze.csv"),
        format!(
            "rule_fingerprint,columns,pairs,max_rows\n{:#018x},{},{},{}\n",
            primitive_rule_fingerprint(),
            encoded.len(),
            pairs.len(),
            real_access_rows()?
        ),
    )?;
    println!(
        "generated {} kernels for {} predicate pairs",
        encoded.len(),
        pairs.len()
    );
    Ok(())
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}
