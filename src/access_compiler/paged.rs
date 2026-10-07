//! Columns as sequences of independently encoded pages.
//!
//! Each page chooses its own encoding and so proves its own facts. A query is
//! planned page by page: every page takes the cheapest action its facts
//! license, and the action names the fact it relies on, together with any
//! evidence it read, so `check_action` can confirm it before it runs.
//!
//! Pages make three things local that a single-page column cannot express:
//! a fact that holds on some pages but not the column, a column that grows
//! by appending pages, and work that runs on pages in parallel.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{
    Authorization, ClosureMode, DecoderNode, EncodedColumn, FactScope, InputColumn, InvariantSet,
    MappingInvariant, NullPlacement, Property, ReadSession, Recipe, Span, ValueInvariant,
    bound_interval, derive_invariants, encode, push_range, vector,
};

/// One page of a paged column.
#[derive(Clone, Debug)]
pub struct Page {
    /// First column row the page holds.
    pub start: usize,
    pub column: EncodedColumn,
    pub facts: InvariantSet,
    /// First and last value of a dense page that proves itself
    /// non-decreasing, read when the page was sealed.
    pub ends: Option<(i128, i128)>,
    /// Exact minimum and maximum of the non-null values, as a writer would
    /// store them in a zone map; `None` when the page holds no value.
    pub stats: Option<(i128, i128)>,
    /// Non-null rows of the page, stored with the statistics.
    pub non_null: usize,
}

impl Page {
    pub fn rows(&self) -> usize {
        self.column.truth.len()
    }

    fn root(&self) -> super::NodeId {
        self.column.decoder.root()
    }
}

/// How a page's encoding is chosen: the smallest successful encoding from a
/// menu of dense recipes. Pages with nulls wrap each recipe in `Nullable`,
/// and an all-null page falls back to a nullable bit-packed stream.
#[derive(Clone, Debug)]
pub struct PagePolicy {
    pub menu: Vec<Recipe>,
    pub rank_interval: usize,
}

impl PagePolicy {
    pub fn smallest(menu: Vec<Recipe>) -> Self {
        Self {
            menu,
            rank_interval: 256,
        }
    }

    pub fn encode(&self, values: &[Option<i64>]) -> Result<EncodedColumn, String> {
        let nullable = values.iter().any(Option::is_none);
        let wrap = |recipe: &Recipe| {
            if nullable {
                Recipe::Nullable {
                    rank_interval: self.rank_interval,
                    values: Box::new(recipe.clone()),
                }
            } else {
                recipe.clone()
            }
        };
        let mut candidates = self.menu.iter().map(wrap).collect::<Vec<_>>();
        if nullable {
            candidates.push(Recipe::Nullable {
                rank_interval: self.rank_interval,
                values: Box::new(Recipe::BitPack),
            });
        }
        let mut best: Option<EncodedColumn> = None;
        for recipe in candidates {
            let Ok(encoded) = encode(&recipe, InputColumn::nullable(values.to_vec())) else {
                continue;
            };
            if best
                .as_ref()
                .is_none_or(|best| encoded.page.bytes().len() < best.page.bytes().len())
            {
                best = Some(encoded);
            }
        }
        best.ok_or_else(|| "no recipe in the page menu encodes this page".into())
    }
}

/// A column stored as pages of at most `page_rows` rows.
#[derive(Clone, Debug)]
pub struct PagedColumn {
    pages: Vec<Page>,
    page_rows: usize,
    rows: usize,
    /// Leading pages that are each non-decreasing and whose seams never
    /// decrease: the column's checked order certificate, kept by `append`.
    ordered_pages: usize,
}

impl PagedColumn {
    pub fn new(page_rows: usize) -> Result<Self, String> {
        if page_rows == 0 {
            return Err("pages must hold at least one row".into());
        }
        Ok(Self {
            pages: Vec::new(),
            page_rows,
            rows: 0,
            ordered_pages: 0,
        })
    }

    pub fn from_values(
        values: &[Option<i64>],
        page_rows: usize,
        policy: &PagePolicy,
    ) -> Result<Self, String> {
        let mut column = Self::new(page_rows)?;
        column.append(values, policy)?;
        Ok(column)
    }

