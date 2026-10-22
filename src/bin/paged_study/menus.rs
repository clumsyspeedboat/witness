//! What the page menu decides about facts. The timed study's direct menu
//! holds four directly addressable recipes, none of which proves order from
//! its bytes: every order its planner uses is attested by the writer. This
//! study encodes the same columns under that menu and under a byte-optimised
//! menu that adds delta chains, a patch, and Zstd frames, then plans every
//! paged query under the fact planner and the zone map, without timing. Per
//! pair and menu it records the compositions chosen, the selector pages
//! that prove a bound and how their order is known, the page actions each
//! planner took, and the bytes stored, next to Zstd Parquet. Every answer is
//! compared with brute force.

use std::collections::BTreeMap;
use std::error::Error;
use std::io::Write;

use witness::access_compiler::paged::{self, Page, PageCounts, PagePolicy, PagedColumn, Planning};
use witness::access_compiler::{Evidence, FactScope, Property, ValueInvariant};
use witness::experiment::time_window::parquet_table_file;
use witness::experiment::types::Column;

use crate::common::{byte_menu, csv, direct_menu, quote, truth};
use crate::corpus::Pair;
use crate::measure::parallel;
use crate::workload::predicates;

const MENUS: [&str; 2] = ["direct", "byte"];
const PLANNERS: [Planning; 2] = [Planning::Facts, Planning::StoredStatistics];

/// How a selector page knows its order, if it does.
#[derive(Clone, Copy, Debug, Default)]
struct OrderShares {
    bounded: usize,
    /// Non-decreasing, proved from the encoded bytes.
    structural: usize,
    /// Non-decreasing within restart blocks, proved from the bytes, and not
    /// known to be non-decreasing throughout.
    piecewise: usize,
    /// Non-decreasing on the writer's word alone.
    attested: usize,
}

pub fn run(
    result_dir: &str,
    columns: &[Column],
    pairs: &[Pair],
    page_rows: usize,
) -> Result<(), Box<dyn Error>> {
    let policies = [
        PagePolicy::smallest(direct_menu()),
        PagePolicy::smallest(byte_menu(page_rows)),
    ];
    let layouts = parallel(columns.len(), |index| {
        policies
            .iter()
            .map(|policy| PagedColumn::from_values(&columns[index].values, page_rows, policy))
            .collect::<Result<Vec<_>, _>>()
    })?;
    let rows = parallel(pairs.len(), |pair_id| {
        let pair = &pairs[pair_id];
        let (selector, value) = (&columns[pair.selector].values, &columns[pair.value].values);
        let parquet_zstd = parquet_table_file(selector, value, page_rows, true)?.len();
        let predicates = predicates(selector)?;
        let mut rows = Vec::with_capacity(MENUS.len());
        for (menu, name) in MENUS.iter().enumerate() {
            let (selector_pages, value_pages) =
                (&layouts[pair.selector][menu], &layouts[pair.value][menu]);
            let mut counts = [PageCounts::default(); PLANNERS.len()];
            for predicate in &predicates {
                let expected = truth(selector, value, predicate.low, predicate.high);
                for (planning, total) in PLANNERS.into_iter().zip(&mut counts) {
                    let (sum, matched, page_counts) = paged::sum_where(
                        selector_pages,
                        value_pages,
                        predicate.low,
                        predicate.high,
                        planning,
                        1,
                    )?;
                    if (sum, matched) != expected {
                        return Err(format!(
                            "{name} menu under {} returned {:?} for pair {pair_id} {}, expected {expected:?}",
                            planning.label(),
                            (sum, matched),
                            predicate.label
                        ));
                    }
                    add(total, page_counts);
                }
            }
            let shares = selector_pages
                .pages()
                .iter()
                .fold(OrderShares::default(), |shares, page| shares.add(page));
            let [facts, zonemap] = counts;
            rows.push(format!(
                "{pair_id},{name},{},{},{},{},{parquet_zstd},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
                selector_pages.pages().len(),
                selector_pages.bytes(),
                value_pages.bytes(),
                selector_pages.bytes() + value_pages.bytes(),
                shares.bounded,
                shares.structural,
                shares.piecewise,
                shares.attested,
                predicates.len(),
                facts.skipped,
                facts.accepted,
                facts.searched,
                facts.translated,
                facts.scanned,
                zonemap.skipped,
                zonemap.accepted,
                zonemap.scanned,
                quote(&recipes(selector_pages)),
                quote(&recipes(value_pages)),
            ));
        }
        Ok(rows)
    })?;
    let mut output = csv(format!("{result_dir}/menus.csv"))?;
    writeln!(
        output,
        "pair,menu,pages,selector_bytes,value_bytes,bytes,parquet_zstd_bytes,bounded_pages,structural_order_pages,piecewise_order_pages,attested_order_pages,queries,facts_skipped,facts_accepted,facts_searched,facts_translated,facts_scanned,zonemap_skipped,zonemap_accepted,zonemap_scanned,selector_recipes,value_recipes"
    )?;
    for row in rows.into_iter().flatten() {
        writeln!(output, "{row}")?;
    }
    output.flush()?;
    Ok(())
}

impl OrderShares {
    /// Adds a selector page: whether its root proves a bound, and how it
    /// knows its order: from its bytes, from the writer, or, failing both,
    /// within restart blocks from its bytes.
    fn add(mut self, page: &Page) -> Self {
        let root = page.column.decoder.root();
        let at_root = || page.facts.iter().filter(move |fact| fact.node == root);
        if at_root()
            .any(|fact| matches!(fact.property, Property::Value(ValueInvariant::Bounded(_))))
        {
            self.bounded += 1;
        }
        let order = |structural: bool| {
            at_root().any(|fact| {
                fact.property == Property::Value(ValueInvariant::NonDecreasing)
                    && fact.scope != FactScope::Mapping
                    && matches!(fact.evidence, Evidence::Structural(_)) == structural
            })
        };
        if order(true) {
            self.structural += 1;
        } else if order(false) {
            self.attested += 1;
        } else if at_root().any(|fact| {
            matches!(
                fact.property,
                Property::Value(ValueInvariant::PiecewiseNonDecreasing { .. })
            )
        }) {
            self.piecewise += 1;
        }
        self
    }
}

fn add(total: &mut PageCounts, counts: PageCounts) {
    total.skipped += counts.skipped;
    total.accepted += counts.accepted;
    total.searched += counts.searched;
    total.translated += counts.translated;
    total.scanned += counts.scanned;
}

fn recipes(column: &PagedColumn) -> String {
    let mut recipes = BTreeMap::<String, usize>::new();
    for page in column.pages() {
        *recipes.entry(page.column.recipe.name()).or_default() += 1;
    }
    recipes
        .into_iter()
        .map(|(name, count)| format!("{name}:{count}"))
        .collect::<Vec<_>>()
        .join(";")
}
