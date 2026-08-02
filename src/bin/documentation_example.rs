use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::Cursor;

use arrow_array::{Array, Int64Array};
use bytes::Bytes;
use orc_rust::arrow_reader::ArrowReaderBuilder as OrcReaderBuilder;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use witness::access_compiler::{
    Answer, BlockBloom, BlockMinMax, ClosureMode, ClosureSpec, EncodedColumn, Execution,
    FieldLayout, FieldLocation, OutputGuarantee, Predicate, Query, Span, SparseFence, compile,
    encode, primitive_rule_fingerprint, refine_eq,
};
use witness::experiment::documentation::{
    EXAMPLE_ROWS, ExampleColumn, example_columns, table_csv, table_orc, table_parquet,
};
use witness::experiment::study_formats::{
    decode_pco_i64_file, decode_raw_i64_file, pco_i64_file, raw_i64_file, zstd_file,
};

#[allow(dead_code, unused_imports)]
mod generated {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/experiments/generated/documentation_example/generated.rs"
    ));
}

const DOC_DIR: &str = "docs/generated";
const ARTIFACT_DIR: &str = "docs/generated/artifacts";

#[derive(Clone, Debug)]
struct FormatRow {
    format: &'static str,
    configuration: &'static str,
    physical_unit: &'static str,
    bytes: usize,
    path: &'static str,
}

#[derive(Clone, Debug)]
struct QueryRow {
    column: &'static str,
    query: &'static str,
    answer: String,
    strategy: String,
    guarantee: OutputGuarantee,
    logical_bytes: usize,
    delivered_bytes: usize,
    transferred_bytes: usize,
    decoded_rows: usize,
    full_decode_bytes: usize,
}

#[derive(Clone, Debug)]
struct CertificateRow {
    column: &'static str,
    certificate: &'static str,
    target: i64,
    metadata_bytes: usize,
    candidate_blocks: String,
    candidate_rows: usize,
    exact_matches: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if generated::RULE_FINGERPRINT != primitive_rule_fingerprint() {
        return Err("documentation kernels are stale; run generate_documentation_example".into());
    }
    if generated::CASE_COUNT != 5 || generated::PLAN_SIGNATURES.contains(&0) {
        return Err("documentation kernel registry is invalid".into());
    }

    fs::create_dir_all(ARTIFACT_DIR)?;
    let examples = example_columns();
    let columns = examples
        .iter()
        .map(|example| encode(&example.recipe, example.input()))
        .collect::<Result<Vec<_>, _>>()?;
    verify_pages(&examples, &columns)?;
    verify_witness_roundtrip(&examples, &columns)?;

    let formats = write_and_verify_formats(&examples, &columns)?;
    let queries = run_queries(&columns)?;
    let certificates = run_certificates(&examples)?;
    let claims = read_claim_manifest("experiments/results/claim_manifest.csv")?;

    write_values_csv(&examples)?;
    write_layout_csv(&examples, &columns)?;
    write_formats_csv(&formats)?;
    write_queries_csv(&queries)?;
    write_certificates_csv(&certificates)?;
    fs::write(
        format!("{DOC_DIR}/END_TO_END_EXAMPLE.md"),
        render_markdown(
            &examples,
            &columns,
            &formats,
            &queries,
            &certificates,
            &claims,
        )?,
    )?;

    println!(
        "wrote Rust-generated documentation for {} rows, {} columns, {} query paths",
        EXAMPLE_ROWS,
        examples.len(),
        queries.len()
    );
    Ok(())
}

fn verify_pages(
    examples: &[ExampleColumn],
    columns: &[EncodedColumn],
) -> Result<(), Box<dyn std::error::Error>> {
    for (example, column) in examples.iter().zip(columns) {
        let saved = fs::read(format!("{ARTIFACT_DIR}/{}.acp", example.name))?;
        if saved != column.page.bytes() {
            return Err(format!("serialized page for {} is stale", example.name).into());
        }
    }
    Ok(())
}

fn verify_witness_roundtrip(
    examples: &[ExampleColumn],
    columns: &[EncodedColumn],
) -> Result<(), String> {
    for (case, (example, column)) in examples.iter().zip(columns).enumerate() {
        let decoded = (0..EXAMPLE_ROWS)
            .map(|row| {
                match generated::GET_FNS[case](column, row, ClosureMode::Selective)?.answer {
                    Answer::Value(value) => Ok(value),
                    _ => Err("generated GET returned a non-value answer".into()),
                }
            })
            .collect::<Result<Vec<_>, String>>()?;
        if decoded != example.values {
            return Err(format!("Witness roundtrip failed for {}", example.name));
        }
    }
    Ok(())
}

