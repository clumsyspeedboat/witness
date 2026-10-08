//! Selective SUM over floating-point selectors. Every float column of the
//! paged study's tables is stored as order-preserving integer keys
//! (`floats::key`), so the integer codecs, facts, and planners apply
//! unchanged; a range of doubles is a range of keys. Each query runs under
//! the facts, zone-map, and scan planners, and the pairs are exported with
//! the selector as doubles, so other engines run the same floating-point
//! predicates.

#[allow(dead_code)]
#[path = "predicate_pipeline/workload.rs"]
mod workload;

#[allow(dead_code)]
#[path = "paged_study/common.rs"]
mod common;

use std::error::Error;
use std::fs;
use std::io::Write;
use std::sync::Arc;

use arrow_array::{Float64Array, Int64Array, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use parquet::arrow::ArrowWriter;
use witness::access_compiler::floats;
use witness::access_compiler::paged::{self, PagePolicy, PagedColumn, Planning};
use witness::access_compiler::primitive_rule_fingerprint;
use witness::experiment::datasets::float_key_tables;
use witness::experiment::types::Column;

use common::{PAGE_ROWS, benchmark, csv, direct_menu, quote, truth};
use workload::predicates;

const DEFAULT_RESULT_DIR: &str = "experiments/results/paged_float";
const MAX_ROWS: usize = 64 * PAGE_ROWS;
const MIN_PAGES: usize = 4;
const SELECTORS_PER_TABLE: usize = 8;

fn main() -> Result<(), Box<dyn Error>> {
    let result_dir = std::env::var("WITNESS_PAGED_FLOAT_RESULT_DIR")
        .unwrap_or_else(|_| DEFAULT_RESULT_DIR.into());
    let export = format!("{result_dir}/parquet");
    fs::create_dir_all(&export)?;
    let policy = PagePolicy::smallest(direct_menu());
    let mut pairs = csv(format!("{result_dir}/pairs.csv"))?;
    writeln!(pairs, "pair,group,source,selector,value,rows,pages")?;
    let mut queries = csv(format!("{result_dir}/queries.csv"))?;
    writeln!(
        queries,
        "pair,predicate,low,high,matched_rows,expected_sum,arm,p25_ns,median_ns,p75_ns,skipped,accepted,searched,translated,scanned"
    )?;
    let mut pair_id = 0;
    for table in float_key_tables(MAX_ROWS)? {
        let non_constant = |column: &&Column| {
            let mut present = column.values.iter().flatten();
            present
                .next()
                .is_some_and(|first| present.any(|value| value != first))
        };
        let Some(value) = table
            .columns
            .iter()
            .filter(|column| !column.name.ends_with("_f64"))
            .find(non_constant)
        else {
            continue;
        };
        let selectors = table
            .columns
            .iter()
            .filter(|column| column.name.ends_with("_f64"))
            .filter(|column| column.values.len() >= MIN_PAGES * PAGE_ROWS)
            .filter(non_constant)
            .take(SELECTORS_PER_TABLE)
            .collect::<Vec<_>>();
        if selectors.is_empty() {
            continue;
        }
        let value_column = PagedColumn::from_values(&value.values, PAGE_ROWS, &policy)?;
        for selector in selectors {
            let selector_column = PagedColumn::from_values(&selector.values, PAGE_ROWS, &policy)?;
            writeln!(
                pairs,
                "{pair_id},{},{},{},{},{},{}",
                quote(&table.group),
                quote(&table.source),
                quote(&selector.name),
                quote(&value.name),
                selector.values.len(),
                selector_column.pages().len()
            )?;
            export_pair(
                &format!("{export}/pair_{pair_id}.parquet"),
                &selector.values,
                &value.values,
            )?;
            for predicate in predicates(&selector.values)? {
                let expected = truth(
                    &selector.values,
                    &value.values,
                    predicate.low,
                    predicate.high,
                );
                // The predicate's bounds as doubles: the exact values the key
                // range starts and ends at.
                let (low, high) = (
                    floats::from_key(predicate.low),
                    floats::from_key(predicate.high),
                );
                for (planning, arm) in [
                    (Planning::Facts, "witness_direct_facts"),
                    (Planning::StoredStatistics, "witness_direct_zonemap"),
                    (Planning::ScanOnly, "witness_direct_scan"),
                ] {
                    let run = || {
                        paged::sum_where(
                            &selector_column,
                            &value_column,
                            predicate.low,
                            predicate.high,
                            planning,
                            1,
                        )
                    };
                    let (sum, matched, counts) = run()?;
                    if (sum, matched) != expected {
                        return Err(format!(
                            "{arm} returned {:?} on {} {}, expected {expected:?}",
                            (sum, matched),
                            selector.name,
                            predicate.label
                        )
                        .into());
                    }
                    let (p25, median, p75) = benchmark(|| run().map(|(sum, _, _)| sum))?;
                    writeln!(
                        queries,
                        "{pair_id},{},{low:?},{high:?},{},{},{arm},{p25:.1},{median:.1},{p75:.1},{},{},{},{},{}",
                        predicate.label,
                        expected.1,
                        expected.0,
                        counts.skipped,
                        counts.accepted,
                        counts.searched,
                        counts.translated,
                        counts.scanned,
                    )?;
                }
            }
            pair_id += 1;
        }
    }
    pairs.flush()?;
    queries.flush()?;
    fs::write(
        format!("{result_dir}/freeze.csv"),
        format!(
            "rule_fingerprint,page_rows,pairs\n{:#018x},{PAGE_ROWS},{pair_id}\n",
            primitive_rule_fingerprint()
        ),
    )?;
    eprintln!("measured {pair_id} float pairs");
    Ok(())
}

/// A pair as Parquet with the selector as doubles and the value as integers.
fn export_pair(
    path: &str,
    keys: &[Option<i64>],
    values: &[Option<i64>],
) -> Result<(), Box<dyn Error>> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("timestamp", DataType::Float64, true),
        Field::new("value", DataType::Int64, true),
    ]));
    let doubles = keys
        .iter()
        .map(|key| key.map(floats::from_key))
        .collect::<Vec<_>>();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(Float64Array::from(doubles)),
            Arc::new(Int64Array::from(values.to_vec())),
        ],
    )?;
    let mut writer = ArrowWriter::try_new(fs::File::create(path)?, schema, None)?;
    writer.write(&batch)?;
    writer.close()?;
    Ok(())
}
