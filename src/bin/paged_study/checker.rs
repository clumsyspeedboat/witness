//! Fault injection against the plan checker. Every page action the fact
//! planner chose is mutated as a planner bug or a stale plan would mutate
//! it, and each mutant goes to the checker. Every mutant, accepted or not,
//! is then executed and compared with the page's true answer, so the study
//! shows both that no accepted mutant answers wrongly and how many rejected
//! mutants would have.

use std::collections::BTreeMap;
use std::error::Error;
use std::io::Write;

use witness::access_compiler::paged::{self, Licence, Page, PageAction, Planning};
use witness::access_compiler::{
    Authorization, ClosureMode, NullPlacement, ReadSession, Span, push_range,
};
use witness::experiment::types::Column;

use crate::common::csv;
use crate::corpus::Pair;
use crate::measure::{Layouts, parallel};
use crate::workload::predicates;

/// Rows per block for piecewise-search mutants that borrow no block size.
const PIECEWISE_ROWS: usize = 256;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Family {
    /// The licence is kept and the action changed to another kind.
    Kind,
    /// The recorded bound is moved off the one the bytes hold.
    Evidence,
    /// A neighbouring page's action is applied to this page.
    ForeignPage,
    /// This page's action for another predicate is reused.
    StalePlan,
    /// A skip or accept cites stored statistics the page does not store.
    ForgedStatistics,
}