fn write_and_verify_formats(
    examples: &[ExampleColumn],
    columns: &[EncodedColumn],
) -> Result<Vec<FormatRow>, Box<dyn std::error::Error>> {
    let csv = table_csv(examples)?;
    let parquet_dictionary = table_parquet(examples, false)?;
    let parquet_delta = table_parquet(examples, true)?;
    let orc = table_orc(examples)?;
    fs::write(format!("{ARTIFACT_DIR}/example.csv"), &csv)?;
    fs::write(
        format!("{ARTIFACT_DIR}/example.dictionary-snappy.parquet"),
        &parquet_dictionary,
    )?;
    fs::write(
        format!("{ARTIFACT_DIR}/example.delta-zstd.parquet"),
        &parquet_delta,
    )?;
    fs::write(format!("{ARTIFACT_DIR}/example.orc"), &orc)?;

    let expected = examples
        .iter()
        .map(|column| column.values.clone())
        .collect::<Vec<_>>();
    if decode_csv_table(&csv, examples.len())? != expected {
        return Err("CSV table roundtrip failed".into());
    }
    if decode_parquet_table(&parquet_dictionary, examples.len())? != expected
        || decode_parquet_table(&parquet_delta, examples.len())? != expected
    {
        return Err("Parquet table roundtrip failed".into());
    }
    if decode_orc_table(&orc, examples.len())? != expected {
        return Err("ORC table roundtrip failed".into());
    }

    let mut raw_bytes = 0;
    let mut raw_zstd_bytes = 0;
    let mut pco_bytes = 0;
    let mut raw_manifest = String::from("column\tbytes\tartifact\n");
    let mut raw_zstd_manifest = String::from("column\tbytes\tartifact\n");
    let mut pco_manifest = String::from("column\tbytes\tartifact\n");
    for example in examples {
        let raw = raw_i64_file(&example.values);
        let compressed = zstd_file(&raw, 3)?;
        let pco = pco_i64_file(&example.values, 8)?;
        if decode_raw_i64_file(&raw)? != example.values
            || decode_raw_i64_file(&zstd::stream::decode_all(Cursor::new(&compressed))?)?
                != example.values
            || decode_pco_i64_file(&pco)? != example.values
        {
            return Err(format!("column-format roundtrip failed for {}", example.name).into());
        }
        raw_bytes += raw.len();
        raw_zstd_bytes += compressed.len();
        pco_bytes += pco.len();

        let raw_name = format!("{}.rawi64", example.name);
        let zstd_name = format!("{}.rawi64.zst", example.name);
        let pco_name = format!("{}.pco", example.name);
        writeln!(
            raw_manifest,
            "{}\t{}\t{}",
            example.name,
            raw.len(),
            raw_name
        )?;
        writeln!(
            raw_zstd_manifest,
            "{}\t{}\t{}",
            example.name,
            compressed.len(),
            zstd_name
        )?;
        writeln!(
            pco_manifest,
            "{}\t{}\t{}",
            example.name,
            pco.len(),
            pco_name
        )?;
        fs::write(format!("{ARTIFACT_DIR}/{raw_name}"), raw)?;
        fs::write(format!("{ARTIFACT_DIR}/{zstd_name}"), compressed)?;
        fs::write(format!("{ARTIFACT_DIR}/{pco_name}"), pco)?;
    }
    writeln!(raw_manifest, "TOTAL\t{raw_bytes}\t-")?;
    writeln!(raw_zstd_manifest, "TOTAL\t{raw_zstd_bytes}\t-")?;
    writeln!(pco_manifest, "TOTAL\t{pco_bytes}\t-")?;
    fs::write(format!("{ARTIFACT_DIR}/rawi64.manifest.tsv"), raw_manifest)?;
    fs::write(
        format!("{ARTIFACT_DIR}/rawi64-zstd.manifest.tsv"),
        raw_zstd_manifest,
    )?;
    fs::write(format!("{ARTIFACT_DIR}/pcodec.manifest.tsv"), pco_manifest)?;

    let mut witness_manifest = String::from("column\tbytes\tartifact\n");
    for (example, column) in examples.iter().zip(columns) {
        writeln!(
            witness_manifest,
            "{}\t{}\t{}.acp",
            example.name,
            column.page.bytes().len(),
            example.name
        )?;
    }
    writeln!(
        witness_manifest,
        "TOTAL\t{}\t-",
        columns
            .iter()
            .map(|column| column.page.bytes().len())
            .sum::<usize>()
    )?;
    fs::write(
        format!("{ARTIFACT_DIR}/witness.manifest.tsv"),
        witness_manifest,
    )?;

    Ok(vec![
        FormatRow {
            format: "CSV",
            configuration: "UTF-8 table",
            physical_unit: "one 5-column table",
            bytes: csv.len(),
            path: "artifacts/example.csv",
        },
        FormatRow {
            format: "Raw i64",
            configuration: "RAWI64V1 validity + dense LE64",
            physical_unit: "sum of 5 self-contained columns",
            bytes: raw_bytes,
            path: "artifacts/rawi64.manifest.tsv",
        },
        FormatRow {
            format: "Raw i64 + Zstd",
            configuration: "level 3 per column",
            physical_unit: "sum of 5 self-contained columns",
            bytes: raw_zstd_bytes,
            path: "artifacts/rawi64-zstd.manifest.tsv",
        },
        FormatRow {
            format: "PCodec",
            configuration: "level 8 + validity wrapper",
            physical_unit: "sum of 5 self-contained columns",
            bytes: pco_bytes,
            path: "artifacts/pcodec.manifest.tsv",
        },
        FormatRow {
            format: "Parquet",
            configuration: "dictionary + Snappy",
            physical_unit: "one 5-column table",
            bytes: parquet_dictionary.len(),
            path: "artifacts/example.dictionary-snappy.parquet",
        },
        FormatRow {
            format: "Parquet",
            configuration: "DELTA_BINARY_PACKED + Zstd",
            physical_unit: "one 5-column table",
            bytes: parquet_delta.len(),
            path: "artifacts/example.delta-zstd.parquet",
        },
        FormatRow {
            format: "ORC",
            configuration: "ORC-Rust RLEv2; no outer compression",
            physical_unit: "one 5-column table",
            bytes: orc.len(),
            path: "artifacts/example.orc",
        },
        FormatRow {
            format: "Witness",
            configuration: "five selected direct .acp pages",
            physical_unit: "sum of 5 self-contained columns; no table container",
            bytes: columns.iter().map(|column| column.page.bytes().len()).sum(),
            path: "artifacts/witness.manifest.tsv",
        },
    ])
}