    /// Appends `values` as new sealed pages. Existing pages, and the facts
    /// they prove, are untouched, so appending never invalidates a plan made
    /// for an earlier page. Order across pages is maintained in O(1) per
    /// page: a new page extends the ordered prefix when it proves itself
    /// non-decreasing and starts no lower than the previous page ends.
    pub fn append(&mut self, values: &[Option<i64>], policy: &PagePolicy) -> Result<(), String> {
        for chunk in values.chunks(self.page_rows) {
            let column = policy.encode(chunk)?;
            let facts = derive_invariants(
                &column.decoder,
                column.page.layout(),
                column.page.invariants(),
            )?;
            let ends = if facts.contains(
                column.decoder.root(),
                FactScope::AllRows,
                Property::Value(ValueInvariant::NonDecreasing),
            ) {
                let mut session = ReadSession::new_untracked(&column.page, ClosureMode::Selective);
                let first = vector::get(&column.decoder, &mut session, 0)?;
                let last = vector::get(&column.decoder, &mut session, chunk.len() - 1)?;
                first.zip(last)
            } else {
                None
            };
            let extends = self.ordered_pages == self.pages.len()
                && ends.is_some_and(|(first, _)| {
                    self.pages
                        .last()
                        .is_none_or(|previous| previous.ends.is_some_and(|(_, last)| last <= first))
                });
            if extends {
                self.ordered_pages += 1;
            }
            let present = chunk.iter().flatten().map(|&value| i128::from(value));
            let stats = present.clone().min().zip(present.clone().max());
            let non_null = present.count();
            self.pages.push(Page {
                start: self.rows,
                column,
                facts,
                ends,
                stats,
                non_null,
            });
            self.rows += chunk.len();
        }
        Ok(())
    }

    /// Whether every page is non-decreasing and no seam decreases, so the
    /// whole column is non-decreasing.
    pub fn ordered(&self) -> bool {
        !self.pages.is_empty() && self.ordered_pages == self.pages.len()
    }

    /// Pages that can hold a value in `[low, high]` when the column is
    /// ordered: everything before the first page ending at or above `low`
    /// and after the last page starting at or below `high` holds none. The
    /// pages just outside the range are re-read to confirm the boundary.
    fn candidate_pages(&self, low: i128, high: i128) -> Result<Option<(usize, usize)>, String> {
        if !self.ordered() {
            return Ok(None);
        }
        // `append` extends the ordered prefix only with pages that have ends,
        // and seams never decrease, so both page ends are sorted.
        let first = self
            .pages
            .partition_point(|page| page.ends.is_some_and(|(_, last)| last < low));
        let end = self
            .pages
            .partition_point(|page| page.ends.is_some_and(|(start, _)| start <= high));
        let confirm = |index: usize, row_of: fn(&Page) -> usize| -> Result<i128, String> {
            let page = &self.pages[index];
            let mut session = ReadSession::new_untracked(&page.column.page, ClosureMode::Selective);
            vector::get(&page.column.decoder, &mut session, row_of(page))?
                .ok_or_else(|| "ordered page holds a null".into())
        };
        if first > 0 && confirm(first - 1, |page| page.rows() - 1)? >= low {
            return Err("column order certificate disagrees with its pages".into());
        }
        if end < self.pages.len() && confirm(end, |_| 0)? <= high {
            return Err("column order certificate disagrees with its pages".into());
        }
        Ok(Some((first, end.max(first))))
    }

    pub fn pages(&self) -> &[Page] {
        &self.pages
    }

    /// Overwrites one page's stored statistics, as a writer whose zone map
    /// fell out of step with its data would. The page's facts are untouched:
    /// they are derived from the bytes that decode.
    pub fn replace_statistics(
        &mut self,
        page: usize,
        stats: Option<(i128, i128)>,
    ) -> Result<(), String> {
        self.pages.get_mut(page).ok_or("no such page")?.stats = stats;
        Ok(())
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn page_rows(&self) -> usize {
        self.page_rows
    }

    pub fn bytes(&self) -> usize {
        self.pages
            .iter()
            .map(|page| page.column.page.bytes().len())
            .sum()
    }
}

/// The licence for a page action: the fact it cites and, for a bound, the
/// interval read from the page when the action was planned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Licence {
    pub fact: Authorization,
    pub evidence: Option<(i128, i128)>,
}

