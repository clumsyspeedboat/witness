//! Intra-query parallelism on full-length columns: TPC-H SF10 lineitem,
//! about 3,700 pages of 16,384 rows per column, queried on 1 to 64 threads.
//! A single-threaded Parquet reader with page-index pruning is the reference.

#[allow(dead_code)]
#[path = "predicate_pipeline/workload.rs"]
mod workload;

#[allow(dead_code)]
#[path = "paged_study/common.rs"]
mod common;

use std::error::Error;
use std::fs;
use std::io::Write;

use bytes::Bytes;
use witness::access_compiler::paged::{self, PagePolicy, PagedColumn, Planning};
use witness::experiment::datasets::scale_columns;
use witness::experiment::time_window::{parquet_indexed_sum, parquet_table_file};

use common::{PAGE_ROWS, benchmark, csv, direct_menu, quote, truth};
use workload::predicates;

const DEFAULT_RESULT_DIR: &str = "experiments/results/paged_scale";
const THREADS: [usize; 7] = [1, 2, 4, 8, 16, 32, 64];
const VALUE_COLUMN: &str = "extendedprice";

fn main() -> Result<(), Box<dyn Error>> {
    let result_dir = std::env::var("WITNESS_PAGED_SCALE_RESULT_DIR")
        .unwrap_or_else(|_| DEFAULT_RESULT_DIR.into());
    fs::create_dir_all(&result_dir)?;
    let columns = scale_columns()?;
    let value = columns
        .iter()
        .position(|column| column.name.contains(VALUE_COLUMN))
        .ok_or("scale corpus lacks the value column; run fetch_scale_corpus.py")?;
    let policy = PagePolicy::smallest(direct_menu());
    let layouts = std::thread::scope(|scope| {
        columns
            .iter()
            .map(|column| {
                scope.spawn(|| PagedColumn::from_values(&column.values, PAGE_ROWS, &policy))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().expect("page encoding panicked"))
            .collect::<Result<Vec<_>, String>>()
    })?;
    let mut output = csv(format!("{result_dir}/scaling.csv"))?;
    writeln!(
        output,
        "selector,predicate,rows,pages,arm,threads,p25_ns,median_ns,p75_ns,matched_rows,low,high,expected_sum"
    )?;
    let export = format!("{result_dir}/parquet");
    fs::create_dir_all(&export)?;
    for (index, selector) in columns.iter().enumerate() {
        if index == value {
            continue;
        }
        let parquet = Bytes::from(parquet_table_file(
            &selector.values,
            &columns[value].values,
            PAGE_ROWS,
            false,
        )?);
        // The plain file, for engines outside this crate to query.
        fs::write(format!("{export}/{}.parquet", selector.name), &parquet)?;
        for predicate in predicates(&selector.values)? {
            let expected = truth(
                &selector.values,
                &columns[value].values,
                predicate.low,
                predicate.high,
            );
            let mut record = |arm: &str, threads: usize, timing: (f64, f64, f64)| {
                writeln!(
                    output,
                    "{},{},{},{},{arm},{threads},{:.1},{:.1},{:.1},{},{},{},{}",
                    quote(&selector.name),
                    predicate.label,
                    selector.values.len(),
                    layouts[index].pages().len(),
                    timing.0,
                    timing.1,
                    timing.2,
                    expected.1,
                    predicate.low,
                    predicate.high,
                    expected.0
                )
            };
            for planning in [
                Planning::Facts,
                Planning::ScanOnly,
                Planning::StoredStatistics,
            ] {
                let arm = match planning {
                    Planning::Facts => "witness_direct_facts",
                    Planning::ScanOnly => "witness_direct_scan",
                    Planning::StoredStatistics => "witness_direct_zonemap",
                };
                for threads in THREADS {
                    let run = || {
                        paged::sum_where(
                            &layouts[index],
                            &layouts[value],
                            predicate.low,
                            predicate.high,
                            planning,
                            threads,
                        )
                        .map(|(sum, matched, _)| (sum, matched))
                    };
                    if run()? != expected {
                        return Err(
                            format!("{arm} x{threads} disagrees on {}", selector.name).into()
                        );
                    }
                    record(arm, threads, benchmark(run)?)?;
                }
            }
            let upper = predicate
                .upper_exclusive()
                .map_err(|error| format!("{}: {error}", selector.name))?;
            let run = || {
                parquet_indexed_sum(parquet.clone(), predicate.low, upper)
                    .map(|result| (result.sum, result.matched_rows))
            };
            if run()? != expected {
                return Err(format!("Parquet disagrees on {}", selector.name).into());
            }
            record("parquet_plain_indexed", 1, benchmark(run)?)?;
        }
        eprintln!("scaled {}", selector.name);
    }
    output.flush()?;
    Ok(())
}
