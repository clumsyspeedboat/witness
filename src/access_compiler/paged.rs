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

use std::sync::{Mutex, OnceLock};

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
        self.candidate_pages_in(&InMemory, low, high)
    }

    /// `candidate_pages`, confirming the boundary pages from `store`.
    fn candidate_pages_in(
        &self,
        store: &dyn PageStore,
        low: i128,
        high: i128,
    ) -> Result<Option<(usize, usize)>, String> {
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
        // The two confirming reads go through `store`; they are not added to
        // a query's I/O counts, being two reads however large the column.
        let confirm = |index: usize, row_of: fn(&Page) -> usize| -> Result<i128, String> {
            let page = &self.pages[index];
            let mut session = store.session(page, index, Access::Plan)?;
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
    /// Every fact the page proves: bounds to skip and accept, order to
    /// search, a sorted dictionary to translate, the column certificate.
    Facts,
    ScanOnly,
    /// A plain zone map: exact per-page minima and maxima stored by the
    /// writer, used to skip and accept pages and nothing else. It isolates
    /// what stored statistics buy on the same executor as derived facts.
    StoredStatistics,
    /// Derived bounds alone: facts skip and accept pages, but never search,
    /// translate, or use the column certificate. Against `StoredStatistics`
    /// it isolates replacing stored bounds with derived ones.
    FactBounds,
    /// A zone map's skips and accepts plus every encoding shortcut that
    /// `Facts` uses. Against `Facts` it isolates stored against derived
    /// bounds with the shortcuts held equal.
    StoredStatisticsPlus,
    /// Plans from bounds kept in a footer, as a zone map is, where each
    /// page's `stats` hold its derived bounds; every page the query reads
    /// re-derives its bound from those bytes and fails on a mismatch, so only
    /// pages skipped or accepted without being read rely on the footer.
    DerivedFooter,
}

impl Planning {
    /// The planner's name in result files.
    pub fn label(self) -> &'static str {
        match self {
            Self::Facts => "facts",
            Self::ScanOnly => "scan",
            Self::StoredStatistics => "zonemap",
            Self::FactBounds => "factbounds",
            Self::StoredStatisticsPlus => "zonemapplus",
            Self::DerivedFooter => "footer",
        }
    }

    /// Whether stored per-page statistics decide skips and accepts.
    pub fn uses_statistics(self) -> bool {
        matches!(
            self,
            Self::StoredStatistics | Self::StoredStatisticsPlus | Self::DerivedFooter
        )
    }

    /// Whether bounds derived from the encoding decide skips and accepts.
    pub fn uses_bounds(self) -> bool {
        matches!(self, Self::Facts | Self::FactBounds)
    }

    /// Whether encoding facts may license a search, a code translation, or
    /// the column certificate.
    pub fn uses_shortcuts(self) -> bool {
        matches!(self, Self::Facts | Self::StoredStatisticsPlus)
    }
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
    let (low_bound, high_bound) = (i128::from(low), i128::from(high));
    let is_shortcut = |action: &PageAction| {
        matches!(
            action,
            PageAction::Search { .. }
                | PageAction::SearchPiecewise { .. }
                | PageAction::Translate(_)
        )
    };
    let action = if planning == Planning::ScanOnly {
        PageAction::Scan
    } else if planning.uses_statistics() {
        match choose_by_statistics(page, low_bound, high_bound) {
            // A page the statistics cannot decide may still take a shortcut.
            PageAction::Scan if planning.uses_shortcuts() => {
                let candidate = choose_between(page, session, low_bound, high_bound)?;
                if is_shortcut(&candidate) {
                    candidate
                } else {
                    PageAction::Scan
                }
            }
            action => action,
        }
    } else {
        match choose_between(page, session, low_bound, high_bound)? {
            action if is_shortcut(&action) && !planning.uses_shortcuts() => PageAction::Scan,
            action => action,
        }
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
        // No entry: the page holds no value, or its summary has no interval
        // for it, as a derived bound may not; only the first can be skipped.
        None if page.non_null == 0 => PageAction::Skip(licence),
        None => PageAction::Scan,
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
        (None, false) if page.non_null == 0 => Ok(()),
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
    if planning.uses_statistics()
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
    if planning.uses_bounds() {
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
    /// Positioned reads issued against a file-backed page store, and the
    /// bytes they transferred; zero for pages held in memory.
    pub io_operations: usize,
    pub io_bytes: usize,
}

impl PageCounts {
    /// Adds the reads a session transferred.
    fn account(&mut self, session: &ReadSession<'_>) {
        let metrics = session.metrics();
        self.io_operations += metrics.transfer_operations;
        self.io_bytes += metrics.transferred_bytes;
    }

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
        self.io_operations += other.io_operations;
        self.io_bytes += other.io_bytes;
    }
}

/// Where the bytes of a column's pages are read from. Planning and execution
/// read every field through a session the store opens, so the same plans run
/// over pages in memory or in a file, and a file store accounts for the reads.
pub trait PageStore: Sync {
    /// A session over `page`, the `index`-th page of its column, opened for
    /// `access`.
    fn session<'a>(
        &'a self,
        page: &'a Page,
        index: usize,
        access: Access,
    ) -> Result<ReadSession<'a>, String>;
}