/// What a BETWEEN predicate does on one page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PageAction {
    /// No row can match, so the page is not read.
    Skip(Licence),
    /// Every non-null row matches, so no value is decoded.
    Accept(Licence),
    /// Two boundary searches find the matching rows.
    Search {
        licence: Licence,
        nulls: NullPlacement,
    },
    /// A boundary search inside every block of `max_rows`.
    SearchPiecewise { licence: Licence, max_rows: usize },
    /// The value range becomes a code range; only codes are compared.
    Translate(Licence),
    /// Every row is decoded and compared: the action needing no licence.
    Scan,
}

/// What SUM does with the selected rows of one value page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SumAction {
    /// The page holds one value, so the sum is that value times the rows.
    Constant(Licence),
    Decode,
}

/// Whether the planner may use page facts at all; `ScanOnly` is the
/// ablation that turns every page into a scan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Planning {
    Facts,
    ScanOnly,
    /// A plain zone map: exact per-page minima and maxima stored by the
    /// writer, used to skip and accept pages and nothing else. It isolates
    /// what stored statistics buy on the same executor as derived facts.
    StoredStatistics,
}

/// Plans `low <= value <= high` on one page, then checks the plan.
pub fn plan_between(
    page: &Page,
    session: &mut ReadSession<'_>,
    low: i64,
    high: i64,
    planning: Planning,
) -> Result<PageAction, String> {
    if low > high {
        return Err("page predicate bounds are invalid".into());
    }
    let action = match planning {
        Planning::ScanOnly => PageAction::Scan,
        Planning::Facts => choose_between(page, session, i128::from(low), i128::from(high))?,
        Planning::StoredStatistics => choose_by_statistics(page, i128::from(low), i128::from(high)),
    };
    check_between(page, session, &action, low, high)?;
    Ok(action)
}

fn choose_by_statistics(page: &Page, low: i128, high: i128) -> PageAction {
    let licence = Licence {
        fact: Authorization::StoredStatistics,
        evidence: page.stats,
    };
    match page.stats {
        None => PageAction::Skip(licence),
        Some((lower, upper)) if upper < low || lower > high => PageAction::Skip(licence),
        Some((lower, upper)) if low <= lower && upper <= high => PageAction::Accept(licence),
        Some(_) => PageAction::Scan,
    }
}

/// Confirms a licence that cites stored statistics: they are the ones the
/// page stores, and they decide the action. Stored statistics cannot be
/// checked against the encoding, so a page whose statistics are wrong is
/// planned wrongly; that is the trust they ask for.
fn check_statistics(
    page: &Page,
    licence: &Licence,
    low: i128,
    high: i128,
    accept: bool,
) -> Result<(), String> {
    if licence.evidence != page.stats {
        return Err("stored-statistics licence disagrees with the page".into());
    }
    match (page.stats, accept) {
        (None, false) => Ok(()),
        (Some((lower, upper)), false) if upper < low || lower > high => Ok(()),
        (Some((lower, upper)), true) if low <= lower && upper <= high => Ok(()),
        _ => Err("stored statistics do not decide this action".into()),
    }
}

fn choose_between(
    page: &Page,
    session: &mut ReadSession<'_>,
    low: i128,
    high: i128,
) -> Result<PageAction, String> {
    let root = page.root();
    if page.facts.contains(
        root,
        FactScope::AllRows,
        Property::Value(ValueInvariant::AllNull),
    ) {
        return Ok(PageAction::Skip(cite(
            root,
            FactScope::AllRows,
            ValueInvariant::AllNull,
            None,
        )));
    }
    for (scope, source) in root_bounds(page) {
        let (lower, upper) = bound_interval(session, source)?;
        let licence = cite(
            root,
            scope,
            ValueInvariant::Bounded(source),
            Some((lower, upper)),
        );
        if upper < low || lower > high {
            return Ok(PageAction::Skip(licence));
        }
        if low <= lower && upper <= high {
            return Ok(PageAction::Accept(licence));
        }
    }
    if let Some(nulls) = page.facts.root_monotone_nulls(root)
        && nulls != NullPlacement::Arbitrary
    {
        let scope = if nulls == NullPlacement::NoNulls {
            FactScope::AllRows
        } else {
            FactScope::NonNullRows
        };
        return Ok(PageAction::Search {
            licence: cite(root, scope, ValueInvariant::NonDecreasing, None),
            nulls,
        });
    }
    if let Some(max_rows) = page.facts.root_piecewise_monotone_rows(root) {
        return Ok(PageAction::SearchPiecewise {
            licence: cite(
                root,
                FactScope::AllRows,
                ValueInvariant::PiecewiseNonDecreasing { max_rows },
                None,
            ),
            max_rows,
        });
    }
    if sorted_dictionary_root(page)
        && page.facts.contains(
            root,
            FactScope::Mapping,
            Property::Mapping(MappingInvariant::OrderPreserving),
        )
    {
        return Ok(PageAction::Translate(Licence {
            fact: Authorization::Fact {
                node: root,
                scope: FactScope::Mapping,
                property: Property::Mapping(MappingInvariant::OrderPreserving),
            },
            evidence: None,
        }));
    }
    Ok(PageAction::Scan)
}

