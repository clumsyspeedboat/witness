//! Declared metadata that is wrong or missing. Every eighth selector page of
//! every pair gets faulty statistics in one of three ways, and the paged
//! queries run under each planner that reads statistics or a footer, and
//! under the fact planner, which reads neither. Each answer is compared with
//! the truth: a planner that trusts a wrong summary answers wrongly, a footer
//! of derived bounds fails the query when a page it reads disagrees with its
//! entry, and facts derived from the pages are unaffected.

use std::collections::BTreeMap;
use std::error::Error;
use std::io::Write;

use witness::access_compiler::paged::{self, PagedColumn, Planning};
use witness::experiment::types::Column;

use crate::common::{csv, truth};
use crate::corpus::Pair;
use crate::measure::{Layouts, parallel};
use crate::workload::predicates;

/// One page in this many gets a faulty summary.
const FAULT_EVERY: usize = 8;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Fault {
    /// The entry of the next page: a summary out of step with its page.
    Neighbour,
    /// The middle half of the true interval, as a writer that truncated its
    /// minimum and maximum would record (PARQUET-251).
    Truncated,
    /// No entry: a writer that recorded no statistics for the page.
    Absent,
}

impl Fault {
    fn name(self) -> &'static str {
        match self {
            Self::Neighbour => "neighbour",
            Self::Truncated => "truncated",
            Self::Absent => "absent",
        }
    }

    fn apply(self, column: &PagedColumn, index: usize) -> Option<(i128, i128)> {
        let pages = column.pages();
        let own = pages[index].stats;
        match self {
            Self::Neighbour => pages
                .get(index + 1)
                .or_else(|| index.checked_sub(1).and_then(|before| pages.get(before)))
                .and_then(|other| other.stats),
            Self::Truncated => own.map(|(low, high)| {
                let quarter = (high - low) / 4;
                (low + quarter, high - quarter)
            }),
            Self::Absent => None,
        }
    }
}

const FAULTS: [Fault; 3] = [Fault::Neighbour, Fault::Truncated, Fault::Absent];

/// The planners whose answers a summary can change, and the fact planner.
const PLANNERS: [Planning; 5] = [
    Planning::StoredStatistics,
    Planning::StoredStatisticsPlus,
    Planning::StoredStatisticsDictionary,
    Planning::DerivedFooter,
    Planning::Facts,
];

#[derive(Clone, Copy, Debug, Default)]
struct Tally {
    queries: usize,
    correct: usize,
    wrong: usize,
    failed: usize,
    /// Queries in which a faulty entry differs from the true one.
    affected: usize,
}

pub fn run(
    result_dir: &str,
    columns: &[Column],
    pairs: &[Pair],
    layouts: &[Layouts],
) -> Result<(), Box<dyn Error>> {
    let tallies = parallel(pairs.len(), |index| {
        let pair = &pairs[index];
        fault_pair(
            &layouts[pair.selector].direct,
            &layouts[pair.value].direct,
            &columns[pair.selector],
            &columns[pair.value],
        )
    })?;
    let mut total = BTreeMap::<(Fault, &'static str), Tally>::new();
    for tally in tallies {
        for (key, part) in tally {
            let entry = total.entry(key).or_default();
            entry.queries += part.queries;
            entry.correct += part.correct;
            entry.wrong += part.wrong;
            entry.failed += part.failed;
            entry.affected += part.affected;
        }
    }
    let mut output = csv(format!("{result_dir}/faults.csv"))?;
    writeln!(
        output,
        "fault,planner,queries,affected,correct,wrong,failed"
    )?;
    for ((fault, planner), tally) in &total {
        writeln!(
            output,
            "{},{},{},{},{},{},{}",
            fault.name(),
            planner,
            tally.queries,
            tally.affected,
            tally.correct,
            tally.wrong,
            tally.failed
        )?;
        if *planner == Planning::Facts.label() && (tally.wrong > 0 || tally.failed > 0) {
            return Err("faulty statistics changed an answer planned from facts".into());
        }
    }
    output.flush()?;
    Ok(())
}

fn fault_pair(
    selector: &PagedColumn,
    value: &PagedColumn,
    selector_values: &Column,
    value_values: &Column,
) -> Result<BTreeMap<(Fault, &'static str), Tally>, String> {
    let mut tally = BTreeMap::new();
    let predicates = predicates(&selector_values.values).map_err(|error| error.to_string())?;
    // The footer holds each page's derived bounds where the zone map holds
    // its exact statistics; both then receive the same faults.
    let mut footer = selector.clone();
    for index in 0..footer.pages().len() {
        let bound = paged::derived_bounds(&footer.pages()[index])?;
        footer.replace_statistics(index, bound)?;
    }
    for fault in FAULTS {
        let faulty = |source: &PagedColumn| -> Result<(PagedColumn, Vec<usize>), String> {
            let mut column = source.clone();
            let mut changed = Vec::new();
            for index in (FAULT_EVERY / 2..source.pages().len()).step_by(FAULT_EVERY) {
                let entry = fault.apply(source, index);
                if entry != source.pages()[index].stats {
                    changed.push(index);
                }
                column.replace_statistics(index, entry)?;
            }
            Ok((column, changed))
        };
        let (zone_map, zone_changed) = faulty(selector)?;
        let (bounds, bounds_changed) = faulty(&footer)?;
        for predicate in &predicates {
            let expected = truth(
                &selector_values.values,
                &value_values.values,
                predicate.low,
                predicate.high,
            );
            for planning in PLANNERS {
                let (column, changed) = match planning {
                    Planning::DerivedFooter => (&bounds, &bounds_changed),
                    _ => (&zone_map, &zone_changed),
                };
                let entry: &mut Tally = tally.entry((fault, planning.label())).or_default();
                entry.queries += 1;
                entry.affected += usize::from(!changed.is_empty());
                match paged::sum_where(column, value, predicate.low, predicate.high, planning, 1) {
                    Ok((sum, rows, _)) if (sum, rows) == expected => entry.correct += 1,
                    Ok(_) => entry.wrong += 1,
                    Err(_) => entry.failed += 1,
                }
            }
        }
    }
    Ok(tally)
}
