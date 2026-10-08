//! Selective SUM over string selectors: every string column of the string
//! corpus, stored as pages of a sorted dictionary and integer ids, queried
//! with range and prefix predicates under the facts, zone-map, and scan
//! planners. Pairs are exported as Parquet with the selector as bytes, so
//! other engines run the same predicates.

#[allow(dead_code)]
#[path = "paged_study/common.rs"]
mod common;

use std::error::Error;
use std::fs;
use std::io::Write;
use std::ops::Bound;
use std::sync::Arc;

use arrow_array::{BinaryArray, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use parquet::arrow::ArrowWriter;
use witness::access_compiler::paged::{PagePolicy, PagedColumn, Planning};
use witness::access_compiler::primitive_rule_fingerprint;
use witness::access_compiler::strings::{
    StringColumn, StringRange, bound_view, prefix_range, string_sum_where,
};
use witness::experiment::datasets::string_study_tables;

use common::{PAGE_ROWS, benchmark, csv, direct_menu, quote};

const DEFAULT_RESULT_DIR: &str = "experiments/results/paged_string";
const MAX_ROWS: usize = 64 * PAGE_ROWS;
const MIN_PAGES: usize = 4;
const SELECTORS_PER_TABLE: usize = 8;
const WINDOWS: [(f64, &str); 4] = [
    (0.001, "0.1pct"),
    (0.01, "1pct"),
    (0.1, "10pct"),
    (0.5, "50pct"),
];

/// A predicate over strings with owned bounds.
struct Predicate {
    label: String,
    low: Bound<Vec<u8>>,
    high: Bound<Vec<u8>>,
}

impl Predicate {
    fn range(&self) -> StringRange<'_> {
        StringRange {
            low: bound_view(&self.low),
            high: bound_view(&self.high),
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let result_dir = std::env::var("WITNESS_PAGED_STRING_RESULT_DIR")
        .unwrap_or_else(|_| DEFAULT_RESULT_DIR.into());
    let export = format!("{result_dir}/parquet");
    fs::create_dir_all(&export)?;
    let policy = PagePolicy::smallest(direct_menu());
    let mut pairs = csv(format!("{result_dir}/pairs.csv"))?;
    writeln!(
        pairs,
        "pair,group,source,selector,rows,pages,distinct_per_page,dictionary_bytes,column_bytes"
    )?;
    let mut queries = csv(format!("{result_dir}/queries.csv"))?;
    writeln!(
        queries,
        "pair,predicate,low,low_kind,high,high_kind,utf8,matched_rows,expected_sum,arm,p25_ns,median_ns,p75_ns,probes,skipped,accepted,translated,scanned"
    )?;
    let mut pair_id = 0;
    for table in string_study_tables(MAX_ROWS)? {
        if table.measure.len() < MIN_PAGES * PAGE_ROWS {
            continue;
        }
        let measure = PagedColumn::from_values(&table.measure, PAGE_ROWS, &policy)?;
        let candidates = table
            .columns
            .iter()
            .filter(|(_, values)| values.len() == table.measure.len())
            .filter(|(_, values)| {
                let mut present = values.iter().flatten();
                present
                    .next()
                    .is_some_and(|first| present.any(|value| value != first))
            })
            .collect::<Vec<_>>();
        let chosen = (0..candidates.len().min(SELECTORS_PER_TABLE))
            .map(|slot| slot * candidates.len() / candidates.len().min(SELECTORS_PER_TABLE))
            .collect::<Vec<_>>();
        for (name, values) in chosen.into_iter().map(|index| candidates[index]) {
            let views = values
                .iter()
                .map(|value| value.as_deref())
                .collect::<Vec<_>>();
            let column = StringColumn::from_values(&views, PAGE_ROWS, &policy)?;
            let distinct = (0..column.pages())
                .map(|page| column.dictionary(page).len())
                .sum::<usize>()
                / column.pages().max(1);
            let dictionary_bytes = (0..column.pages())
                .map(|page| column.dictionary(page).stored_bytes())
                .sum::<usize>();
            writeln!(
                pairs,
                "{pair_id},{},{},{},{},{},{distinct},{dictionary_bytes},{}",
                quote(&table.group),
                quote(&table.source),
                quote(name),
                values.len(),
                column.pages(),
                column.stored_bytes(),
            )?;
            let utf8 = export_pair(
                &format!("{export}/pair_{pair_id}.parquet"),
                values,
                &table.measure,
            )?;
            for predicate in predicates(values) {
                let range = predicate.range();
                let expected = values.iter().zip(&table.measure).fold(
                    (0_i128, 0_usize),
                    |(sum, matched), (value, measure)| match value {
                        Some(value) if range.contains(value) => {
                            (sum + measure.map_or(0, i128::from), matched + 1)
                        }
                        _ => (sum, matched),
                    },
                );
                for (planning, arm) in [
                    (Planning::Facts, "witness_direct_facts"),
                    (Planning::StoredStatistics, "witness_direct_zonemap"),
                    (Planning::ScanOnly, "witness_direct_scan"),
                ] {
                    let run = || string_sum_where(&column, range, &measure, planning);
                    let (sum, matched, counts, probes) = run()?;
                    if (sum, matched) != expected {
                        return Err(format!(
                            "{arm} returned {:?} on {name} {}, expected {expected:?}",
                            (sum, matched),
                            predicate.label
                        )
                        .into());
                    }
                    let (p25, median, p75) = benchmark(|| run().map(|(sum, _, _, _)| sum))?;
                    let (low, low_kind) = encode(&predicate.low);
                    let (high, high_kind) = encode(&predicate.high);
                    writeln!(
                        queries,
                        "{pair_id},{},{low},{low_kind},{high},{high_kind},{utf8},{},{},{arm},{p25:.1},{median:.1},{p75:.1},{probes},{},{},{},{}",
                        predicate.label,
                        expected.1,
                        expected.0,
                        counts.skipped,
                        counts.accepted,
                        counts.translated,
                        counts.scanned,
                    )?;
                }
            }
            pair_id += 1;
        }
        eprintln!("measured {} {}", table.group, table.source);
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
    eprintln!("measured {pair_id} string pairs");
    Ok(())
}

/// Centered quantile windows of the sorted non-null strings, merged when two
/// targets give the same bounds, and the strings sharing the median's first
/// two bytes.
fn predicates(values: &[Option<Vec<u8>>]) -> Vec<Predicate> {
    let mut sorted = values
        .iter()
        .flatten()
        .map(Vec::as_slice)
        .collect::<Vec<_>>();
    sorted.sort_unstable();
    let count = sorted.len();
    let mut out: Vec<Predicate> = Vec::new();
    for (fraction, label) in WINDOWS {
        let width = ((count as f64 * fraction).round() as usize).max(1);
        let start = (count - width) / 2;
        let (low, high) = (sorted[start].to_vec(), sorted[start + width - 1].to_vec());
        match out.iter_mut().find(|predicate| {
            predicate.low == Bound::Included(low.clone())
                && predicate.high == Bound::Included(high.clone())
        }) {
            Some(predicate) => predicate.label = format!("{}+{label}", predicate.label),
            None => out.push(Predicate {
                label: label.to_string(),
                low: Bound::Included(low),
                high: Bound::Included(high),
            }),
        }
    }
    let median = sorted[count / 2];
    let (low, high) = prefix_range(&median[..median.len().min(2)]);
    out.push(Predicate {
        label: "prefix".into(),
        low: Bound::Included(low),
        high: high.map_or(Bound::Unbounded, Bound::Excluded),
    });
    out
}

/// A bound as hex and its kind, for engines that rerun the predicate.
fn encode(bound: &Bound<Vec<u8>>) -> (String, &'static str) {
    let hex = |bytes: &[u8]| {
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    match bound {
        Bound::Included(value) => (hex(value), "included"),
        Bound::Excluded(value) => (hex(value), "excluded"),
        Bound::Unbounded => (String::new(), "unbounded"),
    }
}

/// Writes a pair as Parquet: the selector as text when every value is valid
/// UTF-8, which lets an engine store and compare it as text, else as bytes.
/// Returns whether it is text.
fn export_pair(
    path: &str,
    values: &[Option<Vec<u8>>],
    measure: &[Option<i64>],
) -> Result<bool, Box<dyn Error>> {
    let utf8 = values
        .iter()
        .flatten()
        .all(|value| std::str::from_utf8(value).is_ok());
    let (kind, selector): (DataType, Arc<dyn arrow_array::Array>) = if utf8 {
        (
            DataType::Utf8,
            Arc::new(StringArray::from(
                values
                    .iter()
                    .map(|value| {
                        value
                            .as_deref()
                            .map(|value| std::str::from_utf8(value).unwrap())
                    })
                    .collect::<Vec<_>>(),
            )),
        )
    } else {
        (
            DataType::Binary,
            Arc::new(BinaryArray::from(
                values
                    .iter()
                    .map(|value| value.as_deref())
                    .collect::<Vec<_>>(),
            )),
        )
    };
    let schema = Arc::new(Schema::new(vec![
        Field::new("timestamp", kind, true),
        Field::new("value", DataType::Int64, true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![selector, Arc::new(Int64Array::from(measure.to_vec()))],
    )?;
    let mut writer = ArrowWriter::try_new(fs::File::create(path)?, schema, None)?;
    writer.write(&batch)?;
    writer.close()?;
    Ok(utf8)
}