/// Why a session reads a page. Planning reads a few fields; executing a
/// page reads most of it, so a file store fetches the whole page with its
/// first read instead of one read per batch of rows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Access {
    Plan,
    Execute,
}

/// Pages read from memory, without access accounting.
pub struct InMemory;

impl PageStore for InMemory {
    fn session<'a>(
        &'a self,
        page: &'a Page,
        _: usize,
        _: Access,
    ) -> Result<ReadSession<'a>, String> {
        Ok(ReadSession::new_untracked(
            &page.column.page,
            ClosureMode::Selective,
        ))
    }
}

/// Pages of one column stored in a file, page `i` starting at `offsets[i]`.
/// Each field a session needs is fetched with a positioned read, and the
/// session counts the reads and the bytes they transfer.
pub struct FileStore<'f> {
    pub file: &'f std::fs::File,
    pub offsets: Vec<u64>,
}

impl PageStore for FileStore<'_> {
    fn session<'a>(
        &'a self,
        page: &'a Page,
        index: usize,
        access: Access,
    ) -> Result<ReadSession<'a>, String> {
        let offset = *self.offsets.get(index).ok_or("file store lacks the page")?;
        let mode = match access {
            Access::Plan => ClosureMode::Selective,
            Access::Execute => ClosureMode::FullPage,
        };
        Ok(ReadSession::from_file(
            &page.column.page,
            mode,
            self.file,
            offset,
        ))
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
    sum_where_in(
        (selector, &InMemory),
        (value, &InMemory),
        low,
        high,
        planning,
        threads,
    )
}