fn run_queries(columns: &[EncodedColumn]) -> Result<Vec<QueryRow>, String> {
    let mut rows = Vec::new();
    push_query(
        &mut rows,
        &columns[0],
        "event_time",
        "FILTER BETWEEN 1700000180 AND 1700000360",
        Query::Filter {
            predicate: Predicate::Between {
                low: 1_700_000_180,
                high: 1_700_000_360,
            },
        },
        generated::FILTER_FNS[0](
            &columns[0],
            1_700_000_180,
            1_700_000_360,
            ClosureMode::Selective,
        )?,
        generated::FILTER_SCAN_FNS[0](
            &columns[0],
            1_700_000_180,
            1_700_000_360,
            ClosureMode::Selective,
        )?,
    )?;
    push_query(
        &mut rows,
        &columns[1],
        "meter",
        "SUM rows [4,12)",
        Query::Sum {
            rows: Span::new(4, 12)?,
        },
        generated::SUM_FNS[1](&columns[1], Span::new(4, 12)?, ClosureMode::Selective)?,
        generated::FUSED_SUM_FNS[1](&columns[1], Span::new(4, 12)?, ClosureMode::Selective)?,
    )?;
    push_query(
        &mut rows,
        &columns[2],
        "status",
        "FILTER status = 2",
        Query::Filter {
            predicate: Predicate::Between { low: 2, high: 2 },
        },
        generated::FILTER_FNS[2](&columns[2], 2, 2, ClosureMode::Selective)?,
        generated::FILTER_SCAN_FNS[2](&columns[2], 2, 2, ClosureMode::Selective)?,
    )?;
    for (query_name, row) in [("GET row 7", 7), ("GET row 8 (null)", 8)] {
        push_query(
            &mut rows,
            &columns[3],
            "sparse_event",
            query_name,
            Query::Get { row },
            generated::GET_FNS[3](&columns[3], row, ClosureMode::Selective)?,
            generated::FUSED_GET_FNS[3](&columns[3], row, ClosureMode::Selective)?,
        )?;
    }
    push_query(
        &mut rows,
        &columns[4],
        "reading",
        "GET patched row 9",
        Query::Get { row: 9 },
        generated::GET_FNS[4](&columns[4], 9, ClosureMode::Selective)?,
        generated::FUSED_GET_FNS[4](&columns[4], 9, ClosureMode::Selective)?,
    )?;
    Ok(rows)
}

fn push_query(
    output: &mut Vec<QueryRow>,
    column: &EncodedColumn,
    column_name: &'static str,
    query_name: &'static str,
    query: Query,
    execution: Execution,
    full: Execution,
) -> Result<(), String> {
    if execution.answer != full.answer {
        return Err(format!("{column_name} query disagrees with fused decode"));
    }
    let plan = compile(column, query)?;
    output.push(QueryRow {
        column: column_name,
        query: query_name,
        answer: answer_string(&execution.answer),
        strategy: plan_strategy(&plan),
        guarantee: plan.output,
        logical_bytes: execution.metrics.logical_bytes,
        delivered_bytes: execution.metrics.delivered_bytes,
        transferred_bytes: execution.metrics.transferred_bytes,
        decoded_rows: execution.decoded_rows,
        full_decode_bytes: full.metrics.delivered_bytes,
    });
    Ok(())
}

fn run_certificates(examples: &[ExampleColumn]) -> Result<Vec<CertificateRow>, String> {
    let status = &examples[2].values;
    let bloom = BlockBloom::build(status, 4, 8)?;
    let minmax = BlockMinMax::build(status, 4)?;
    let mut output = Vec::new();
    for target in [99, 3, 1] {
        for (certificate, plan) in [
            ("Bloom", bloom.probe_eq(target)),
            ("min/max", minmax.probe_eq(target)),
        ] {
            let exact = refine_eq(status, &plan, target)?;
            output.push(CertificateRow {
                column: "status",
                certificate,
                target,
                metadata_bytes: plan.metadata_bytes,
                candidate_blocks: join_usize(&plan.blocks),
                candidate_rows: plan.candidate_rows(status.len()),
                exact_matches: spans_string(&exact),
            });
        }
    }

    let event_time = &examples[0].values;
    let fence = SparseFence::build_equal_budget(event_time, bloom.bytes())?;
    let target = 1_700_000_420;
    let plan = fence.probe_eq(target);
    let exact = refine_eq(event_time, &plan, target)?;
    output.push(CertificateRow {
        column: "event_time",
        certificate: "sparse fence",
        target,
        metadata_bytes: plan.metadata_bytes,
        candidate_blocks: join_usize(&plan.blocks),
        candidate_rows: plan.candidate_rows(event_time.len()),
        exact_matches: spans_string(&exact),
    });
    Ok(output)
}

