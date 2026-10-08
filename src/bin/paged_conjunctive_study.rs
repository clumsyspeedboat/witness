//! Conjunctive queries on full-length columns: TPC-H Q6 on SF10 lineitem,
//! and the same query with its date range replaced by a range of the sorted
//! order key, under each planner on 1 to 64 threads. The study exports the
//! five columns as Parquet so other engines can run the same SQL.

#[allow(dead_code)]
#[path = "paged_study/common.rs"]
mod common;

use std::error::Error;
use std::fs;
use std::io::Write;
use std::sync::Arc;

use arrow_array::{Int64Array, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use parquet::arrow::ArrowWriter;
use parquet::file::properties::WriterProperties;
use witness::access_compiler::paged::{
    self, Aggregate, Conjunct, PageCounts, PagePolicy, PagedColumn, Planning,
};
use witness::experiment::datasets::scale_columns;

use common::{PAGE_ROWS, csv, direct_menu, interleaved};

const DEFAULT_RESULT_DIR: &str = "experiments/results/paged_conjunctive";
const THREADS: [usize; 7] = [1, 2, 4, 8, 16, 32, 64];
/// Every planner, timed in interleaved rounds so drift falls on all alike.
const PLANNERS: [Planning; 5] = [
    Planning::Facts,
    Planning::StoredStatistics,
    Planning::ScanOnly,
    Planning::FactBounds,
    Planning::StoredStatisticsPlus,
];
const ROUNDS: usize = 21;

/// Columns the queries read, by a fragment of their corpus name, and the
/// name the exported table gives them.
const COLUMNS: [(&str, &str); 5] = [
    ("l_orderkey", "orderkey"),
    ("l_shipdate", "shipdate"),
    ("l_discount", "discount"),
    ("l_quantity", "quantity"),
    ("l_extendedprice", "extendedprice"),
];
const ORDERKEY: usize = 0;
const SHIPDATE: usize = 1;
const DISCOUNT: usize = 2;
const QUANTITY: usize = 3;
const PRICE: usize = 4;

/// 1994-01-01 and the last second of 1994, as the corpus stores dates.
const YEAR_START: i64 = 757_382_400;
const YEAR_END: i64 = 788_918_399;

struct Query {
    name: &'static str,
    conjuncts: Vec<(usize, i64, i64)>,
}

fn main() -> Result<(), Box<dyn Error>> {
    let result_dir = std::env::var("WITNESS_PAGED_CONJUNCTIVE_RESULT_DIR")
        .unwrap_or_else(|_| DEFAULT_RESULT_DIR.into());
    fs::create_dir_all(&result_dir)?;
    let corpus = scale_columns()?;
    let mut values = Vec::with_capacity(COLUMNS.len());
    for (fragment, _) in COLUMNS {
        let column = corpus
            .iter()
            .find(|column| column.name.contains(fragment))
            .ok_or_else(|| format!("scale corpus lacks {fragment}; run fetch_scale_corpus.py"))?;
        values.push(column.values.clone());
    }
    drop(corpus);
    let policy = PagePolicy::smallest(direct_menu());
    let layouts = std::thread::scope(|scope| {
        values
            .iter()
            .map(|column| scope.spawn(|| PagedColumn::from_values(column, PAGE_ROWS, &policy)))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().expect("page encoding panicked"))
            .collect::<Result<Vec<_>, String>>()
    })?;

    // Q6 keeps a seventh of the years; the clustered variant keeps the middle
    // seventh of the order keys instead.
    let keys = values[ORDERKEY].iter().flatten();
    let (low_key, high_key) = (
        *keys.clone().min().ok_or("order key column is empty")?,
        *keys.max().ok_or("order key column is empty")?,
    );
    let seventh = (high_key - low_key) / 7;
    let filters = [(DISCOUNT, 5, 7), (QUANTITY, i64::MIN, 23)];
    let queries = [
        Query {
            name: "q6",
            conjuncts: [vec![(SHIPDATE, YEAR_START, YEAR_END)], filters.to_vec()].concat(),
        },
        Query {
            name: "q6_orderkey",
            conjuncts: [
                vec![(ORDERKEY, low_key + 3 * seventh, low_key + 4 * seventh - 1)],
                filters.to_vec(),
            ]
            .concat(),
        },
    ];

    let mut spec = csv(format!("{result_dir}/spec.csv"))?;
    writeln!(spec, "query,column,low,high,expected_sum,matched_rows")?;
    let mut output = csv(format!("{result_dir}/queries.csv"))?;
    writeln!(
        output,
        "query,arm,threads,rows,pages,p25_ns,median_ns,p75_ns,matched_rows,skipped,accepted,searched,translated,scanned"
    )?;
    for query in &queries {
        let expected = truth(&values, &query.conjuncts);
        for &(column, low, high) in &query.conjuncts {
            writeln!(
                spec,
                "{},{},{low},{high},{},{}",
                query.name, COLUMNS[column].1, expected.0, expected.1
            )?;
        }
        let conjuncts = query
            .conjuncts
            .iter()
            .map(|&(column, low, high)| Conjunct {
                column: &layouts[column],
                low,
                high,
            })
            .collect::<Vec<_>>();
        let aggregate = Aggregate::SumProduct(&layouts[PRICE], &layouts[DISCOUNT]);
        for threads in THREADS {
            let conjuncts = &conjuncts;
            let make = |planning| {
                move || {
                    paged::aggregate_where(conjuncts, aggregate, planning, threads)
                        .map(|(sum, _, _)| sum)
                }
            };
            let mut counts = Vec::with_capacity(PLANNERS.len());
            for planning in PLANNERS {
                let (sum, matched, page_counts) =
                    paged::aggregate_where(conjuncts, aggregate, planning, threads)?;
                if (sum, matched) != expected {
                    return Err(format!(
                        "{} x{threads} returned {:?} for {}, expected {expected:?}",
                        planning.label(),
                        (sum, matched),
                        query.name
                    )
                    .into());
                }
                counts.push(page_counts);
            }
            let mut runs = PLANNERS.map(make);
            let timings = interleaved(&mut runs, ROUNDS)?;
            for ((planning, counts), (p25, median, p75)) in PLANNERS.iter().zip(counts).zip(timings)
            {
                let PageCounts {
                    skipped,
                    accepted,
                    searched,
                    translated,
                    scanned,
                    ..
                } = counts;
                writeln!(
                    output,
                    "{},witness_direct_{},{threads},{},{},{p25:.1},{median:.1},{p75:.1},{},{skipped},{accepted},{searched},{translated},{scanned}",
                    query.name,
                    planning.label(),
                    values[0].len(),
                    layouts[0].pages().len(),
                    expected.1,
                )?;
            }
        }
        eprintln!("measured {}", query.name);
    }
    spec.flush()?;
    output.flush()?;
    export(&format!("{result_dir}/parquet"), &values)?;
    fs::write(
        format!("{result_dir}/freeze.csv"),
        format!(
            "rule_fingerprint,page_rows,rounds\n{:#018x},{PAGE_ROWS},{ROUNDS}\n",
            witness::access_compiler::primitive_rule_fingerprint()
        ),
    )?;
    Ok(())
}

