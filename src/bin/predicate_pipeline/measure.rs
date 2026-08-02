use std::fs::{self, File};
use std::hint::black_box;
use std::io::{BufWriter, Write};
use std::time::Instant;

use bytes::Bytes;
use witness::access_compiler::{AccessMetrics, primitive_rule_fingerprint};
use witness::experiment::access_real::predicate_access_corpus;
use witness::experiment::time_window::{
    ParquetAggregate, parquet_boundary_sum, parquet_boundary_sum_counted, parquet_full_sum,
    parquet_full_sum_counted, parquet_indexed_sum, parquet_indexed_sum_counted, parquet_late_sum,
    parquet_late_sum_counted, parquet_oracle_sum, parquet_oracle_sum_counted, parquet_pair_file,
};

use super::engine::{
    FilterPlan, PipelinePlans, PipelineResult, ValuePlan, complete_query, complete_query_untracked,
    known_selection, known_selection_untracked,
};
use super::workload::{predicates, selected_rows, truth};
use crate::generated;

const DEFAULT_RESULT_DIR: &str = "experiments/results/predicate_pipeline";
const REPEATS: usize = 5;
const TARGET_NS: u128 = 2_000_000;
const PARQUET_PAGE_ROWS: [usize; 3] = [1_024, 4_096, 16_384];

#[derive(Clone, Debug)]
struct Outcome {
    sum: i128,
    matched_rows: usize,
    selector: AccessMetrics,
    value: AccessMetrics,
    selector_decoded_rows: usize,
    value_decoded_rows: usize,
    selector_primitive_values_read: usize,
    value_primitive_values_read: usize,
    page_index_entries_examined: usize,
    candidate_pages: usize,
    boundary_order_used: bool,
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    if generated::RULE_FINGERPRINT != primitive_rule_fingerprint() {
        return Err("predicate pipeline generated source is stale".into());
    }
    let result_dir =
        std::env::var("WITNESS_PREDICATE_RESULT_DIR").unwrap_or_else(|_| DEFAULT_RESULT_DIR.into());
    let parquet_dir = format!("{result_dir}/parquet");
    if std::path::Path::new(&parquet_dir).exists() {
        fs::remove_dir_all(&parquet_dir)?;
    }
    fs::create_dir_all(&parquet_dir)?;
    let (columns, pairs) = predicate_access_corpus()?;
    if generated::CASE_COUNT != columns.len() * 2 || pairs.len() < 30 {
        return Err("predicate pipeline corpus does not match generated kernels".into());
    }
    let mut pair_csv = csv(format!("{result_dir}/pairs.csv"))?;
    let mut predicate_csv = csv(format!("{result_dir}/predicates.csv"))?;
    let mut known_csv = csv(format!("{result_dir}/known_selection.csv"))?;
    let mut complete_csv = csv(format!("{result_dir}/complete_query.csv"))?;
    writeln!(
        pair_csv,
        "pair,class,selector_kind,group,source,selector_column,selector_name,selector_recipe,selector_non_decreasing,value_column,value_name,value_recipe,rows,gen_bytes,direct_gen_bytes,parquet_bytes_p1024,parquet_bytes_p4096,parquet_bytes_p16384"
    )?;
    writeln!(
        predicate_csv,
        "pair,predicate,requested_selectivities,low,high,selected_rows,actual_selectivity,ranges"
    )?;
    write_result_header(&mut known_csv)?;
    write_result_header(&mut complete_csv)?;