fn render_markdown(
    examples: &[ExampleColumn],
    columns: &[EncodedColumn],
    formats: &[FormatRow],
    queries: &[QueryRow],
    certificates: &[CertificateRow],
    claims: &BTreeMap<String, String>,
) -> Result<String, String> {
    let mut out = String::new();
    writeln!(
        out,
        "# Witness: one numerical example from values to storage to query"
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "> Generated by Rust from the current encoder, serializer, invariant calculus, compiler, and generated kernels. Do not hand-edit this file."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Regenerate with `cargo run --release --features experiment --bin generate_documentation_example` followed by `cargo run --release --features experiment --bin documentation_example`. Rule fingerprint: `{:#018x}`.",
        primitive_rule_fingerprint()
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "## What this example does and does not prove").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "The 16-row table below is a mechanics example. Every byte count, field offset, answer, and access counter comes from an actual artifact. It is deliberately too small for compression-ratio or latency claims because fixed headers dominate. The empirical claims come later from the canonical 109-column census and 40-pair query study."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "The comparison has two tiers. CSV, raw i64, Zstd, PCodec, Parquet, ORC, and Witness are executable artifacts generated here. FastLanes, BtrBlocks, LeCo, White-box Compression, and Vortex are literature comparators only; this repository does not serialize their formats, so it does not invent byte layouts or timings for them."
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "## 1. The logical table").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "| row | event_time | meter | status | sparse_event | reading |"
    )
    .unwrap();
    writeln!(out, "|---:|---:|---:|---:|---:|---:|").unwrap();
    for row in 0..EXAMPLE_ROWS {
        write!(out, "| {row} ").unwrap();
        for column in examples {
            write!(
                out,
                "| {} ",
                column.values[row].map_or_else(|| "NULL".into(), |value| value.to_string())
            )
            .unwrap();
        }
        writeln!(out, "|").unwrap();
    }
    writeln!(out).unwrap();
    writeln!(
        out,
        "`event_time` is globally non-decreasing. `meter` falls at rows 8 and 14. `status` has a small sorted dictionary and runs. `sparse_event` needs validity and rank data. `reading[9]=10000` is stored as a checked patch over a regular FOR body."
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "## 2. Actual physical artifacts").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "| format | configuration | physical unit | bytes | verified artifact or manifest |"
    )
    .unwrap();
    writeln!(out, "|---|---|---|---:|---|").unwrap();
    for row in formats {
        writeln!(
            out,
            "| {} | {} | {} | {} | [{}]({}) |",
            row.format, row.configuration, row.physical_unit, row.bytes, row.path, row.path
        )
        .unwrap();
    }
    writeln!(out).unwrap();
    writeln!(
        out,
        "These rows are not a ratio ranking: the units are printed because a five-column table file and five independently self-contained column files pay different fixed metadata. Aggregate rows link to generated manifests that enumerate all five retained files; manifest bytes are not counted. Every data artifact round-trips to the table above before this document is written."
    )
    .unwrap();
    writeln!(out).unwrap();
    for (label, path) in [
        ("CSV", "example.csv"),
        ("PCodec wrapper", "event_time.pco"),
        ("Parquet delta/Zstd", "example.delta-zstd.parquet"),
        ("ORC", "example.orc"),
        ("Witness event page", "event_time.acp"),
    ] {
        let bytes =
            fs::read(format!("{ARTIFACT_DIR}/{path}")).map_err(|error| error.to_string())?;
        writeln!(
            out,
            "- **{label}:** `{}` bytes begin `{}`.",
            bytes.len(),
            hex_preview(&bytes, 24)
        )
        .unwrap();
    }
    writeln!(out).unwrap();

    writeln!(out, "## 3. Why Witness selected five different pages").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "| column | role | selected decoder composition | page bytes | checked facts |"
    )
    .unwrap();
    writeln!(out, "|---|---|---|---:|---|").unwrap();
    for (example, column) in examples.iter().zip(columns) {
        let invariants = column.page.invariants();
        writeln!(
            out,
            "| `{}` | {} | `{}` | {} | non-decreasing={}, non-decreasing non-null={}, null placement={:?} |",
            example.name,
            example.role,
            example.recipe.name(),
            column.page.bytes().len(),
            invariants.non_decreasing,
            invariants.non_decreasing_non_null,
            invariants.null_placement,
        )
        .unwrap();
    }
    writeln!(out).unwrap();
    writeln!(
        out,
        "Selection here is explicit, so each mechanism is visible. The real study separately compares a size-selected menu with an access-ready menu; this toy table is not evidence that these five recipes are optimal."
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "## 4. Exact page directories and stored fields").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Every page begins with `ACPAGE01`, format version 3, an authenticated descriptor, field/frame directories, and dependency rules. Direct fields have physical offsets. A framed field instead names one Zstd frame; touching any byte then delivers the complete compressed frame."
    )
    .unwrap();
    writeln!(out).unwrap();
    for (example, column) in examples.iter().zip(columns) {
        writeln!(out, "### `{}`: `{}`", example.name, example.recipe.name()).unwrap();
        writeln!(out).unwrap();
        writeln!(out, "| id | field | logical bytes | physical location | read granularity | exact content/preview |").unwrap();
        writeln!(out, "|---:|---|---:|---|---:|---|").unwrap();
        for field in &column.page.layout().fields {
            writeln!(
                out,
                "| {} | `{}` | {} | {} | {} | {} |",
                field.id.0,
                field.name,
                field.length,
                location_string(field, column),
                field.read_granularity,
                describe_field(field, column),
            )
            .unwrap();
        }
        writeln!(out).unwrap();
        writeln!(out, "Decoder IR:").unwrap();
        writeln!(out).unwrap();
        for (id, node) in column.decoder.nodes().iter().enumerate() {
            writeln!(out, "- node {id}: `{node:?}`").unwrap();
        }
        writeln!(out).unwrap();
    }

    writeln!(out, "## 5. Physical access closure").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "A query first asks for logical fields. Layout dependencies add metadata, indexes, restart anchors, and enclosing frames until a fixed point is reached. Three counters remain separate:"
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "- **logical bytes:** bytes in fields the operator requested;"
    )
    .unwrap();
    writeln!(
        out,
        "- **delivered bytes:** bytes forced by field granularity or a compressed frame;"
    )
    .unwrap();
    writeln!(
        out,
        "- **transferred bytes:** new bytes moved after overlap and cache reuse."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "For each operation below, the generated result was checked against a fused full-stream decode.").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "| column/query | generated answer | authorized strategy | guarantee | logical B | delivered B | transferred B | values decoded | fused full-stream delivered B |").unwrap();
    writeln!(out, "|---|---|---|---|---:|---:|---:|---:|---:|").unwrap();
    for row in queries {
        writeln!(
            out,
            "| `{}`: {} | `{}` | {} | `{:?}` | {} | {} | {} | {} | {} |",
            row.column,
            row.query,
            row.answer,
            row.strategy,
            row.guarantee,
            row.logical_bytes,
            row.delivered_bytes,
            row.transferred_bytes,
            row.decoded_rows,
            row.full_decode_bytes,
        )
        .unwrap();
    }
    writeln!(out).unwrap();
    writeln!(
        out,
        "The `meter` SUM is exact but not metadata-only: the kernel visits the selected encoded rows and accumulates without materializing an output array. The monotone timestamp filter is different: it performs exact boundary searches. The dictionary filter translates value bounds to ID bounds, then scans encoded IDs. Patch and nullable GETs first read their position/rank prerequisites. Unsupported structure converges to fused decoding; it is not reported as an encoded-domain win."
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "### Compiler plans, including dynamic closures").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "Each node closure below is a self-contained dependency closure and may overlap earlier nodes; it is not an additive I/O ledger. The execution table above reports deduplicated totals.").unwrap();
    writeln!(out).unwrap();
    for row in queries {
        let (case, query) = query_for_row(row)?;
        let plan = compile(&columns[case], query)?;
        writeln!(out, "**`{}` / {}**", row.column, row.query).unwrap();
        writeln!(out).unwrap();
        for node in plan.nodes {
            let closure = match node.byte_closure {
                ClosureSpec::Exact(closure) => format!(
                    "exact: logical={} B, delivered={} B",
                    closure.logical_bytes, closure.delivered_bytes
                ),
                ClosureSpec::RuntimeRefined {
                    possible_fields,
                    reason,
                    ..
                } => format!(
                    "runtime-refined over fields [{}]: {}",
                    possible_fields
                        .iter()
                        .map(|field| field.0.to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                    reason
                ),
            };
            writeln!(
                out,
                "- `{:?}` on rows `[{}, {})`; {}; output `{:?}`.",
                node.op, node.rows.0.start, node.rows.0.end, closure, node.guarantee
            )
            .unwrap();
        }
        writeln!(out).unwrap();
    }

    writeln!(out, "## 6. Bloom filters, min/max, and sparse fences").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "These are control certificates, not decoder-tree facts. They return candidate blocks and require refinement unless the candidate set is empty. The toy controls use four-row blocks so the result is visible."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "| column | certificate | target | metadata B | candidate blocks | candidate rows | exact matches after refinement |").unwrap();
    writeln!(out, "|---|---|---:|---:|---|---:|---|").unwrap();
    for row in certificates {
        writeln!(
            out,
            "| `{}` | {} | {} | {} | `{}` | {} | `{}` |",
            row.column,
            row.certificate,
            row.target,
            row.metadata_bytes,
            row.candidate_blocks,
            row.candidate_rows,
            row.exact_matches,
        )
        .unwrap();
    }
    writeln!(out).unwrap();
    writeln!(
        out,
        "An absent Bloom result is an exact empty answer. A positive Bloom result is only a candidate bitmap because false positives are possible. Min/max can reject a block whose range excludes the target. A sparse fence applies only to sorted data and narrows the search interval. None of these sidecars is silently credited to the Witness page layout."
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "## 7. How the executable formats differ").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "| format | encode/store | GET | SUM/range | predicate path | unavoidable boundary |"
    )
    .unwrap();
    writeln!(out, "|---|---|---|---|---|---|").unwrap();
    writeln!(out, "| CSV | decimal text rows | parse preceding text or index externally | parse selected/all text | parse and compare | no intrinsic column/page index |").unwrap();
    writeln!(out, "| Raw i64 | validity + dense LE64 | direct offset after validity rank | read and add values | compare values | no compression; nullable rank still needed |").unwrap();
    writeln!(out, "| Raw + Zstd | raw artifact in entropy frame | decompress frame | decompress then aggregate | decompress then compare | enclosing frame |").unwrap();
    writeln!(out, "| PCodec | validity + PCO compressed payload | chunk decode | chunk decode and aggregate | decode/refine | PCO chunk granularity |").unwrap();
    writeln!(out, "| Parquet | pages, levels, encodings, compression, footer | page-reader path | decode selected pages | statistics/dictionary/page decode as supported | row group/page/compression frame |").unwrap();
    writeln!(out, "| ORC | stripes, streams, RLEv2, indexes | stream/stripe path | decode selected streams | row-index/Bloom support depends on writer and reader | stripe/stream granularity |").unwrap();
    writeln!(out, "| Witness | composed decoder + authenticated layout + checked facts | dependency-closed selective read | fused encoded traversal; no materialization | derived search when proved, otherwise scan | field granularity, restarts, indexes, or frame |").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "## 8. Literature-only systems: capability scope").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "| system | documented focus | what this repository does not claim |"
    )
    .unwrap();
    writeln!(out, "|---|---|---|").unwrap();
    writeln!(out, "| FastLanes | very fast decoding and a composable file layout | no locally generated FastLanes artifact or timing |").unwrap();
    writeln!(out, "| BtrBlocks | efficient lightweight columnar compression | no locally generated BtrBlocks artifact or timing |").unwrap();
    writeln!(out, "| LeCo | learned serial correlation with random access | no locally generated LeCo artifact or timing |").unwrap();
    writeln!(out, "| White-box Compression | learned table expressions and execution opportunities | no locally generated White-box artifact or timing |").unwrap();
    writeln!(out, "| Vortex | extensible nested encodings and compute | no locally generated Vortex artifact or timing |").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "The research distinction under test is not that Witness has uniquely invented compressed execution. It is that checked facts, physical dependencies, and output guarantees are derived compositionally from a decoder/layout description, then used to choose a sound query path. See `docs/ENCODINGS.md` for measured and literature-only comparison scope."
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "## 9. Current measured study, not the toy example").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "| evidence | current generated result | interpretation |"
    )
    .unwrap();
    writeln!(out, "|---|---|---|").unwrap();
    writeln!(
        out,
        "| invariant census | {} columns, {} independent sources, {} million rows; {}% globally monotone | useful order evidence exists, but is not universal |",
        claim_value(claims, "WitCensusColumns")?,
        claim_value(claims, "WitCensusSources")?,
        claim_value(claims, "WitCensusRowsMillions")?,
        claim_value(claims, "WitCensusMonotoneColumns")?,
    )
    .unwrap();
    writeln!(
        out,
        "| access-ready predicate path vs Parquet | source median {}x; 95% bootstrap CI [{}, {}] | point estimate favors Witness, interval crosses parity |",
        claim_value(claims, "WitSourceDirectBoundaryMedian")?,
        claim_value(claims, "WitSourceDirectBoundaryCiLow")?,
        claim_value(claims, "WitSourceDirectBoundaryCiHigh")?,
    )
    .unwrap();
    writeln!(
        out,
        "| access-ready vs size-selected Witness | source median {}x; byte premium {}x | lower delivered access is purchased with larger files |",
        claim_value(claims, "WitSourceDirectStorageMedian")?,
        claim_value(claims, "WitAccessPremium")?,
    )
    .unwrap();
    writeln!(
        out,
        "| Bloom absent / rare / frequent | candidate fractions {} / {} / {}; modeled-byte ratios {} / {} / {} | membership helps absent and rare probes, then converges to no pruning |",
        claim_value(claims, "WitBloomAbsentCandidate")?,
        claim_value(claims, "WitBloomRareCandidate")?,
        claim_value(claims, "WitBloomFrequentCandidate")?,
        claim_value(claims, "WitBloomAbsentModeledBytes")?,
        claim_value(claims, "WitBloomRareModeledBytes")?,
        claim_value(claims, "WitBloomFrequentModeledBytes")?,
    )
    .unwrap();
    writeln!(
        out,
        "| cold XFS schedule | {} MiB required in a {} MiB file; {} page reads vs {} coalesced vs 1 full read; latency ratios {}x / {}x / 1x | byte-minimal page reads lose to call overhead; coalescing recovers sequential behavior |",
        claim_value(claims, "WitColdRequiredMb")?,
        claim_value(claims, "WitColdFileMb")?,
        claim_value(claims, "WitColdPageCalls")?,
        claim_value(claims, "WitColdCoalescedCalls")?,
        claim_value(claims, "WitColdPageOverFull")?,
        claim_value(claims, "WitColdCoalescedOverFull")?,
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Canonical evidence lives in `experiments/results/invariant_census/`, `predicate_pipeline/`, `predicate_pipeline_rows16k/`, `additional_plans.csv`, `certificate_study/`, and `real_access/`. `./reproduce.sh` rebuilds those files, the claim manifest, and this walkthrough."
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "## 10. Failure and fallback checklist").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "- A checksum or descriptor mismatch rejects the page.").unwrap();
    writeln!(
        out,
        "- A signed delta stream does not certify monotonicity."
    )
    .unwrap();
    writeln!(
        out,
        "- Arbitrary null placement blocks exact monotone boundary search."
    )
    .unwrap();
    writeln!(
        out,
        "- A dictionary must be sorted and unique before ID order can stand for value order."
    )
    .unwrap();
    writeln!(
        out,
        "- RLE, patch, and nullable reads include run, position, validity, and rank prerequisites."
    )
    .unwrap();
    writeln!(
        out,
        "- Touching a field inside a Zstd frame delivers the whole frame."
    )
    .unwrap();
    writeln!(
        out,
        "- Bloom positives and min/max overlaps are candidates, not answers."
    )
    .unwrap();
    writeln!(out, "- SUM is metadata-only only when an exact aggregate certificate is present; this prototype otherwise performs fused encoded traversal.").unwrap();
    writeln!(
        out,
        "- When no fact authorizes a narrower algorithm, the compiler chooses scan/decode behavior."
    )
    .unwrap();

    Ok(out)
}

