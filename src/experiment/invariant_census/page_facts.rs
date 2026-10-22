//! Facts per page under the paged writer. Each census column is cut into
//! pages of 1,024 and 16,384 rows, every page is encoded as the paged studies
//! encode their columns (the smallest of the direct recipes), and the facts
//! its encoding proves are recorded: whether they license an action an exact
//! zone map cannot (a binary search, a search block by block, a translation
//! of the predicate into dictionary codes), whether the page carries a
//! derived bound, and how much wider that bound is than the page's exact
//! range.

use std::error::Error;
use std::fs;

use crate::access_compiler::paged::{PagePolicy, PagedColumn, derived_bounds};
use crate::access_compiler::{NullPlacement, Recipe};
use crate::experiment::types::Column;

/// Page sizes the census reports per-page facts for.
pub const PAGE_SIZES: [usize; 2] = [1_024, 16_384];

/// The paged studies' direct menu: every recipe whose fields are directly
/// addressable.
fn direct_menu() -> Vec<Recipe> {
    let bitpack = || Box::new(Recipe::BitPack);
    vec![
        Recipe::For(bitpack()),
        Recipe::Dictionary(bitpack()),
        Recipe::Dictionary(Box::new(Recipe::Rle {
            index_interval: 32,
            values: bitpack(),
        })),
        Recipe::Rle {
            index_interval: 32,
            values: Box::new(Recipe::For(bitpack())),
        },
    ]
}

/// One page of one census column under the paged writer.
pub struct PageFactRow {
    pub column: usize,
    pub page_size: usize,
    pub page: usize,
    pub recipe: String,
    /// `all_null`, `constant` (one distinct non-null value), or `values`.
    pub state: &'static str,
    /// Order with nulls at one end or none: a binary search is licensed.
    pub search: bool,
    /// Order within every block of rows: a search per block is licensed.
    pub piecewise: bool,
    /// An order-preserving dictionary at the root: predicates translate into
    /// code ranges.
    pub translate: bool,
    /// The page carries a derived bound.
    pub bounded: bool,
    /// The derived bound equals the exact minimum and maximum.
    pub exact: bool,
    /// Derived bound width over exact width, for pages of `values`.
    pub width_ratio: Option<f64>,
}

/// The per-page facts of `column` at every size in `PAGE_SIZES`.
pub fn page_facts(id: usize, column: &Column) -> Result<Vec<PageFactRow>, String> {
    let policy = PagePolicy::smallest(direct_menu());
    let mut rows = Vec::new();
    for page_size in PAGE_SIZES {
        let paged = PagedColumn::from_values(&column.values, page_size, &policy)?;
        for (index, page) in paged.pages().iter().enumerate() {
            let state = match page.stats {
                None => "all_null",
                Some((low, high)) if low == high => "constant",
                Some(_) => "values",
            };
            let root = page.column.decoder.root();
            let search = page
                .facts
                .root_monotone_nulls(root)
                .is_some_and(|placement| placement != NullPlacement::Arbitrary);
            let piecewise = page.facts.root_piecewise_monotone_rows(root).is_some();
            let translate = page.translates();
            let bound = derived_bounds(page)?;
            let width_ratio = match (bound, page.stats) {
                (Some((bound_low, bound_high)), Some((low, high))) if low < high => {
                    Some((bound_high - bound_low + 1) as f64 / (high - low + 1) as f64)
                }
                _ => None,
            };
            rows.push(PageFactRow {
                column: id,
                page_size,
                page: index,
                recipe: page.column.recipe.name(),
                state,
                search,
                piecewise,
                translate,
                bounded: bound.is_some(),
                exact: bound.is_some() && bound == page.stats,
                width_ratio,
            });
        }
    }
    Ok(rows)
}

/// Writes `page_facts.csv`.
pub fn write(result_dir: &str, rows: &[PageFactRow]) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(result_dir)?;
    let mut writer = csv::Writer::from_path(format!("{result_dir}/page_facts.csv"))?;
    writer.write_record([
        "column",
        "page_size",
        "page",
        "recipe",
        "state",
        "search",
        "piecewise",
        "translate",
        "bounded",
        "exact",
        "width_ratio",
    ])?;
    for row in rows {
        writer.write_record([
            row.column.to_string(),
            row.page_size.to_string(),
            row.page.to_string(),
            row.recipe.clone(),
            row.state.to_string(),
            row.search.to_string(),
            row.piecewise.to_string(),
            row.translate.to_string(),
            row.bounded.to_string(),
            row.exact.to_string(),
            row.width_ratio
                .map(|ratio| format!("{ratio:.6}"))
                .unwrap_or_default(),
        ])?;
    }
    writer.flush()?;
    Ok(())
}
