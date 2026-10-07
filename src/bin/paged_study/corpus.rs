//! Selector/value pairs for the paged study.

use std::error::Error;

use witness::experiment::datasets::paged_study_tables;
use witness::experiment::types::Column;

use crate::common::PAGE_ROWS;

/// Rows read per column: 64 pages.
pub const MAX_ROWS: usize = 64 * PAGE_ROWS;
const MIN_PAGES: usize = 4;
const SELECTORS_PER_TABLE: usize = 8;

pub struct Pair {
    pub group: String,
    pub source: String,
    pub selector: usize,
    pub value: usize,
}

/// Every column the study encodes, and the pairs that query them. In each
/// table with at least `MIN_PAGES` pages and two non-constant exact columns,
/// the first such column is the value column and up to
/// `SELECTORS_PER_TABLE` others, spread evenly over the table, select.
pub fn pairs() -> Result<(Vec<Column>, Vec<Pair>), Box<dyn Error>> {
    let mut columns = Vec::new();
    let mut pairs = Vec::new();
    for table in paged_study_tables(MAX_ROWS)? {
        let candidates = table
            .columns
            .into_iter()
            .filter(|column| column.values.len() >= MIN_PAGES * PAGE_ROWS && non_constant(column))
            .collect::<Vec<_>>();
        if candidates.len() < 2 {
            continue;
        }
        let mut candidates = candidates.into_iter();
        let value = columns.len();
        columns.push(candidates.next().unwrap());
        let rest = candidates.collect::<Vec<_>>();
        let chosen = (0..rest.len().min(SELECTORS_PER_TABLE))
            .map(|slot| slot * rest.len() / rest.len().min(SELECTORS_PER_TABLE))
            .collect::<Vec<_>>();
        for (index, column) in rest.into_iter().enumerate() {
            if chosen.contains(&index) {
                pairs.push(Pair {
                    group: table.group.clone(),
                    source: table.source.clone(),
                    selector: columns.len(),
                    value,
                });
                columns.push(column);
            }
        }
    }
    Ok((columns, pairs))
}

fn non_constant(column: &Column) -> bool {
    let mut present = column.values.iter().flatten();
    present
        .next()
        .is_some_and(|first| present.any(|value| value != first))
}
