use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;

use witness::access_compiler::{
    ClosureSpec, FieldLocation, Predicate, Query, Span, compile, encode, generate_rust_module,
    heldout_cases, input_for, primitive_rule_fingerprint,
};

const ROWS: usize = 16_384;
const GENERATED_DIR: &str = "experiments/generated/access_compiler";
const RESULT_DIR: &str = "experiments/results/access_compiler";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(GENERATED_DIR)?;
    fs::create_dir_all(format!("{RESULT_DIR}/pages"))?;
    let cases = heldout_cases();
    let mut columns = Vec::with_capacity(cases.len());
    let mut layouts = csv(format!("{RESULT_DIR}/layouts.csv"))?;
    let mut plans = csv(format!("{RESULT_DIR}/plans.csv"))?;
    writeln!(
        layouts,
        "case,recipe,file_bytes,field,field_name,logical_length,location,physical_offset,frame,decoded_offset,read_granularity"
    )?;
    writeln!(
        plans,
        "case,recipe,query,node,operation,row_start,row_end,guarantee,closure,logical_bytes,delivered_bytes,possible_fields,reason"
    )?;

    for case in &cases {
        let column = encode(&case.recipe, input_for(case, ROWS))?;
        column
            .page
            .write(format!("{RESULT_DIR}/pages/case_{:02}.acp", case.id))?;
        write_layout(case.id, &case.name(), &column, &mut layouts)?;
        for (query_name, query) in queries(&column.truth) {
            let plan = compile(&column, query)?;
            for node in plan.nodes {
                let (closure, logical, delivered, possible, reason) = match node.byte_closure {
                    ClosureSpec::Exact(closure) => (
                        "exact",
                        closure.logical_bytes,
                        closure.delivered_bytes,
                        String::new(),
                        String::new(),
                    ),
                    ClosureSpec::RuntimeRefined {
                        possible_fields,
                        reason,
                        ..
                    } => (
                        "runtime_refined",
                        0,
                        0,
                        possible_fields
                            .iter()
                            .map(|field| field.0.to_string())
                            .collect::<Vec<_>>()
                            .join("|"),
                        reason,
                    ),
                };
                writeln!(
                    plans,
                    "{},{},{},{},{:?},{},{},{:?},{},{},{},{},{}",
                    case.id,
                    quote(&case.name()),
                    query_name,
                    node.id.0,
                    node.op,
                    node.rows.0.start,
                    node.rows.0.end,
                    node.guarantee,
                    closure,
                    logical,
                    delivered,
                    quote(&possible),
                    quote(&reason),
                )?;
            }
        }
        columns.push(column);
    }
    layouts.flush()?;
    plans.flush()?;
    let generated = generate_rust_module(&columns)?;
    fs::write(format!("{GENERATED_DIR}/generated.rs"), generated)?;
    fs::write(
        format!("{RESULT_DIR}/rule_freeze.csv"),
        format!(
            "rule_version,fingerprint,heldout_compositions,rows_per_case\n2,{:#018x},{},{}\n",
            primitive_rule_fingerprint(),
            cases.len(),
            ROWS
        ),
    )?;
    println!(
        "generated {} held-out plans at fingerprint {:#018x}",
        cases.len(),
        primitive_rule_fingerprint()
    );
    Ok(())
}

fn write_layout(
    case: usize,
    recipe: &str,
    column: &witness::access_compiler::EncodedColumn,
    output: &mut impl Write,
) -> std::io::Result<()> {
    for field in &column.page.layout().fields {
        let (location, offset, frame, decoded_offset) = match field.location {
            FieldLocation::Direct { offset } => ("direct", offset, String::new(), 0),
            FieldLocation::Framed {
                frame,
                decoded_offset,
            } => ("framed", 0, frame.0.to_string(), decoded_offset),
        };
        writeln!(
            output,
            "{},{},{},{},{},{},{},{},{},{},{}",
            case,
            quote(recipe),
            column.page.bytes().len(),
            field.id.0,
            quote(&field.name),
            field.length,
            location,
            offset,
            frame,
            decoded_offset,
            field.read_granularity,
        )?;
    }
    Ok(())
}

fn queries(values: &[Option<i64>]) -> Vec<(&'static str, Query)> {
    let n = values.len();
    let width = (n / 100).max(1);
    let start = n / 2 - width / 2;
    let mut present = values.iter().flatten().copied().collect::<Vec<_>>();
    present.sort_unstable();
    let low = present[present.len() / 4];
    let middle = present[present.len() / 2];
    let high = present[present.len() * 3 / 4];
    vec![
        ("GET", Query::Get { row: n / 2 + 7 }),
        (
            "SUM_1PCT",
            Query::Sum {
                rows: Span::new(start, start + width).unwrap(),
            },
        ),
        (
            "BETWEEN_1PCT",
            Query::Between {
                rows: Span::new(start, start + width).unwrap(),
                low,
                high,
            },
        ),
        (
            "FILTER_POINT",
            Query::Filter {
                predicate: Predicate::Between {
                    low: middle,
                    high: middle,
                },
            },
        ),
        (
            "FILTER_50PCT",
            Query::Filter {
                predicate: Predicate::Between { low, high },
            },
        ),
    ]
}

fn csv(path: impl AsRef<Path>) -> std::io::Result<BufWriter<File>> {
    Ok(BufWriter::new(File::create(path)?))
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}