impl Family {
    fn name(self) -> &'static str {
        match self {
            Self::Kind => "kind",
            Self::Evidence => "evidence",
            Self::ForeignPage => "foreign_page",
            Self::StalePlan => "stale_plan",
            Self::ForgedStatistics => "forged_statistics",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Outcome {
    Correct,
    Wrong,
    Error,
}

impl Outcome {
    fn name(self) -> &'static str {
        match self {
            Self::Correct => "correct",
            Self::Wrong => "wrong",
            Self::Error => "error",
        }
    }
}

type Tally = BTreeMap<(Family, bool, Outcome), usize>;

pub fn run(
    result_dir: &str,
    columns: &[Column],
    pairs: &[Pair],
    layouts: &[Layouts],
) -> Result<(), Box<dyn Error>> {
    let tallies = parallel(pairs.len(), |index| {
        let pair = &pairs[index];
        mutate_pair(
            layouts[pair.selector].direct.pages(),
            &columns[pair.selector],
        )
    })?;
    let mut total = Tally::new();
    for tally in tallies {
        for (key, count) in tally {
            *total.entry(key).or_default() += count;
        }
    }
    let mut output = csv(format!("{result_dir}/checker.csv"))?;
    writeln!(output, "family,checker,outcome,mutants")?;
    for ((family, accepted, outcome), count) in &total {
        writeln!(
            output,
            "{},{},{},{count}",
            family.name(),
            if *accepted { "accepted" } else { "rejected" },
            outcome.name()
        )?;
    }
    output.flush()?;
    let unsound = total
        .iter()
        .filter(|((_, accepted, outcome), _)| *accepted && *outcome != Outcome::Correct)
        .map(|(_, count)| count)
        .sum::<usize>();
    if unsound > 0 {
        return Err(format!("the checker accepted {unsound} mutants that answer wrongly").into());
    }
    Ok(())
}

fn mutate_pair(pages: &[Page], selector: &Column) -> Result<Tally, String> {
    let predicates = predicates(&selector.values).map_err(|error| error.to_string())?;
    // plans[predicate][page]: the checked fact plan.
    let mut plans = Vec::with_capacity(predicates.len());
    for predicate in &predicates {
        let mut row = Vec::with_capacity(pages.len());
        for page in pages {
            let mut session = ReadSession::new_untracked(&page.column.page, ClosureMode::Selective);
            row.push(paged::plan_between(
                page,
                &mut session,
                predicate.low,
                predicate.high,
                Planning::Facts,
            )?);
        }
        plans.push(row);
    }
    let mut tally = Tally::new();
    for (query, predicate) in predicates.iter().enumerate() {
        let (low, high) = (predicate.low, predicate.high);
        for (index, page) in pages.iter().enumerate() {
            let original = &plans[query][index];
            let mut mutants = Vec::new();
            if let Some(licence) = licence_of(original) {
                for action in kinds(licence, original) {
                    mutants.push((Family::Kind, action));
                }
                if let Some((lower, upper)) = licence.evidence {
                    for evidence in [
                        Some((lower - 1, upper)),
                        Some((lower + 1, upper)),
                        Some((lower, upper - 1)),
                        Some((lower, upper + 1)),
                        None,
                    ] {
                        let moved = Licence {
                            evidence,
                            ..licence.clone()
                        };
                        mutants.push((Family::Evidence, with_licence(original, moved)));
                    }
                }
            }
            for neighbour in [index.wrapping_sub(1), index + 1] {
                if let Some(action) = plans[query].get(neighbour) {
                    mutants.push((Family::ForeignPage, action.clone()));
                }
            }
            for (other, row) in plans.iter().enumerate() {
                if other != query {
                    mutants.push((Family::StalePlan, row[index].clone()));
                }
            }
            let forged = pages
                .get(index + 1)
                .or_else(|| index.checked_sub(1).and_then(|before| pages.get(before)))
                .and_then(|other| other.stats)
                .filter(|stats| Some(*stats) != page.stats);
            for evidence in [forged, Some((i128::from(low), i128::from(high)))] {
                if evidence == page.stats {
                    continue;
                }
                let licence = Licence {
                    fact: Authorization::StoredStatistics,
                    evidence,
                };
                mutants.push((Family::ForgedStatistics, PageAction::Skip(licence.clone())));
                mutants.push((Family::ForgedStatistics, PageAction::Accept(licence)));
            }

            let truth = page_truth(page, low, high);
            let mut session = ReadSession::new_untracked(&page.column.page, ClosureMode::Selective);
            for (family, mutant) in mutants {
                if &mutant == original {
                    continue;
                }
                let accepted = paged::check_between(page, &mut session, &mutant, low, high).is_ok();
                let outcome = match paged::execute_between(page, &mut session, &mutant, low, high) {
                    Ok(spans) if spans == truth => Outcome::Correct,
                    Ok(_) => Outcome::Wrong,
                    Err(_) => Outcome::Error,
                };
                *tally.entry((family, accepted, outcome)).or_default() += 1;
            }
        }
    }
    Ok(tally)
}

fn licence_of(action: &PageAction) -> Option<&Licence> {
    match action {
        PageAction::Skip(licence)
        | PageAction::Accept(licence)
        | PageAction::Translate(licence)
        | PageAction::Search { licence, .. }
        | PageAction::SearchPiecewise { licence, .. } => Some(licence),
        PageAction::Scan => None,
    }
}

fn with_licence(action: &PageAction, licence: Licence) -> PageAction {
    match action {
        PageAction::Skip(_) => PageAction::Skip(licence),
        PageAction::Accept(_) => PageAction::Accept(licence),
        PageAction::Translate(_) => PageAction::Translate(licence),
        PageAction::Search { nulls, .. } => PageAction::Search {
            licence,
            nulls: *nulls,
        },
        PageAction::SearchPiecewise { max_rows, .. } => PageAction::SearchPiecewise {
            licence,
            max_rows: *max_rows,
        },
        PageAction::Scan => PageAction::Scan,
    }
}

/// Every licensed action kind carrying `licence`, with the variants a
/// search can take: each null placement and a halved or doubled block.
fn kinds(licence: &Licence, original: &PageAction) -> Vec<PageAction> {
    let max_rows = match original {
        PageAction::SearchPiecewise { max_rows, .. } => *max_rows,
        _ => PIECEWISE_ROWS,
    };
    let mut actions = vec![
        PageAction::Skip(licence.clone()),
        PageAction::Accept(licence.clone()),
        PageAction::Translate(licence.clone()),
    ];
    for nulls in [
        NullPlacement::NoNulls,
        NullPlacement::First,
        NullPlacement::Last,
        NullPlacement::Arbitrary,
    ] {
        actions.push(PageAction::Search {
            licence: licence.clone(),
            nulls,
        });
    }
    for rows in [max_rows, max_rows / 2, max_rows * 2] {
        if rows > 0 {
            actions.push(PageAction::SearchPiecewise {
                licence: licence.clone(),
                max_rows: rows,
            });
        }
    }
    actions
}

fn page_truth(page: &Page, low: i64, high: i64) -> Vec<Span> {
    let mut spans = Vec::new();
    for (row, value) in page.column.truth.iter().enumerate() {
        if value.is_some_and(|value| low <= value && value <= high) {
            push_range(&mut spans, row, row + 1);
        }
    }
    spans
}
