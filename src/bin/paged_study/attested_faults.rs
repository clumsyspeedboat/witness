//! Attestations that are wrong. Where `faults` corrupts the statistics a
//! zone map stores, which the fact planner never reads, this study corrupts
//! what the fact planner trusts from the writer: the order and null
//! placement in a page's descriptor, the sortedness of its dictionary, and
//! the ends the column certificate is built from. Every eighth selector page
//! of every pair is forged in one of six ways, after it was sealed, and the
//! paged queries run under the fact planner, the zone map, the zone map with
//! the encoding's shortcuts, and a footer of derived bounds, which the
//! writer derives from its forged facts. Each answer is compared with the
//! truth: correct, wrong, or stopped by the checker or another error.

use std::collections::BTreeMap;
use std::error::Error;
use std::io::Write;

use witness::access_compiler::paged::{self, PagedColumn, Planning};
use witness::access_compiler::{CheckedInvariants, NullPlacement};
use witness::experiment::types::Column;

use crate::common::{csv, truth};
use crate::corpus::Pair;
use crate::faults::FAULT_EVERY;
use crate::measure::{Layouts, parallel};
use crate::workload::predicates;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Fault {
    /// A page that is not ordered claims order in its descriptor: over all
    /// rows when it holds no null, over its non-null rows when it does.
    Order,
    /// A dictionary in the order its values first appear claims to be sorted.
    Unsorted,
    /// The page's ends, from which the column certificate is built, cover the
    /// middle half of its true interval.
    Ends,
    /// The descriptor places the page's nulls first where they come last,
    /// last where they come first, and first where they lie anywhere.
    Nulls,
    /// A dictionary sorted as unsigned integers, so negative values follow
    /// the others, claims to be sorted.
    Unsigned,
    /// A dictionary sorted in descending order claims to be sorted.
    Reversed,
}

impl Fault {
    fn name(self) -> &'static str {
        match self {
            Self::Order => "order",
            Self::Unsorted => "dictionary_unsorted",
            Self::Ends => "ends",
            Self::Nulls => "null_placement",
            Self::Unsigned => "dictionary_unsigned",
            Self::Reversed => "dictionary_reversed",
        }
    }

    /// Forges page `index` of `column`; whether it changed anything.
    fn apply(self, column: &mut PagedColumn, index: usize) -> Result<bool, String> {
        let page = &column.pages()[index];
        let checked = page.column.page.invariants();
        let distinct = || {
            let mut values = page
                .column
                .truth
                .iter()
                .flatten()
                .copied()
                .collect::<Vec<_>>();
            values.sort_unstable();
            values.dedup();
            values
        };
        match self {
            Self::Order => {
                if checked.non_decreasing || checked.non_decreasing_non_null {
                    return Ok(false);
                }
                let nulls = page.non_null < page.rows();
                column.forge_descriptor(
                    index,
                    CheckedInvariants {
                        non_decreasing: !nulls,
                        non_decreasing_non_null: nulls,
                        ..checked
                    },
                )
            }
            Self::Nulls => {
                let null_placement = match checked.null_placement {
                    NullPlacement::NoNulls => return Ok(false),
                    NullPlacement::First => NullPlacement::Last,
                    NullPlacement::Last | NullPlacement::Arbitrary => NullPlacement::First,
                };
                column.forge_descriptor(
                    index,
                    CheckedInvariants {
                        null_placement,
                        ..checked
                    },
                )
            }
            Self::Ends => {
                let ends = page.ends.map(|(first, last)| {
                    let quarter = (last - first) / 4;
                    (first + quarter, last - quarter)
                });
                column.forge_ends(index, ends)
            }
            Self::Unsorted => {
                let mut table = Vec::new();
                let mut seen = std::collections::BTreeSet::new();
                for &value in page.column.truth.iter().flatten() {
                    if seen.insert(value) {
                        table.push(value);
                    }
                }
                column.forge_dictionary(index, &table)
            }
            Self::Unsigned => {
                let mut table = distinct();
                table.sort_by_key(|&value| value as u64);
                column.forge_dictionary(index, &table)
            }
            Self::Reversed => {
                let mut table = distinct();
                table.reverse();
                column.forge_dictionary(index, &table)
            }
        }
    }
}

const FAULTS: [Fault; 6] = [
    Fault::Order,
    Fault::Unsorted,
    Fault::Ends,
    Fault::Nulls,
    Fault::Unsigned,
    Fault::Reversed,
];

/// The fact planner, the planners that read statistics or a footer, and
/// the zone map with the encoding's shortcuts, which trusts the same
/// attestations the fact planner does.
const PLANNERS: [Planning; 4] = [
    Planning::Facts,
    Planning::StoredStatistics,
    Planning::StoredStatisticsPlus,
    Planning::DerivedFooter,
];

#[derive(Clone, Copy, Debug, Default)]
struct Tally {
    queries: usize,
    correct: usize,
    wrong: usize,
    stopped: usize,
    /// Queries over a column in which a forged attestation differs from the
    /// true one.
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
            entry.stopped += part.stopped;
            entry.affected += part.affected;
        }
    }
    let mut output = csv(format!("{result_dir}/attested_faults.csv"))?;
    writeln!(
        output,
        "fault,planner,queries,affected,correct,wrong,stopped"
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
            tally.stopped
        )?;
        if *planner == Planning::StoredStatistics.label() && (tally.wrong > 0 || tally.stopped > 0)
        {
            return Err("a forged attestation changed an answer planned from a zone map".into());
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
    for fault in FAULTS {
        let mut forged = selector.clone();
        let mut changed = false;
        for index in (FAULT_EVERY / 2..selector.pages().len()).step_by(FAULT_EVERY) {
            changed |= fault.apply(&mut forged, index)?;
        }
        // The footer holds the bounds the writer derives from its facts,
        // forged ones included.
        let mut footer = forged.clone();
        for index in 0..footer.pages().len() {
            let bound = paged::derived_bounds(&footer.pages()[index])?;
            footer.replace_statistics(index, bound)?;
        }
        for predicate in &predicates {
            let expected = truth(
                &selector_values.values,
                &value_values.values,
                predicate.low,
                predicate.high,
            );
            for planning in PLANNERS {
                let column = match planning {
                    Planning::DerivedFooter => &footer,
                    _ => &forged,
                };
                let entry: &mut Tally = tally.entry((fault, planning.label())).or_default();
                entry.queries += 1;
                entry.affected += usize::from(changed);
                match paged::sum_where(column, value, predicate.low, predicate.high, planning, 1) {
                    Ok((sum, rows, _)) if (sum, rows) == expected => entry.correct += 1,
                    Ok(_) => entry.wrong += 1,
                    Err(_) => entry.stopped += 1,
                }
            }
        }
    }
    Ok(tally)
}