fn write_values_csv(examples: &[ExampleColumn]) -> Result<(), Box<dyn std::error::Error>> {
    let mut writer = csv::Writer::from_path(format!("{DOC_DIR}/example_values.csv"))?;
    let mut header = vec!["row"];
    header.extend(examples.iter().map(|column| column.name));
    writer.write_record(header)?;
    for row in 0..EXAMPLE_ROWS {
        let mut record = vec![row.to_string()];
        record.extend(
            examples.iter().map(|column| {
                column.values[row].map_or_else(String::new, |value| value.to_string())
            }),
        );
        writer.write_record(record)?;
    }
    writer.flush()?;
    Ok(())
}

fn write_layout_csv(
    examples: &[ExampleColumn],
    columns: &[EncodedColumn],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut writer = csv::Writer::from_path(format!("{DOC_DIR}/witness_layout.csv"))?;
    writer.write_record([
        "column",
        "recipe",
        "page_bytes",
        "field_id",
        "field",
        "logical_bytes",
        "location",
        "read_granularity",
        "content",
    ])?;
    for (example, column) in examples.iter().zip(columns) {
        for field in &column.page.layout().fields {
            writer.write_record([
                example.name.to_string(),
                example.recipe.name(),
                column.page.bytes().len().to_string(),
                field.id.0.to_string(),
                field.name.clone(),
                field.length.to_string(),
                location_string(field, column),
                field.read_granularity.to_string(),
                describe_field(field, column),
            ])?;
        }
    }
    writer.flush()?;
    Ok(())
}