/// Confirms that `action` is licensed for `low <= value <= high`: the cited
/// fact is one the page proves, it entails the action, and any bound it
/// relies on, re-read from the page, still decides the predicate.
pub fn check_between(
    page: &Page,
    session: &mut ReadSession<'_>,
    action: &PageAction,
    low: i64,
    high: i64,
) -> Result<(), String> {
    if low > high {
        return Err("BETWEEN needs low <= high".into());
    }
    let (low, high) = (i128::from(low), i128::from(high));
    match action {
        PageAction::Scan => Ok(()),
        PageAction::Skip(licence) | PageAction::Accept(licence)
            if licence.fact == Authorization::StoredStatistics =>
        {
            check_statistics(
                page,
                licence,
                low,
                high,
                matches!(action, PageAction::Accept(_)),
            )
        }
        PageAction::Skip(licence) => match cited(page, licence)? {
            (FactScope::AllRows, ValueInvariant::AllNull) => Ok(()),
            (_, ValueInvariant::Bounded(source)) => {
                let (lower, upper) = reread(session, source, licence)?;
                if upper < low || lower > high {
                    Ok(())
                } else {
                    Err("page skip cites a bound that overlaps the predicate".into())
                }
            }
            _ => Err("page skip cites a fact that cannot exclude rows".into()),
        },
        PageAction::Accept(licence) => match cited(page, licence)? {
            (_, ValueInvariant::Bounded(source)) => {
                let (lower, upper) = reread(session, source, licence)?;
                if low <= lower && upper <= high {
                    Ok(())
                } else {
                    Err("page accept cites a bound the predicate does not contain".into())
                }
            }
            _ => Err("page accept cites a fact that cannot admit rows".into()),
        },
        PageAction::Search { licence, nulls } => match cited(page, licence)? {
            (FactScope::AllRows, ValueInvariant::NonDecreasing)
                if *nulls == NullPlacement::NoNulls =>
            {
                Ok(())
            }
            (FactScope::NonNullRows, ValueInvariant::NonDecreasing)
                if *nulls != NullPlacement::Arbitrary
                    && page.facts.contains(
                        page.root(),
                        FactScope::AllRows,
                        Property::Value(ValueInvariant::NullPlacement(*nulls)),
                    ) =>
            {
                Ok(())
            }
            _ => Err("page search cites no order fact for its null placement".into()),
        },
        PageAction::SearchPiecewise { licence, max_rows } => match cited(page, licence)? {
            (FactScope::AllRows, ValueInvariant::PiecewiseNonDecreasing { max_rows: proven })
                if proven == *max_rows =>
            {
                Ok(())
            }
            _ => Err("piecewise search cites no matching piecewise order".into()),
        },
        PageAction::Translate(licence) => match licence.fact {
            Authorization::Fact {
                node,
                scope: FactScope::Mapping,
                property: Property::Mapping(MappingInvariant::OrderPreserving),
            } if node == page.root()
                && sorted_dictionary_root(page)
                && page.facts.contains(
                    node,
                    FactScope::Mapping,
                    Property::Mapping(MappingInvariant::OrderPreserving),
                ) =>
            {
                Ok(())
            }
            _ => Err("translation cites no order-preserving dictionary".into()),
        },
    }
}