/// Exact SUM(extendedprice * discount) and row count of a conjunctive query.
fn truth(values: &[Vec<Option<i64>>], conjuncts: &[(usize, i64, i64)]) -> (i128, usize) {
    (0..values[PRICE].len())
        .filter(|&row| {
            conjuncts.iter().all(|&(column, low, high)| {
                values[column][row].is_some_and(|value| low <= value && value <= high)
            })
        })
        .fold((0, 0), |(sum, matched), row| {
            let product = values[PRICE][row]
                .zip(values[DISCOUNT][row])
                .map_or(0, |(price, discount)| {
                    i128::from(price) * i128::from(discount)
                });
            (sum + product, matched + 1)
        })
}

/// The five columns as one plain Parquet table, for engines outside this
/// crate.
fn export(dir: &str, values: &[Vec<Option<i64>>]) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(dir)?;
    let schema = Arc::new(Schema::new(
        COLUMNS
            .iter()
            .zip(values)
            .map(|((_, name), column)| {
                Field::new(*name, DataType::Int64, column.iter().any(Option::is_none))
            })
            .collect::<Vec<_>>(),
    ));
    let properties = WriterProperties::builder()
        .set_max_row_group_row_count(Some(1 << 20))
        .build();
    let file = fs::File::create(format!("{dir}/lineitem.parquet"))?;
    let mut writer = ArrowWriter::try_new(file, schema.clone(), Some(properties))?;
    let rows = values[0].len();
    let mut start = 0;
    while start < rows {
        let end = (start + (1 << 20)).min(rows);
        let batch = RecordBatch::try_new(
            schema.clone(),
            values
                .iter()
                .map(|column| {
                    Arc::new(Int64Array::from(column[start..end].to_vec()))
                        as Arc<dyn arrow_array::Array>
                })
                .collect(),
        )?;
        writer.write(&batch)?;
        start = end;
    }
    writer.close()?;
    Ok(())
}