fn write_formats_csv(rows: &[FormatRow]) -> Result<(), Box<dyn std::error::Error>> {
    let mut writer = csv::Writer::from_path(format!("{DOC_DIR}/format_artifacts.csv"))?;
    writer.write_record([
        "format",
        "configuration",
        "physical_unit",
        "bytes",
        "artifact",
        "performance_claim",
    ])?;
    for row in rows {
        writer.write_record([
            row.format,
            row.configuration,
            row.physical_unit,
            &row.bytes.to_string(),
            row.path,
            "false",
        ])?;
    }
    writer.flush()?;
    Ok(())
}

fn write_queries_csv(rows: &[QueryRow]) -> Result<(), Box<dyn std::error::Error>> {
    let mut writer = csv::Writer::from_path(format!("{DOC_DIR}/query_paths.csv"))?;
    writer.write_record([
        "column",
        "query",
        "answer",
        "strategy",
        "guarantee",
        "logical_bytes",
        "delivered_bytes",
        "transferred_bytes",
        "decoded_rows",
        "fused_full_stream_delivered_bytes",
    ])?;
    for row in rows {
        writer.write_record([
            row.column.to_string(),
            row.query.to_string(),
            row.answer.clone(),
            row.strategy.clone(),
            format!("{:?}", row.guarantee),
            row.logical_bytes.to_string(),
            row.delivered_bytes.to_string(),
            row.transferred_bytes.to_string(),
            row.decoded_rows.to_string(),
            row.full_decode_bytes.to_string(),
        ])?;
    }
    writer.flush()?;
    Ok(())
}