/// Matching rows of one page, as page-local spans.
pub fn execute_between(
    page: &Page,
    session: &mut ReadSession<'_>,
    action: &PageAction,
    low: i64,
    high: i64,
) -> Result<Vec<Span>, String> {
    let decoder = &page.column.decoder;
    let all = Span::new(0, page.rows())?;
    match action {
        PageAction::Skip(_) => Ok(Vec::new()),
        PageAction::Accept(_) => vector::non_null(decoder, session, all),
        PageAction::Search { nulls, .. } => {
            vector::search_monotone(decoder, session, all, low, high, *nulls)
        }
        PageAction::SearchPiecewise { max_rows, .. } => {
            vector::search_piecewise(decoder, session, all, low, high, *max_rows)
        }
        PageAction::Translate(_) => vector::dictionary_range(decoder, session, all, low, high),
        PageAction::Scan => vector::between(decoder, session, all, low, high),
    }
}

/// Plans SUM over selected rows of a value page, then checks the plan. A
/// dense page whose bound has equal ends holds a single value.
pub fn plan_sum(
    page: &Page,
    session: &mut ReadSession<'_>,
    planning: Planning,
) -> Result<SumAction, String> {
    if planning == Planning::StoredStatistics
        && let Some((lower, upper)) = page.stats
        && lower == upper
        && page.non_null == page.rows()
    {
        let action = SumAction::Constant(Licence {
            fact: Authorization::StoredStatistics,
            evidence: page.stats,
        });
        check_sum(page, session, &action)?;
        return Ok(action);
    }
    if planning == Planning::Facts {
        for (scope, source) in root_bounds(page) {
            if scope != FactScope::AllRows {
                continue;
            }
            let (lower, upper) = bound_interval(session, source)?;
            if lower == upper {
                let action = SumAction::Constant(cite(
                    page.root(),
                    scope,
                    ValueInvariant::Bounded(source),
                    Some((lower, upper)),
                ));
                check_sum(page, session, &action)?;
                return Ok(action);
            }
        }
    }
    Ok(SumAction::Decode)
}

pub fn check_sum(
    page: &Page,
    session: &mut ReadSession<'_>,
    action: &SumAction,
) -> Result<(), String> {
    match action {
        SumAction::Decode => Ok(()),
        SumAction::Constant(licence) if licence.fact == Authorization::StoredStatistics => {
            match page.stats {
                Some((lower, upper))
                    if licence.evidence == page.stats
                        && lower == upper
                        && page.non_null == page.rows() =>
                {
                    Ok(())
                }
                _ => Err("constant SUM cites statistics that do not fix one value".into()),
            }
        }
        SumAction::Constant(licence) => match cited(page, licence)? {
            (FactScope::AllRows, ValueInvariant::Bounded(source)) => {
                let (lower, upper) = reread(session, source, licence)?;
                if lower == upper {
                    Ok(())
                } else {
                    Err("constant SUM cites a bound with distinct ends".into())
                }
            }
            _ => Err("constant SUM needs a dense bound".into()),
        },
    }
}

pub fn execute_sum(
    page: &Page,
    session: &mut ReadSession<'_>,
    action: &SumAction,
    ranges: &[Span],
) -> Result<i128, String> {
    match action {
        SumAction::Decode => vector::sum_ranges(&page.column.decoder, session, ranges),
        SumAction::Constant(licence) => {
            vector::validate_ranges(&page.column.decoder, ranges)?;
            let (value, _) = licence.evidence.ok_or("constant SUM lost its evidence")?;
            let rows = ranges.iter().map(|range| range.len() as i128).sum::<i128>();
            value
                .checked_mul(rows)
                .ok_or_else(|| "constant SUM overflow".into())
        }
    }
}

/// Matching rows of one page as a selection, for actions that decode every
/// row anyway: one bit per row however scattered the matches, where search
/// and acceptance yield a few contiguous spans.
pub fn execute_between_selection(
    page: &Page,
    session: &mut ReadSession<'_>,
    action: &PageAction,
    low: i64,
    high: i64,
) -> Result<vector::Selection, String> {
    let decoder = &page.column.decoder;
    let all = Span::new(0, page.rows())?;
    match action {
        PageAction::Translate(_) => vector::dictionary_selection(decoder, session, all, low, high),
        PageAction::Scan => vector::between_selection(decoder, session, all, low, high),
        _ => {
            let mut selection = vector::Selection::empty(all);
            for range in execute_between(page, session, action, low, high)? {
                selection.set_range(range.start, range.end);
            }
            Ok(selection)
        }
    }
}