    for (pair_id, pair) in pairs.iter().enumerate() {
        let selector = &columns[pair.selector];
        let value = &columns[pair.value];
        let mut parquet_files = Vec::new();
        for page_rows in PARQUET_PAGE_ROWS {
            let parquet = parquet_pair_file(
                &selector.size_selected.truth,
                &value.size_selected.truth,
                page_rows,
            )?;
            fs::write(
                format!("{result_dir}/parquet/pair_{pair_id:02}_p{page_rows}.parquet"),
                &parquet,
            )?;
            parquet_files.push(Bytes::from(parquet));
        }
        let gen_bytes =
            selector.size_selected.page.bytes().len() + value.size_selected.page.bytes().len();
        let direct_gen_bytes =
            selector.access_ready.page.bytes().len() + value.access_ready.page.bytes().len();
        writeln!(
            pair_csv,
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            pair_id,
            quote(&pair.class),
            quote(&pair.selector_kind),
            quote(&pair.group),
            quote(&pair.source),
            pair.selector,
            quote(&selector.name),
            quote(&selector.size_selected.recipe.name()),
            selector.size_selected.page.invariants().non_decreasing,
            pair.value,
            quote(&value.name),
            quote(&value.size_selected.recipe.name()),
            selector.size_selected.truth.len(),
            gen_bytes,
            direct_gen_bytes,
            parquet_files[0].len(),
            parquet_files[1].len(),
            parquet_files[2].len(),
        )?;
        let parquet = parquet_files[0].clone();
        for predicate in predicates(&selector.size_selected.truth)? {
            let (ranges, expected_sum) = truth(
                &selector.size_selected.truth,
                &value.size_selected.truth,
                &predicate,
            )?;
            let matched = selected_rows(&ranges);
            writeln!(
                predicate_csv,
                "{},{},{},{},{},{},{:.8},{}",
                pair_id,
                predicate.label,
                predicate.requested,
                predicate.low,
                predicate.high,
                matched,
                matched as f64 / selector.size_selected.truth.len() as f64,
                ranges.len(),
            )?;

            for plan in [ValuePlan::Selective, ValuePlan::Fused] {
                let result = known_selection(pair.value, &value.size_selected, &ranges, plan)?;
                let (timing, _) = benchmark(|| {
                    known_selection_untracked(pair.value, &value.size_selected, &ranges, plan)
                })?;
                let outcome = gen_outcome(result, matched);
                verify(&outcome, expected_sum, matched, "known Gen")?;
                write_result(
                    &mut known_csv,
                    pair_id,
                    &predicate.label,
                    plan.name(),
                    timing,
                    &outcome,
                    value.size_selected.page.bytes().len(),
                )?;
            }
            let direct_value_case = columns.len() + pair.value;
            let result = known_selection(
                direct_value_case,
                &value.access_ready,
                &ranges,
                ValuePlan::Selective,
            )?;
            let (timing, _) = benchmark(|| {
                known_selection_untracked(
                    direct_value_case,
                    &value.access_ready,
                    &ranges,
                    ValuePlan::Selective,
                )
            })?;
            let outcome = gen_outcome(result, matched);
            verify(&outcome, expected_sum, matched, "known direct Gen")?;
            write_result(
                &mut known_csv,
                pair_id,
                &predicate.label,
                "generated_direct_selective",
                timing,
                &outcome,
                value.access_ready.page.bytes().len(),
            )?;
            let parquet_ranges = ranges
                .iter()
                .map(|range| (range.start, range.end))
                .collect::<Vec<_>>();
            let counted = parquet_oracle_sum_counted(
                parquet.clone(),
                &parquet_ranges,
                selector.size_selected.truth.len(),
            )?;
            let (timing, _) = benchmark(|| {
                parquet_oracle_sum(
                    parquet.clone(),
                    &parquet_ranges,
                    selector.size_selected.truth.len(),
                )
            })?;
            let outcome = parquet_outcome(counted);
            verify(&outcome, expected_sum, matched, "Parquet row selection")?;
            write_result(
                &mut known_csv,
                pair_id,
                &predicate.label,
                "parquet_row_selection",
                timing,
                &outcome,
                parquet.len(),
            )?;

            for plan in [ValuePlan::Selective, ValuePlan::Fused] {
                let plans = PipelinePlans {
                    value: plan,
                    filter: FilterPlan::Compiled,
                };
                let result = complete_query(
                    pair.selector,
                    &selector.size_selected,
                    pair.value,
                    &value.size_selected,
                    predicate.low,
                    predicate.high,
                    plans,
                )?;
                let (timing, _) = benchmark(|| {
                    complete_query_untracked(
                        pair.selector,
                        &selector.size_selected,
                        pair.value,
                        &value.size_selected,
                        predicate.low,
                        predicate.high,
                        plans,
                    )
                })?;
                if result.ranges != ranges {
                    return Err(
                        format!("generated filter ranges disagree on pair {pair_id}").into(),
                    );
                }
                let outcome = gen_outcome(result, matched);
                verify(&outcome, expected_sum, matched, "complete Gen")?;
                write_result(
                    &mut complete_csv,
                    pair_id,
                    &predicate.label,
                    plan.name(),
                    timing,
                    &outcome,
                    gen_bytes,
                )?;
            }
            let direct_plans = PipelinePlans {
                value: ValuePlan::Selective,
                filter: FilterPlan::Compiled,
            };
            let result = complete_query(
                columns.len() + pair.selector,
                &selector.access_ready,
                columns.len() + pair.value,
                &value.access_ready,
                predicate.low,
                predicate.high,
                direct_plans,
            )?;
            let (timing, _) = benchmark(|| {
                complete_query_untracked(
                    columns.len() + pair.selector,
                    &selector.access_ready,
                    columns.len() + pair.value,
                    &value.access_ready,
                    predicate.low,
                    predicate.high,
                    direct_plans,
                )
            })?;
            if result.ranges != ranges {
                return Err(
                    format!("direct generated filter ranges disagree on pair {pair_id}").into(),
                );
            }
            let outcome = gen_outcome(result, matched);
            verify(&outcome, expected_sum, matched, "complete direct Gen")?;
            write_result(
                &mut complete_csv,
                pair_id,
                &predicate.label,
                "generated_direct_selective",
                timing,
                &outcome,
                direct_gen_bytes,
            )?;
            let scan_plans = PipelinePlans {
                value: ValuePlan::Selective,
                filter: FilterPlan::FullScan,
            };
            let result = complete_query(
                pair.selector,
                &selector.size_selected,
                pair.value,
                &value.size_selected,
                predicate.low,
                predicate.high,
                scan_plans,
            )?;
            let (timing, _) = benchmark(|| {
                complete_query_untracked(
                    pair.selector,
                    &selector.size_selected,
                    pair.value,
                    &value.size_selected,
                    predicate.low,
                    predicate.high,
                    scan_plans,
                )
            })?;
            if result.ranges != ranges {
                return Err(format!("scan filter ranges disagree on pair {pair_id}").into());
            }
            let outcome = gen_outcome(result, matched);
            verify(&outcome, expected_sum, matched, "complete Gen scan")?;
            write_result(
                &mut complete_csv,
                pair_id,
                &predicate.label,
                "generated_scan_selective",
                timing,
                &outcome,
                gen_bytes,
            )?;
            let upper = predicate.upper_exclusive()?;
            measure_parquet_complete(
                &mut complete_csv,
                pair_id,
                &predicate.label,
                "parquet_full",
                parquet.clone(),
                parquet_full_sum,
                parquet_full_sum_counted,
                predicate.low,
                upper,
                expected_sum,
                matched,
            )?;
            for (page_rows, bytes) in PARQUET_PAGE_ROWS
                .into_iter()
                .zip(parquet_files.iter())
                .skip(1)
            {
                measure_parquet_complete(
                    &mut complete_csv,
                    pair_id,
                    &predicate.label,
                    &format!("parquet_boundary_search_p{page_rows}"),
                    bytes.clone(),
                    parquet_boundary_sum,
                    parquet_boundary_sum_counted,
                    predicate.low,
                    upper,
                    expected_sum,
                    matched,
                )?;
            }
            measure_parquet_complete(
                &mut complete_csv,
                pair_id,
                &predicate.label,
                "parquet_boundary_search",
                parquet.clone(),
                parquet_boundary_sum,
                parquet_boundary_sum_counted,
                predicate.low,
                upper,
                expected_sum,
                matched,
            )?;
            measure_parquet_complete(
                &mut complete_csv,
                pair_id,
                &predicate.label,
                "parquet_row_filter",
                parquet.clone(),
                parquet_late_sum,
                parquet_late_sum_counted,
                predicate.low,
                upper,
                expected_sum,
                matched,
            )?;
            measure_parquet_complete(
                &mut complete_csv,
                pair_id,
                &predicate.label,
                "parquet_page_index_filter",
                parquet.clone(),
                parquet_indexed_sum,
                parquet_indexed_sum_counted,
                predicate.low,
                upper,
                expected_sum,
                matched,
            )?;
        }
        println!(
            "predicate pair {pair_id:02}: {}/{} -> {} SUM({})",
            pair.group, pair.source, selector.name, value.name
        );
    }
    pair_csv.flush()?;
    predicate_csv.flush()?;
    known_csv.flush()?;
    complete_csv.flush()?;
    drop((pair_csv, predicate_csv, known_csv, complete_csv));
    super::summary::write(&result_dir)?;
    super::source_summary::write(&result_dir)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn measure_parquet_complete(
    output: &mut impl Write,
    pair: usize,
    predicate: &str,
    baseline: &str,
    bytes: Bytes,
    run: fn(Bytes, i64, i64) -> Result<ParquetAggregate, String>,
    counted: fn(Bytes, i64, i64) -> Result<ParquetAggregate, String>,
    low: i64,
    upper: i64,
    expected_sum: i128,
    matched: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let metrics = parquet_outcome(counted(bytes.clone(), low, upper)?);
    let (timing, _) = benchmark(|| run(bytes.clone(), low, upper))?;
    verify(&metrics, expected_sum, matched, baseline)?;
    write_result(
        output,
        pair,
        predicate,
        baseline,
        timing,
        &metrics,
        bytes.len(),
    )?;
    Ok(())
}

fn gen_outcome(result: PipelineResult, matched_rows: usize) -> Outcome {
    Outcome {
        sum: result.sum,
        matched_rows,
        selector: result.selector_metrics,
        value: result.value_metrics,
        selector_decoded_rows: result.selector_decoded_rows,
        value_decoded_rows: result.value_decoded_rows,
        selector_primitive_values_read: result.selector_primitive_values_read,
        value_primitive_values_read: result.value_primitive_values_read,
        page_index_entries_examined: 0,
        candidate_pages: 0,
        boundary_order_used: false,
    }
}

fn parquet_outcome(result: ParquetAggregate) -> Outcome {
    Outcome {
        sum: result.sum,
        matched_rows: result.matched_rows,
        selector: AccessMetrics {
            logical_bytes: 0,
            delivered_bytes: result.bytes_read,
            transferred_bytes: result.unique_bytes,
            transfer_operations: result.read_calls,
            frames_decoded: 0,
        },
        value: zero_metrics(),
        selector_decoded_rows: result.timestamp_values_examined,
        value_decoded_rows: result.matched_rows,
        selector_primitive_values_read: result.timestamp_values_examined,
        value_primitive_values_read: result.matched_rows,
        page_index_entries_examined: result.page_index_entries_examined,
        candidate_pages: result.candidate_pages,
        boundary_order_used: result.boundary_order_used,
    }
}

fn verify(result: &Outcome, expected_sum: i128, matched: usize, name: &str) -> Result<(), String> {
    if result.sum != expected_sum || result.matched_rows != matched {
        return Err(format!("{name} returned an incorrect query result"));
    }
    Ok(())
}

fn benchmark<T, F>(mut run: F) -> Result<((f64, f64, f64), T), String>
where
    F: FnMut() -> Result<T, String>,
{
    let result = run()?;
    black_box(run()?);
    let started = Instant::now();
    black_box(run()?);
    let probe = started.elapsed().as_nanos().max(1);
    let iterations = usize::try_from((TARGET_NS / probe).clamp(1, 1_000)).unwrap();
    let mut samples = Vec::with_capacity(REPEATS);
    for _ in 0..REPEATS {
        let started = Instant::now();
        for _ in 0..iterations {
            black_box(run()?);
        }
        samples.push(started.elapsed().as_nanos() as f64 / iterations as f64);
    }
    samples.sort_by(f64::total_cmp);
    Ok(((samples[1], samples[2], samples[3]), result))
}

fn write_result_header(output: &mut impl Write) -> std::io::Result<()> {
    writeln!(
        output,
        "pair,predicate,baseline,p25_ns,median_ns,p75_ns,sum,selected_rows,logical_bytes,delivered_bytes,transferred_bytes,transfer_operations,frames_decoded,selector_decoded_rows,value_decoded_rows,selector_primitive_values_read,value_primitive_values_read,file_bytes,page_index_entries_examined,candidate_pages,boundary_order_used"
    )
}

fn write_result(
    output: &mut impl Write,
    pair: usize,
    predicate: &str,
    baseline: &str,
    timing: (f64, f64, f64),
    result: &Outcome,
    file_bytes: usize,
) -> std::io::Result<()> {
    writeln!(
        output,
        "{pair},{predicate},{baseline},{:.1},{:.1},{:.1},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
        timing.0,
        timing.1,
        timing.2,
        result.sum,
        result.matched_rows,
        result.selector.logical_bytes + result.value.logical_bytes,
        result.selector.delivered_bytes + result.value.delivered_bytes,
        result.selector.transferred_bytes + result.value.transferred_bytes,
        result.selector.transfer_operations + result.value.transfer_operations,
        result.selector.frames_decoded + result.value.frames_decoded,
        result.selector_decoded_rows,
        result.value_decoded_rows,
        result.selector_primitive_values_read,
        result.value_primitive_values_read,
        file_bytes,
        result.page_index_entries_examined,
        result.candidate_pages,
        result.boundary_order_used,
    )
}

fn zero_metrics() -> AccessMetrics {
    AccessMetrics {
        logical_bytes: 0,
        delivered_bytes: 0,
        transferred_bytes: 0,
        transfer_operations: 0,
        frames_decoded: 0,
    }
}

fn csv(path: impl AsRef<std::path::Path>) -> std::io::Result<BufWriter<File>> {
    Ok(BufWriter::new(File::create(path)?))
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}