fn write_certificates_csv(rows: &[CertificateRow]) -> Result<(), Box<dyn std::error::Error>> {
    let mut writer = csv::Writer::from_path(format!("{DOC_DIR}/certificate_example.csv"))?;
    writer.write_record([
        "column",
        "certificate",
        "target",
        "metadata_bytes",
        "candidate_blocks",
        "candidate_rows",
        "exact_matches_after_refinement",
        "probe_guarantee",
    ])?;
    for row in rows {
        writer.write_record([
            row.column.to_string(),
            row.certificate.to_string(),
            row.target.to_string(),
            row.metadata_bytes.to_string(),
            row.candidate_blocks.clone(),
            row.candidate_rows.to_string(),
            row.exact_matches.clone(),
            "CandidateBitmap".to_string(),
        ])?;
    }
    writer.flush()?;
    Ok(())
}

fn decode_csv_table(bytes: &[u8], columns: usize) -> Result<Vec<Vec<Option<i64>>>, String> {
    let mut output = vec![Vec::new(); columns];
    let mut reader = csv::Reader::from_reader(bytes);
    for record in reader.records() {
        let record = record.map_err(|error| error.to_string())?;
        if record.len() != columns {
            return Err("CSV example has the wrong column count".into());
        }
        for (column, value) in record.iter().enumerate() {
            output[column].push(if value.is_empty() {
                None
            } else {
                Some(value.parse::<i64>().map_err(|error| error.to_string())?)
            });
        }
    }
    Ok(output)
}

fn decode_parquet_table(bytes: &[u8], columns: usize) -> Result<Vec<Vec<Option<i64>>>, String> {
    let reader = ParquetRecordBatchReaderBuilder::try_new(Bytes::copy_from_slice(bytes))
        .map_err(|error| error.to_string())?
        .build()
        .map_err(|error| error.to_string())?;
    decode_batches(reader, columns)
}

fn decode_orc_table(bytes: &[u8], columns: usize) -> Result<Vec<Vec<Option<i64>>>, String> {
    let reader = OrcReaderBuilder::try_new(Bytes::copy_from_slice(bytes))
        .map_err(|error| error.to_string())?
        .build();
    decode_batches(reader, columns)
}

fn decode_batches<I, E>(batches: I, columns: usize) -> Result<Vec<Vec<Option<i64>>>, String>
where
    I: IntoIterator<Item = Result<arrow_array::RecordBatch, E>>,
    E: std::fmt::Display,
{
    let mut output = vec![Vec::new(); columns];
    for batch in batches {
        let batch = batch.map_err(|error| error.to_string())?;
        if batch.num_columns() != columns {
            return Err("physical table has the wrong column count".into());
        }
        for (column, output_column) in output.iter_mut().enumerate() {
            let array = batch
                .column(column)
                .as_any()
                .downcast_ref::<Int64Array>()
                .ok_or("physical table column is not Int64")?;
            output_column.extend(
                (0..array.len()).map(|row| (!array.is_null(row)).then(|| array.value(row))),
            );
        }
    }
    Ok(output)
}

fn read_claim_manifest(path: &str) -> Result<BTreeMap<String, String>, String> {
    let mut reader = csv::Reader::from_path(path).map_err(|error| error.to_string())?;
    let mut output = BTreeMap::new();
    for row in reader.records() {
        let row = row.map_err(|error| error.to_string())?;
        let name = row
            .get(0)
            .ok_or_else(|| "claim name is absent".to_string())?;
        let value = row
            .get(1)
            .ok_or_else(|| format!("claim value is absent for {name}"))?;
        output.insert(name.to_string(), value.to_string());
    }
    Ok(output)
}

fn claim_value<'a>(claims: &'a BTreeMap<String, String>, name: &str) -> Result<&'a str, String> {
    claims
        .get(name)
        .map(String::as_str)
        .ok_or_else(|| format!("claim {name} is absent"))
}