/// SUM over the selected rows of a value page, in one pass.
pub fn execute_sum_selection(
    page: &Page,
    session: &mut ReadSession<'_>,
    action: &SumAction,
    selection: &vector::Selection,
) -> Result<i128, String> {
    match action {
        SumAction::Decode => vector::sum_selection(&page.column.decoder, session, selection),
        SumAction::Constant(licence) => {
            vector::validate_selection(&page.column.decoder, selection)?;
            let (value, _) = licence.evidence.ok_or("constant SUM lost its evidence")?;
            value
                .checked_mul(selection.count() as i128)
                .ok_or_else(|| "constant SUM overflow".into())
        }
    }
}

/// How the pages of one query were answered.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PageCounts {
    pub skipped: usize,
    pub accepted: usize,
    pub searched: usize,
    pub translated: usize,
    pub scanned: usize,
    pub constant_sums: usize,
}

impl PageCounts {
    fn record(&mut self, action: &PageAction) {
        match action {
            PageAction::Skip(_) => self.skipped += 1,
            PageAction::Accept(_) => self.accepted += 1,
            PageAction::Search { .. } | PageAction::SearchPiecewise { .. } => self.searched += 1,
            PageAction::Translate(_) => self.translated += 1,
            PageAction::Scan => self.scanned += 1,
        }
    }

    fn add(&mut self, other: &Self) {
        self.skipped += other.skipped;
        self.accepted += other.accepted;
        self.searched += other.searched;
        self.translated += other.translated;
        self.scanned += other.scanned;
        self.constant_sums += other.constant_sums;
    }
}