/// `sum_where` over pages read from the given stores.
pub fn sum_where_in(
    (selector, selector_store): (&PagedColumn, &dyn PageStore),
    (value, value_store): (&PagedColumn, &dyn PageStore),
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
    // Every page is planned first, then only pages left with work are
    // executed; both phases share the worker pool, so a query that skips
    // nearly every page executes almost nothing.
    let mut counts = PageCounts::default();
    let mut work = Vec::new();
    // On a column ordered across pages, only a contiguous run of pages can
    // match; every page outside it is skipped without being planned.
    // The order certificate is a derived fact; planners without the
    // encoding's shortcuts consult every page.
    let (first, end) = if planning.uses_shortcuts() {
        selector
            .candidate_pages_in(selector_store, i128::from(low), i128::from(high))?
            .unwrap_or((0, selector.pages.len()))
    } else {
        (0, selector.pages.len())
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
    // Pages are planned on the worker pool once there are enough of them;
    // a column of a few pages is planned on the calling thread.
    let actions = run_pages(end - first, threads, |offset| {
        let index = first + offset;
        let page = &selector.pages[index];
        let mut session = selector_store.session(page, index, Access::Plan)?;
        let action = plan_between(page, &mut session, low, high, planning)?;
        let mut reads = PageCounts::default();
        reads.account(&session);
        Ok((action, reads))
    })?;
    for (offset, (action, reads)) in actions.into_iter().enumerate() {
        counts.add(&reads);
        counts.record(&action);
        if !matches!(action, PageAction::Skip(_)) {
            work.push((first + offset, action));
        }
    }
    let page_results = run_pages(work.len(), threads, |slot| {
        let (index, action) = &work[slot];
        let selector_page = &selector.pages[*index];
        let value_page = &value.pages[*index];
        let mut counts = PageCounts::default();
        let mut session = selector_store.session(selector_page, *index, Access::Execute)?;
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
            let mut value_session = value_store.session(value_page, *index, Access::Execute)?;
            let fused = if plan_sum(value_page, &mut value_session, planning)? == SumAction::Decode
            {
                vector::fused_sum_between(
                    &selector_page.column.decoder,
                    &mut session,
                    &value_page.column.decoder,
                    &mut value_session,
                    Span::new(0, selector_page.rows())?,
                    (low, high),
                    compare,
                )?
            } else {
                None
            };
            counts.account(&value_session);
            if let Some((sum, matched)) = fused {
                verify_footer(selector_page, &mut session, planning)?;
                counts.account(&session);
                return Ok((sum, matched, counts));
            }
        }
        // Search and acceptance select a few contiguous spans; a scan or a
        // code translation may select rows anywhere, which a bitmap carries
        // at one bit per row and the value page consumes in one pass.
        let selected = match action {
            PageAction::Skip(_) => {
                counts.account(&session);
                return Ok((0, 0, counts));
            }
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
        if !matches!(action, PageAction::Accept(_)) {
            verify_footer(selector_page, &mut session, planning)?;
        }
        counts.account(&session);
        if matched == 0 {
            return Ok((0, 0, counts));
        }
        let mut session = value_store.session(value_page, *index, Access::Execute)?;
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
        counts.account(&session);
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

/// One conjunct of a conjunctive predicate: `low <= value <= high` on
/// `column`.
#[derive(Clone, Copy, Debug)]
pub struct Conjunct<'c> {
    pub column: &'c PagedColumn,
    pub low: i64,
    pub high: i64,
}

/// What a conjunctive query computes over its matching rows.
#[derive(Clone, Copy, Debug)]
pub enum Aggregate<'c> {
    Count,
    Sum(&'c PagedColumn),
    /// SUM of the product of two columns, over rows where both are non-null.
    SumProduct(&'c PagedColumn, &'c PagedColumn),
}

/// `SELECT aggregate WHERE c1 AND c2 AND ...` over columns paged alike: the
/// aggregate and the matching row count. Each conjunct plans every page from
/// its column's facts (or the zone map, or nothing), and every action is
/// checked. A page is skipped when any conjunct skips it; a dense page that a
/// conjunct accepts is not constrained by it; the remaining conjuncts are
/// evaluated as row selections and intersected, the cheapest first, until
/// none is left or no row remains.
///
/// `PageCounts::skipped` counts skipped pages; the other counts tally the
/// actions taken by the conjuncts of pages that were not skipped.
pub fn aggregate_where(
    conjuncts: &[Conjunct<'_>],
    aggregate: Aggregate<'_>,
    planning: Planning,
    threads: usize,
) -> Result<(i128, usize, PageCounts), String> {
    let Some(first) = conjuncts.first() else {
        return Err("a conjunctive query needs a conjunct".into());
    };
    let shape = first.column;
    let mut columns = conjuncts
        .iter()
        .map(|conjunct| conjunct.column)
        .collect::<Vec<_>>();
    match aggregate {
        Aggregate::Count => {}
        Aggregate::Sum(value) => columns.push(value),
        Aggregate::SumProduct(left, right) => columns.extend([left, right]),
    }
    for column in &columns {
        if column.rows != shape.rows
            || column.page_rows != shape.page_rows
            || column.pages.len() != shape.pages.len()
            || column
                .pages
                .iter()
                .zip(&shape.pages)
                .any(|(page, other)| page.start != other.start || page.rows() != other.rows())
        {
            return Err("a conjunctive query needs columns paged alike".into());
        }
    }
    for conjunct in conjuncts {
        if conjunct.low > conjunct.high {
            return Err("BETWEEN needs low <= high".into());
        }
    }
    // An ordered column narrows the pages any conjunct on it can match.
    let (mut first_page, mut end_page) = (0, shape.pages.len());
    if planning.uses_shortcuts() {
        for conjunct in conjuncts {
            if let Some((start, end)) = conjunct
                .column
                .candidate_pages(i128::from(conjunct.low), i128::from(conjunct.high))?
            {
                first_page = first_page.max(start);
                end_page = end_page.min(end);
            }
        }
    }
    let end_page = end_page.max(first_page);
    let mut counts = PageCounts {
        skipped: shape.pages.len() - (end_page - first_page),
        ..PageCounts::default()
    };
    let mut work = Vec::new();
    // Each page is planned conjunct by conjunct, stopping at the first skip;
    // pages are planned on the worker pool once there are enough of them.
    let planned = run_pages(end_page - first_page, threads, |offset| {
        let index = first_page + offset;
        let mut actions = Vec::with_capacity(conjuncts.len());
        for (slot, conjunct) in conjuncts.iter().enumerate() {
            let page = &conjunct.column.pages[index];
            let action = plan_between(
                page,
                &mut untracked(page),
                conjunct.low,
                conjunct.high,
                planning,
            )?;
            if matches!(action, PageAction::Skip(_)) {
                return Ok(None);
            }
            actions.push((slot, action));
        }
        Ok(Some(actions))
    })?;
    for (offset, actions) in planned.into_iter().enumerate() {
        let index = first_page + offset;
        let Some(mut actions) = actions else {
            counts.skipped += 1;
            continue;
        };
        for (_, action) in &actions {
            counts.record(action);
        }
        // A dense accepted page selects every row; it constrains nothing.
        actions.retain(|(slot, action)| {
            let page = &conjuncts[*slot].column.pages[index];
            !(matches!(action, PageAction::Accept(_)) && page.non_null == page.rows())
        });
        actions.sort_by_key(|(_, action)| action_cost(action));
        work.push((index, actions));
    }
    let page_results = run_pages(work.len(), threads, |slot| {
        let (index, actions) = &work[slot];
        let rows = Span::new(0, shape.pages[*index].rows())?;
        // Scans and code translations run as one fused pass over the page
        // when every page involved is plain codes.
        if !actions.is_empty()
            && actions
                .iter()
                .all(|(_, action)| matches!(action, PageAction::Scan | PageAction::Translate(_)))
        {
            let mut pages = actions
                .iter()
                .map(|(conjunct_slot, action)| {
                    let conjunct = &conjuncts[*conjunct_slot];
                    let page = &conjunct.column.pages[*index];
                    vector::ConjunctPage {
                        decoder: &page.column.decoder,
                        session: untracked(page),
                        low: conjunct.low,
                        high: conjunct.high,
                        compare: if matches!(action, PageAction::Translate(_)) {
                            vector::CodeCompare::SortedDictionary
                        } else {
                            vector::CodeCompare::Offset
                        },
                    }
                })
                .collect::<Vec<_>>();
            let mut fused = match aggregate {
                Aggregate::Count => vector::FusedAggregate::Count,
                Aggregate::Sum(column) => {
                    vector::FusedAggregate::Sum(Box::new(value_page(column, *index)))
                }
                Aggregate::SumProduct(left, right) => {
                    vector::FusedAggregate::SumProduct(Box::new([
                        value_page(left, *index),
                        value_page(right, *index),
                    ]))
                }
            };
            if let Some((value, matched)) = vector::fused_conjunction(&mut pages, &mut fused, rows)?
            {
                return Ok((value, matched, 0));
            }
        }
        let mut selection = vector::Selection::full(rows);
        for (conjunct_slot, action) in actions {
            let conjunct = &conjuncts[*conjunct_slot];
            let page = &conjunct.column.pages[*index];
            let mut session = ReadSession::new_untracked(&page.column.page, ClosureMode::Selective);
            let matched =
                execute_between_selection(page, &mut session, action, conjunct.low, conjunct.high)?;
            for (word, other) in selection.words.iter_mut().zip(&matched.words) {
                *word &= other;
            }
            if selection.words.iter().all(|word| *word == 0) {
                return Ok((0, 0, 0));
            }
        }
        let matched = selection.count();
        let mut constant = 0;
        let value = match aggregate {
            Aggregate::Count => matched as i128,
            Aggregate::Sum(column) => {
                let page = &column.pages[*index];
                let mut session =
                    ReadSession::new_untracked(&page.column.page, ClosureMode::Selective);
                let action = plan_sum(page, &mut session, planning)?;
                constant += usize::from(matches!(action, SumAction::Constant(_)));
                execute_sum_selection(page, &mut session, &action, &selection)?
            }
            Aggregate::SumProduct(left, right) => {
                let (left, right) = (&left.pages[*index], &right.pages[*index]);
                let mut left_session =
                    ReadSession::new_untracked(&left.column.page, ClosureMode::Selective);
                let mut right_session =
                    ReadSession::new_untracked(&right.column.page, ClosureMode::Selective);
                vector::sum_product_selection(
                    &left.column.decoder,
                    &mut left_session,
                    &right.column.decoder,
                    &mut right_session,
                    &selection,
                )?
            }
        };
        Ok((value, matched, constant))
    })?;
    let mut total = 0_i128;
    let mut matched = 0;
    for (value, rows, constant) in page_results {
        total = total.checked_add(value).ok_or("aggregate overflow")?;
        matched += rows;
        counts.constant_sums += constant;
    }
    Ok((total, matched, counts))
}

/// Under `Planning::DerivedFooter`, re-derives the bound of a page the query
/// has read, from the bytes in `session`, and fails if the footer entry
/// planned from (the page's `stats`) disagrees.
fn verify_footer(
    page: &Page,
    session: &mut ReadSession<'_>,
    planning: Planning,
) -> Result<(), String> {
    if planning == Planning::DerivedFooter && derived_bounds_in(page, session)? != page.stats {
        return Err("footer bound disagrees with the page it describes".into());
    }
    Ok(())
}

fn untracked(page: &Page) -> ReadSession<'_> {
    ReadSession::new_untracked(&page.column.page, ClosureMode::Selective)
}

fn value_page(column: &PagedColumn, index: usize) -> vector::ValuePage<'_, '_> {
    let page = &column.pages[index];
    vector::ValuePage {
        decoder: &page.column.decoder,
        session: untracked(page),
    }
}

/// Order in which a page evaluates its conjuncts: searches touch a few rows,
/// code comparisons read codes only, scans decode every row.
fn action_cost(action: &PageAction) -> u8 {
    match action {
        PageAction::Skip(_) | PageAction::Accept(_) => 0,
        PageAction::Search { .. } | PageAction::SearchPiecewise { .. } => 1,
        PageAction::Translate(_) => 2,
        PageAction::Scan => 3,
    }
}

/// Fewest pages assigned per worker; smaller queries stay on the caller.
const MIN_PAGES_PER_THREAD: usize = 16;

/// Small batches amortize queue locking while balancing uneven page work.
const PAGES_PER_BATCH: usize = 4;

/// One bounded pool shared by queries. Single-threaded and small queries do
/// not initialize it. Explicit sizing avoids inheriting Rayon environment
/// settings, and query task counts still honor the requested thread limit.
fn page_workers() -> Result<&'static rayon::ThreadPool, String> {
    static WORKERS: OnceLock<Result<rayon::ThreadPool, String>> = OnceLock::new();
    WORKERS
        .get_or_init(|| {
            rayon::ThreadPoolBuilder::new()
                .num_threads(std::thread::available_parallelism().map_or(1, usize::from))
                .thread_name(|index| format!("witness-page-{index}"))
                .build()
                .map_err(|error| format!("cannot start page workers: {error}"))
        })
        .as_ref()
        .map_err(Clone::clone)
}

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
    // Dispatch and synchronization still cost more than tiny page searches,
    // even with persistent workers, so small queries run on the caller.
    let threads = threads.min(pages / MIN_PAGES_PER_THREAD).max(1);
    if threads <= 1 {
        return (0..pages).map(&work).collect();
    }
    let workers = page_workers()?;
    let mut slots = (0..pages).map(|_| None).collect::<Vec<_>>();
    let batches = Mutex::new(slots.chunks_mut(PAGES_PER_BATCH).enumerate());
    workers.in_place_scope(|scope| {
        for _ in 0..threads.min(workers.current_num_threads()) {
            scope.spawn(|_| {
                loop {
                    // Each worker owns its output slice until the scope ends;
                    // no lock is needed to publish individual page results.
                    let Some((batch, slots)) = batches.lock().unwrap().next() else {
                        break;
                    };
                    for (offset, slot) in slots.iter_mut().enumerate() {
                        *slot = Some(work(batch * PAGES_PER_BATCH + offset));
                    }
                }
            });
        }
    });
    slots
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
    derived_bounds_in(page, &mut untracked(page))
}

/// `derived_bounds`, reading the bound fields through `session`, so the
/// reads land wherever the session's store keeps the page.
pub fn derived_bounds_in(
    page: &Page,
    session: &mut ReadSession<'_>,
) -> Result<Option<(i128, i128)>, String> {
    let mut tightest: Option<(i128, i128)> = None;
    for (_, source) in root_bounds(page) {
        let (lower, upper) = bound_interval(session, source)?;
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

#[cfg(test)]
mod tests {
    use super::run_pages;
    use std::cell::Cell;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn page_workers_preserve_order_and_visit_each_page_once() {
        let visits = (0..131).map(|_| AtomicUsize::new(0)).collect::<Vec<_>>();
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let results = run_pages(visits.len(), 3, |page| {
            let running = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(running, Ordering::SeqCst);
            visits[page].fetch_add(1, Ordering::Relaxed);
            std::thread::yield_now();
            active.fetch_sub(1, Ordering::SeqCst);
            // Results need only be Send, not Sync.
            Ok(Cell::new(page))
        })
        .unwrap();
        assert_eq!(
            results
                .into_iter()
                .map(Cell::into_inner)
                .collect::<Vec<_>>(),
            (0..131).collect::<Vec<_>>()
        );
        assert!(
            visits
                .iter()
                .all(|count| count.load(Ordering::Relaxed) == 1)
        );
        assert!((1..=3).contains(&peak.load(Ordering::SeqCst)));
    }

    #[test]
    fn page_workers_return_the_first_error_in_page_order_and_remain_usable() {
        let result = run_pages(131, 4, |page| {
            if page == 17 || page == 117 {
                Err(format!("failed page {page}"))
            } else {
                Ok(page)
            }
        });
        assert_eq!(result.unwrap_err(), "failed page 17");
        assert_eq!(run_pages(131, 4, Ok).unwrap(), (0..131).collect::<Vec<_>>());
        assert_eq!(run_pages(3, 0, Ok).unwrap(), vec![0, 1, 2]);
        assert!(
            run_pages::<usize, _>(0, 64, |_| panic!("empty query"))
                .unwrap()
                .is_empty()
        );
    }
}
