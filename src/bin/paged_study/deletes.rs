//! Deleting rows from the paged study's tables. Each query is planned as
//! though no row were deleted, runs over the rows that remain, and must
//! return what a scan of those rows returns; every planner and every
//! deletion is timed in the same interleaved rounds, so the cost of
//! deleting is read against the same query on the same table.

use std::error::Error;
use std::io::Write;

use witness::access_compiler::paged::{self, DeletionVector, PagedColumn, Planning};
use witness::experiment::types::Column;

use crate::common::{REPEATS, csv, interleaved};
use crate::corpus::Pair;
use crate::measure::Layouts;
use crate::workload::predicates;

/// The deletions each table receives: rows chosen at random at the given
/// rate, or one contiguous block holding that share of the rows.
const DELETIONS: [(&str, f64, bool); 3] = [
    ("random1", 0.01, false),
    ("random10", 0.10, false),
    ("block10", 0.10, true),
];

/// Planner and deletion (index into the table's deletions, 0 for none) of
/// every timed run of a query.
const RUNS: [(Planning, usize); 6] = [
    (Planning::Facts, 0),
    (Planning::Facts, 1),
    (Planning::Facts, 2),
    (Planning::Facts, 3),
    (Planning::StoredStatistics, 0),
    (Planning::StoredStatistics, 2),
];

struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
}

/// A deletion over `column`'s table, and the rows it deletes.
fn delete(
    column: &PagedColumn,
    seed: u64,
    rate: f64,
    block: bool,
) -> Result<(DeletionVector, Vec<bool>), String> {
    let rows = column.rows();
    let mut random = Random(seed);
    let mut dead = vec![false; rows];
    if block {
        let width = (rows as f64 * rate) as usize;
        let start = random.next() as usize % (rows - width + 1);
        dead[start..start + width].fill(true);
    } else {
        let threshold = (rate * 1_000_000.0) as u64;
        for row in &mut dead {
            *row = random.next() % 1_000_000 < threshold;
        }
    }
    let mut deleted = DeletionVector::for_column(column);
    for (row, _) in dead.iter().enumerate().filter(|(_, dead)| **dead) {
        deleted.delete(row)?;
    }
    Ok((deleted, dead))
}

/// SUM and matching row count over the rows `dead` leaves.
fn truth(
    selector: &[Option<i64>],
    value: &[Option<i64>],
    dead: &[bool],
    low: i64,
    high: i64,
) -> (i128, usize) {
    let (mut sum, mut rows) = (0_i128, 0);
    for ((selected, value), dead) in selector.iter().zip(value).zip(dead) {
        if !dead && selected.is_some_and(|selected| low <= selected && selected <= high) {
            rows += 1;
            sum += value.map_or(0, i128::from);
        }
    }
    (sum, rows)
}

pub fn run(
    result_dir: &str,
    columns: &[Column],
    pairs: &[Pair],
    layouts: &[Layouts],
) -> Result<(), Box<dyn Error>> {
    let mut output = csv(format!("{result_dir}/deletes.csv"))?;
    writeln!(
        output,
        "pair,predicate,low,high,deletion,deleted_rows,arm,matched_rows,expected_sum,skipped,p25_ns,median_ns,p75_ns"
    )?;
    for (pair_id, pair) in pairs.iter().enumerate() {
        let (selector, value) = (&layouts[pair.selector].direct, &layouts[pair.value].direct);
        let (selector_values, value_values) =
            (&columns[pair.selector].values, &columns[pair.value].values);
        let mut deletions = vec![(
            "none",
            DeletionVector::for_column(selector),
            vec![false; selector.rows()],
        )];
        for (slot, (name, rate, block)) in DELETIONS.into_iter().enumerate() {
            let (deleted, dead) = delete(selector, (pair_id * 8 + slot) as u64, rate, block)?;
            deletions.push((name, deleted, dead));
        }
        for predicate in predicates(selector_values)? {
            let (low, high) = (predicate.low, predicate.high);
            let mut checked = Vec::with_capacity(RUNS.len());
            for (planning, slot) in RUNS {
                let (_, deleted, dead) = &deletions[slot];
                let expected = truth(selector_values, value_values, dead, low, high);
                let (sum, matched, counts) =
                    paged::sum_where_live(selector, value, deleted, low, high, planning, 1)?;
                if (sum, matched) != expected {
                    return Err(format!(
                        "{} with {} deleted returned {:?} for pair {pair_id} {}, expected {expected:?}",
                        planning.label(),
                        deletions[slot].0,
                        (sum, matched),
                        predicate.label
                    )
                    .into());
                }
                checked.push((expected, counts.skipped));
            }
            let mut runs = RUNS.map(|(planning, slot)| {
                let deleted = &deletions[slot].1;
                move || paged::sum_where_live(selector, value, deleted, low, high, planning, 1)
            });
            let quartiles = interleaved(&mut runs, REPEATS)?;
            for (((planning, slot), ((sum, matched), skipped)), (p25, median, p75)) in
                RUNS.iter().zip(&checked).zip(quartiles)
            {
                let (name, deleted, _) = &deletions[*slot];
                writeln!(
                    output,
                    "{pair_id},{},{low},{high},{name},{},{},{matched},{sum},{skipped},{p25:.1},{median:.1},{p75:.1}",
                    predicate.label,
                    deleted.deleted(),
                    planning.label(),
                )?;
            }
        }
        if pair_id % 50 == 49 {
            eprintln!("deleted from {} of {} pairs", pair_id + 1, pairs.len());
        }
    }
    output.flush()?;
    Ok(())
}