/// `SELECT SUM(value) WHERE low <= selector <= high` over two columns paged
/// alike, with the matching selector rows. Pages are planned and executed on
/// up to `threads` threads; results are combined in page order.
pub fn sum_where(
    selector: &PagedColumn,
    value: &PagedColumn,
    low: i64,
    high: i64,
    planning: Planning,
    threads: usize,
) -> Result<(i128, usize, PageCounts), String> {
    if low > high {
        return Err("BETWEEN needs low <= high".into());
    }
    if selector.rows != value.rows
        || selector.page_rows != value.page_rows
        || selector.pages.len() != value.pages.len()
    {
        return Err("SUM WHERE needs two columns paged alike".into());
    }
    // Planning reads a few bytes per page, so every page is planned first,
    // on one thread; only pages left with work are spread over threads, and
    // a query whose pages are nearly all skipped never pays to start any.
    let mut counts = PageCounts::default();
    let mut work = Vec::new();
    // On a column ordered across pages, only a contiguous run of pages can
    // match; every page outside it is skipped without being planned.
    let (first, end) = match planning {
        Planning::Facts => selector
            .candidate_pages(i128::from(low), i128::from(high))?
            .unwrap_or((0, selector.pages.len())),
        // A zone map consults every page's stored entry; the order
        // certificate is a derived fact it does not have.
        Planning::ScanOnly | Planning::StoredStatistics => (0, selector.pages.len()),
    };
    counts.skipped += selector.pages.len() - (end - first);
    if selector
        .pages
        .iter()
        .zip(&value.pages)
        .any(|(left, right)| left.start != right.start || left.rows() != right.rows())
    {
        return Err("SUM WHERE pages cover different rows".into());
    }
    for (index, (selector_page, _)) in selector
        .pages
        .iter()
        .zip(&value.pages)
        .enumerate()
        .take(end)
        .skip(first)
    {
        let mut session =
            ReadSession::new_untracked(&selector_page.column.page, ClosureMode::Selective);
        let action = plan_between(selector_page, &mut session, low, high, planning)?;
        counts.record(&action);
        if !matches!(action, PageAction::Skip(_)) {
            work.push((index, action));
        }
    }
    let page_results = run_pages(work.len(), threads, |slot| {
        let (index, action) = &work[slot];
        let selector_page = &selector.pages[*index];
        let value_page = &value.pages[*index];
        let mut counts = PageCounts::default();
        let mut session =
            ReadSession::new_untracked(&selector_page.column.page, ClosureMode::Selective);
        let action = action.clone();
        // A scan of frame-of-reference codes, or a translation of sorted
        // dictionary ids, over a value page of plain codes runs as one fused
        // pass over both pages.
        let compare = match action {
            PageAction::Scan => Some(vector::CodeCompare::Offset),
            PageAction::Translate(_) => Some(vector::CodeCompare::SortedDictionary),
            _ => None,
        };
        if let Some(compare) = compare {
            let mut value_session =
                ReadSession::new_untracked(&value_page.column.page, ClosureMode::Selective);
            if plan_sum(value_page, &mut value_session, planning)? == SumAction::Decode
                && let Some((sum, matched)) = vector::fused_sum_between(
                    &selector_page.column.decoder,
                    &mut session,
                    &value_page.column.decoder,
                    &mut value_session,
                    Span::new(0, selector_page.rows())?,
                    (low, high),
                    compare,
                )?
            {
                return Ok((sum, matched, counts));
            }
        }
        // Search and acceptance select a few contiguous spans; a scan or a
        // code translation may select rows anywhere, which a bitmap carries
        // at one bit per row and the value page consumes in one pass.
        let selected = match action {
            PageAction::Skip(_) => return Ok((0, 0, counts)),
            PageAction::Translate(_) | PageAction::Scan => Selected::Bitmap(
                execute_between_selection(selector_page, &mut session, &action, low, high)?,
            ),
            _ => Selected::Spans(execute_between(
                selector_page,
                &mut session,
                &action,
                low,
                high,
            )?),
        };
        let matched = match &selected {
            Selected::Spans(ranges) => ranges.iter().map(|range| range.len()).sum::<usize>(),
            Selected::Bitmap(selection) => selection.count(),
        };
        if matched == 0 {
            return Ok((0, 0, counts));
        }
        let mut session =
            ReadSession::new_untracked(&value_page.column.page, ClosureMode::Selective);
        let sum_action = plan_sum(value_page, &mut session, planning)?;
        if matches!(sum_action, SumAction::Constant(_)) {
            counts.constant_sums += 1;
        }
        let sum = match &selected {
            Selected::Spans(ranges) => execute_sum(value_page, &mut session, &sum_action, ranges)?,
            Selected::Bitmap(selection) => {
                // A fused pass decodes every row between the first and last
                // match; when the matches form few spans, summing span by
                // span decodes only the matching rows.
                let hull = selection.hull().map_or(0, |hull| hull.len());
                if selection.run_count() * SPAN_COST_ROWS < hull {
                    execute_sum(value_page, &mut session, &sum_action, &selection.ranges())?
                } else {
                    execute_sum_selection(value_page, &mut session, &sum_action, selection)?
                }
            }
        };
        Ok((sum, matched, counts))
    })?;
    let mut total = 0_i128;
    let mut matched = 0;
    for (sum, rows, page_counts) in page_results {
        total = total.checked_add(sum).ok_or("SUM WHERE overflow")?;
        matched += rows;
        counts.add(&page_counts);
    }
    Ok((total, matched, counts))
}

/// Fewest pages a thread is started for.
const MIN_PAGES_PER_THREAD: usize = 16;

/// Rows a fused pass may decode per span it saves: a span costs one scan
/// dispatch, about as much as decoding this many rows in a batch.
const SPAN_COST_ROWS: usize = 64;

/// The rows one selector page selects, in the form its action produced.
enum Selected {
    Spans(Vec<Span>),
    Bitmap(vector::Selection),
}