fn query_for_row(row: &QueryRow) -> Result<(usize, Query), String> {
    match row.query {
        "FILTER BETWEEN 1700000180 AND 1700000360" => Ok((
            0,
            Query::Filter {
                predicate: Predicate::Between {
                    low: 1_700_000_180,
                    high: 1_700_000_360,
                },
            },
        )),
        "SUM rows [4,12)" => Ok((
            1,
            Query::Sum {
                rows: Span::new(4, 12)?,
            },
        )),
        "FILTER status = 2" => Ok((
            2,
            Query::Filter {
                predicate: Predicate::Between { low: 2, high: 2 },
            },
        )),
        "GET row 7" => Ok((3, Query::Get { row: 7 })),
        "GET row 8 (null)" => Ok((3, Query::Get { row: 8 })),
        "GET patched row 9" => Ok((4, Query::Get { row: 9 })),
        _ => Err("unknown documentation query".into()),
    }
}

fn plan_strategy(plan: &witness::access_compiler::PlanIr) -> String {
    plan.nodes
        .iter()
        .filter_map(|node| match &node.op {
            witness::access_compiler::PlanOp::LoadMetadata { .. } => None,
            operation => Some(format!("{operation:?}")),
        })
        .collect::<Vec<_>>()
        .join(" -> ")
}

fn answer_string(answer: &Answer) -> String {
    match answer {
        Answer::Value(Some(value)) => value.to_string(),
        Answer::Value(None) => "NULL".into(),
        Answer::Sum(value) => value.to_string(),
        Answer::Ranges(ranges) => spans_string(ranges),
        Answer::Count(count) => count.to_string(),
    }
}

fn spans_string(ranges: &[Span]) -> String {
    if ranges.is_empty() {
        return "empty".into();
    }
    ranges
        .iter()
        .map(|range| format!("[{},{})", range.start, range.end))
        .collect::<Vec<_>>()
        .join("|")
}

fn join_usize(values: &[usize]) -> String {
    if values.is_empty() {
        "empty".into()
    } else {
        values
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join("|")
    }
}

fn location_string(field: &FieldLayout, column: &EncodedColumn) -> String {
    match field.location {
        FieldLocation::Direct { offset } => format!("[{offset},{}) direct", offset + field.length),
        FieldLocation::Framed {
            frame,
            decoded_offset,
        } => {
            let frame = &column.page.layout().frames[frame.0];
            format!(
                "frame {} physical [{},{}), decoded [{},{})",
                frame.id.0,
                frame.offset,
                frame.offset + frame.compressed_length,
                decoded_offset,
                decoded_offset + field.length
            )
        }
    }
}

fn describe_field(field: &FieldLayout, column: &EncodedColumn) -> String {
    if field.id == column.page.layout().metadata {
        let bytes = column.page.bytes();
        return format!(
            "magic=ACPAGE01, version={}, fields={}, frames={}, dependencies={}, file_length={}",
            read_u32(bytes, 8),
            read_u32(bytes, 12),
            read_u32(bytes, 16),
            read_u32(bytes, 20),
            read_u64(bytes, 24)
        );
    }
    let Some(bytes) = direct_field_bytes(field, column) else {
        return "inside compressed frame; see decoded offset".into();
    };
    match field.name.as_str() {
        "for.base" => format!("base={}", read_i64(bytes, 0)),
        "delta.restarts" | "dictionary.values" | "patch.exceptions" => {
            format!("i64=[{}]", i64_values(bytes))
        }
        "rle.lengths" | "patch.positions" | "nullable.rank" => {
            format!("u32=[{}]", u32_values(bytes))
        }
        "rle.index" | "patch.index" => format!("u32 pairs=[{}]", u32_pairs(bytes)),
        "nullable.validity" => format!("bitmap=0x{}", hex_preview(bytes, bytes.len())),
        "bitpack.miniblocks" => {
            let detail = column
                .decoder
                .nodes()
                .iter()
                .find_map(|node| match node {
                    witness::access_compiler::DecoderNode::BitUnpack {
                        stream,
                        width,
                        len,
                        miniblock_rows,
                        miniblock_bytes,
                    } if *stream == field.id => Some(format!(
                        "width={width}, values={len}, block_rows={miniblock_rows}, block_bytes={miniblock_bytes}"
                    )),
                    _ => None,
                })
                .unwrap_or_else(|| "packed stream".into());
            format!("{detail}, hex={}", hex_preview(bytes, 24))
        }
        _ => format!("hex={}", hex_preview(bytes, 24)),
    }
}

fn direct_field_bytes<'a>(field: &FieldLayout, column: &'a EncodedColumn) -> Option<&'a [u8]> {
    match field.location {
        FieldLocation::Direct { offset } => column.page.bytes().get(offset..offset + field.length),
        FieldLocation::Framed { .. } => None,
    }
}

fn i64_values(bytes: &[u8]) -> String {
    bytes
        .chunks_exact(8)
        .map(|chunk| i64::from_le_bytes(chunk.try_into().unwrap()).to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn u32_values(bytes: &[u8]) -> String {
    bytes
        .chunks_exact(4)
        .map(|chunk| u32::from_le_bytes(chunk.try_into().unwrap()).to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn u32_pairs(bytes: &[u8]) -> String {
    bytes
        .chunks_exact(8)
        .map(|chunk| {
            format!(
                "({}, {})",
                u32::from_le_bytes(chunk[..4].try_into().unwrap()),
                u32::from_le_bytes(chunk[4..].try_into().unwrap())
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn hex_preview(bytes: &[u8], limit: usize) -> String {
    let mut output = bytes
        .iter()
        .take(limit)
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    if bytes.len() > limit {
        output.push_str(" ...");
    }
    output
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn read_i64(bytes: &[u8], offset: usize) -> i64 {
    i64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}
