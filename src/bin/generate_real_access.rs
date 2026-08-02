use std::fs::{self, File};
use std::io::{BufWriter, Write};

use witness::access_compiler::{generate_rust_module, primitive_rule_fingerprint};
use witness::experiment::access_real::real_access_columns;

const GENERATED_DIR: &str = "experiments/generated/real_access";
const RESULT_DIR: &str = "experiments/results/real_access";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(GENERATED_DIR)?;
    fs::create_dir_all(format!("{RESULT_DIR}/pages"))?;
    let columns = real_access_columns()?;
    let mut selection = csv(format!("{RESULT_DIR}/selection.csv"))?;
    let mut candidates = csv(format!("{RESULT_DIR}/candidates.csv"))?;
    writeln!(
        selection,
        "column,group,source,name,rows,nulls,selected_recipe,file_bytes,runner_up_bytes,margin_bytes"
    )?;
    writeln!(
        candidates,
        "column,group,source,name,rows,recipe,file_bytes,selected"
    )?;
    for (index, column) in columns.iter().enumerate() {
        let selected = column.size_selected.recipe.name();
        let selected_bytes = column.size_selected.page.bytes().len();
        let runner_up = column
            .candidates
            .iter()
            .find(|candidate| candidate.recipe != selected)
            .map_or(selected_bytes, |candidate| candidate.bytes);
        writeln!(
            selection,
            "{},{},{},{},{},{},{},{},{},{}",
            index,
            quote(&column.group),
            quote(&column.source),
            quote(&column.name),
            column.size_selected.truth.len(),
            column.nulls,
            quote(&selected),
            selected_bytes,
            runner_up,
            runner_up.saturating_sub(selected_bytes),
        )?;
        for candidate in &column.candidates {
            writeln!(
                candidates,
                "{},{},{},{},{},{},{},{}",
                index,
                quote(&column.group),
                quote(&column.source),
                quote(&column.name),
                column.size_selected.truth.len(),
                quote(&candidate.recipe),
                candidate.bytes,
                usize::from(candidate.recipe == selected),
            )?;
        }
        column
            .size_selected
            .page
            .write(format!("{RESULT_DIR}/pages/column_{index:02}.acp"))?;
    }
    selection.flush()?;
    candidates.flush()?;
    let encoded = columns
        .iter()
        .map(|column| column.size_selected.clone())
        .collect::<Vec<_>>();
    fs::write(
        format!("{GENERATED_DIR}/generated.rs"),
        generate_rust_module(&encoded)?,
    )?;
    let corpus_fingerprint = columns
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, column| {
            column
                .size_selected
                .page
                .bytes()
                .iter()
                .fold(hash, |hash, byte| {
                    (hash ^ u64::from(*byte)).wrapping_mul(0x1000_0000_01b3)
                })
        });
    fs::write(
        format!("{RESULT_DIR}/freeze.csv"),
        format!(
            "rule_fingerprint,corpus_fingerprint,columns,max_rows\n{:#018x},{:#018x},{},{}\n",
            primitive_rule_fingerprint(),
            corpus_fingerprint,
            columns.len(),
            witness::experiment::access_real::real_access_rows()?,
        ),
    )?;
    println!(
        "generated {} real-column kernels at corpus fingerprint {corpus_fingerprint:#018x}",
        columns.len()
    );
    Ok(())
}

fn csv(path: impl AsRef<std::path::Path>) -> std::io::Result<BufWriter<File>> {
    Ok(BufWriter::new(File::create(path)?))
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}