/// Matching rows of a paged column, as column-row spans.
pub fn between(
    column: &PagedColumn,
    low: i64,
    high: i64,
    planning: Planning,
    threads: usize,
) -> Result<(Vec<Span>, PageCounts), String> {
    if low > high {
        return Err("BETWEEN needs low <= high".into());
    }
    let page_results = run_pages(column.pages.len(), threads, |index| {
        let page = &column.pages[index];
        let mut session = ReadSession::new_untracked(&page.column.page, ClosureMode::Selective);
        let action = plan_between(page, &mut session, low, high, planning)?;
        let mut counts = PageCounts::default();
        counts.record(&action);
        let ranges = execute_between(page, &mut session, &action, low, high)?;
        Ok((ranges, counts))
    })?;
    let mut ranges = Vec::new();
    let mut counts = PageCounts::default();
    for (index, (page_ranges, page_counts)) in page_results.into_iter().enumerate() {
        let start = column.pages[index].start;
        for range in page_ranges {
            push_range(&mut ranges, start + range.start, start + range.end);
        }
        counts.add(&page_counts);
    }
    Ok((ranges, counts))
}

/// Runs `work` for every page index on up to `threads` threads and returns
/// the results in page order. Pages are independent, so any interleaving
/// gives the same results.
fn run_pages<T, F>(pages: usize, threads: usize, work: F) -> Result<Vec<T>, String>
where
    T: Send,
    F: Fn(usize) -> Result<T, String> + Sync,
{
    // A thread costs about as much to start as a few pages cost to plan and
    // scan, so small columns use fewer threads than asked.
    let threads = threads.min(pages / MIN_PAGES_PER_THREAD).max(1);
    if threads <= 1 {
        return (0..pages).map(&work).collect();
    }
    let next = AtomicUsize::new(0);
    let slots = Mutex::new((0..pages).map(|_| None).collect::<Vec<_>>());
    std::thread::scope(|scope| {
        for _ in 0..threads.min(pages) {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= pages {
                        break;
                    }
                    let result = work(index);
                    slots.lock().unwrap()[index] = Some(result);
                }
            });
        }
    });
    slots
        .into_inner()
        .unwrap()
        .into_iter()
        .map(|slot| slot.unwrap_or_else(|| Err("page was not executed".into())))
        .collect()
}

fn root_bounds(page: &Page) -> Vec<(FactScope, super::BoundSource)> {
    let root = page.root();
    page.facts
        .iter()
        .filter(|fact| fact.node == root)
        .filter_map(|fact| match fact.property {
            Property::Value(ValueInvariant::Bounded(source)) => Some((fact.scope, source)),
            _ => None,
        })
        .collect()
}

/// The tightest interval the page's bound facts give its non-null values,
/// read from the encoded bytes; `None` when the page proves no bound. Next
/// to `Page::stats` it measures how much slack a structural bound carries.
pub fn derived_bounds(page: &Page) -> Result<Option<(i128, i128)>, String> {
    let mut session = ReadSession::new_untracked(&page.column.page, ClosureMode::Selective);
    let mut tightest: Option<(i128, i128)> = None;
    for (_, source) in root_bounds(page) {
        let (lower, upper) = bound_interval(&mut session, source)?;
        tightest = Some(tightest.map_or((lower, upper), |(low, high)| {
            (low.max(lower), high.min(upper))
        }));
    }
    Ok(tightest)
}

fn sorted_dictionary_root(page: &Page) -> bool {
    matches!(
        page.column.decoder.node(page.root()),
        Ok(DecoderNode::Dictionary {
            sorted_unique: true,
            ..
        })
    )
}

fn cite(
    node: super::NodeId,
    scope: FactScope,
    property: ValueInvariant,
    evidence: Option<(i128, i128)>,
) -> Licence {
    Licence {
        fact: Authorization::Fact {
            node,
            scope,
            property: Property::Value(property),
        },
        evidence,
    }
}

/// The scope and value fact a licence cites, after confirming the page
/// proves it at its root.
fn cited(page: &Page, licence: &Licence) -> Result<(FactScope, ValueInvariant), String> {
    match licence.fact {
        Authorization::Fact {
            node,
            scope,
            property: Property::Value(property),
        } if node == page.root() && page.facts.contains(node, scope, Property::Value(property)) => {
            Ok((scope, property))
        }
        _ => Err("page action cites a value fact the page does not prove".into()),
    }
}

/// Re-reads a bound and confirms it is the interval the plan recorded.
fn reread(
    session: &mut ReadSession<'_>,
    source: super::BoundSource,
    licence: &Licence,
) -> Result<(i128, i128), String> {
    let interval = bound_interval(session, source)?;
    if licence.evidence != Some(interval) {
        return Err("page bound changed between planning and checking".into());
    }
    Ok(interval)
}
