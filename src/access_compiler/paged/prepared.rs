//! Reusable, immutably borrowed bindings whose page alignment is checked once.

use super::{
    Aggregate, Conjunct, DeletionVector, InMemory, PageCounts, PageStore, PagedColumn, Planning,
};

/// A selector/value binding checked once, before its first query.
///
/// Both columns and any deletion vector stay immutably borrowed while this
/// context is used. Appending pages or changing deletions therefore requires
/// preparing a new context. Plans and action checks still run per query;
/// only the invariant row-alignment check is reused.
///
/// ```compile_fail
/// use witness::access_compiler::{Recipe, paged::{PagePolicy, PagedColumn, PreparedSum, Planning}};
/// let policy = PagePolicy::smallest(vec![Recipe::For(Box::new(Recipe::BitPack))]);
/// let mut selector = PagedColumn::from_values(&[Some(1)], 128, &policy).unwrap();
/// let value = PagedColumn::from_values(&[Some(7)], 128, &policy).unwrap();
/// let prepared = PreparedSum::new(&selector, &value).unwrap();
/// selector.append(&[Some(2)], &policy).unwrap();
/// prepared.execute(1, 2, Planning::Facts, 1).unwrap();
/// ```
#[derive(Clone, Copy)]
pub struct PreparedSum<'a> {
    pub(super) selector: (&'a PagedColumn, &'a dyn PageStore),
    pub(super) value: (&'a PagedColumn, &'a dyn PageStore),
    pub(super) deleted: Option<&'a DeletionVector>,
}

impl<'a> PreparedSum<'a> {
    pub fn new(selector: &'a PagedColumn, value: &'a PagedColumn) -> Result<Self, String> {
        Self::new_in((selector, &InMemory), (value, &InMemory))
    }

    /// The same binding over caller-provided resident or file-backed stores.
    pub fn new_in(
        selector: (&'a PagedColumn, &'a dyn PageStore),
        value: (&'a PagedColumn, &'a dyn PageStore),
    ) -> Result<Self, String> {
        Self::from_parts(selector, value, None)
    }

    pub fn with_deletions(
        selector: &'a PagedColumn,
        value: &'a PagedColumn,
        deleted: &'a DeletionVector,
    ) -> Result<Self, String> {
        Self::with_deletions_in((selector, &InMemory), (value, &InMemory), deleted)
    }

    pub fn with_deletions_in(
        selector: (&'a PagedColumn, &'a dyn PageStore),
        value: (&'a PagedColumn, &'a dyn PageStore),
        deleted: &'a DeletionVector,
    ) -> Result<Self, String> {
        Self::from_parts(selector, value, Some(deleted))
    }

    pub(super) fn from_parts(
        selector: (&'a PagedColumn, &'a dyn PageStore),
        value: (&'a PagedColumn, &'a dyn PageStore),
        deleted: Option<&'a DeletionVector>,
    ) -> Result<Self, String> {
        let (left, right) = (selector.0, value.0);
        if left.rows != right.rows
            || left.page_rows != right.page_rows
            || left.pages.len() != right.pages.len()
        {
            return Err("SUM WHERE needs two columns paged alike".into());
        }
        if !same_page_spans(left, right) {
            return Err("SUM WHERE pages cover different rows".into());
        }
        if let Some(deleted) = deleted {
            deleted.check(left)?;
        }
        Ok(Self {
            selector,
            value,
            deleted,
        })
    }

    /// Executes new bounds using the ordinary planner, checker, and kernels.
    /// Initialization and this call can be timed independently.
    pub fn execute(
        &self,
        low: i64,
        high: i64,
        planning: Planning,
        threads: usize,
    ) -> Result<(i128, usize, PageCounts), String> {
        super::sum_where_prepared(self, (low, high), planning, threads)
    }
}

/// Validated columns for repeated conjunctive COUNT, SUM, or SUM-product.
///
/// Bounds may change between calls, but their order must match `selectors`.
/// The selected columns, aggregate columns, and optional deletion vector
/// remain immutably borrowed for the lifetime of the binding.
#[derive(Clone)]
pub struct PreparedAggregate<'a> {
    pub(super) selectors: Vec<&'a PagedColumn>,
    pub(super) aggregate: Aggregate<'a>,
    pub(super) deleted: Option<&'a DeletionVector>,
}

impl<'a> PreparedAggregate<'a> {
    pub fn new(selectors: &[&'a PagedColumn], aggregate: Aggregate<'a>) -> Result<Self, String> {
        Self::from_parts(selectors, aggregate, None)
    }

    pub fn with_deletions(
        selectors: &[&'a PagedColumn],
        aggregate: Aggregate<'a>,
        deleted: &'a DeletionVector,
    ) -> Result<Self, String> {
        Self::from_parts(selectors, aggregate, Some(deleted))
    }

    pub(super) fn from_parts(
        selectors: &[&'a PagedColumn],
        aggregate: Aggregate<'a>,
        deleted: Option<&'a DeletionVector>,
    ) -> Result<Self, String> {
        validate_aggregate(selectors.iter().copied(), aggregate, deleted)?;
        Ok(Self {
            selectors: selectors.to_vec(),
            aggregate,
            deleted,
        })
    }

    pub fn execute(
        &self,
        bounds: &[(i64, i64)],
        planning: Planning,
        threads: usize,
    ) -> Result<(i128, usize, PageCounts), String> {
        if bounds.len() != self.selectors.len() {
            return Err("prepared query needs one bound pair per selector".into());
        }
        let conjuncts = self
            .selectors
            .iter()
            .zip(bounds)
            .map(|(&column, &(low, high))| {
                if low > high {
                    return Err("BETWEEN needs low <= high".into());
                }
                Ok(Conjunct { column, low, high })
            })
            .collect::<Result<Vec<_>, String>>()?;
        // The columns are taken only from this already validated binding;
        // callers supply bounds, never replacement columns.
        let aligned = AlignedAggregate {
            conjuncts: &conjuncts,
            aggregate: self.aggregate,
            deleted: self.deleted,
        };
        super::aggregate_where_prepared(&aligned, planning, threads)
    }
}

/// Private execution binding: ordinary queries validate its columns now;
/// prepared queries construct it only from their already borrowed columns.
/// Keeping this distinct from the public API makes skipping validation an
/// internal invariant rather than a caller-controlled option.
pub(super) struct AlignedAggregate<'a, 'q> {
    pub(super) conjuncts: &'q [Conjunct<'a>],
    pub(super) aggregate: Aggregate<'a>,
    pub(super) deleted: Option<&'a DeletionVector>,
}

impl<'a, 'q> AlignedAggregate<'a, 'q> {
    pub(super) fn new(
        conjuncts: &'q [Conjunct<'a>],
        aggregate: Aggregate<'a>,
        deleted: Option<&'a DeletionVector>,
    ) -> Result<Self, String> {
        validate_aggregate(
            conjuncts.iter().map(|conjunct| conjunct.column),
            aggregate,
            deleted,
        )?;
        if conjuncts
            .iter()
            .any(|conjunct| conjunct.low > conjunct.high)
        {
            return Err("BETWEEN needs low <= high".into());
        }
        Ok(Self {
            conjuncts,
            aggregate,
            deleted,
        })
    }
}

fn validate_aggregate<'a>(
    selectors: impl Iterator<Item = &'a PagedColumn> + Clone,
    aggregate: Aggregate<'a>,
    deleted: Option<&DeletionVector>,
) -> Result<(), String> {
    let shape = selectors
        .clone()
        .next()
        .ok_or("a conjunctive query needs a conjunct")?;
    let aggregate_columns = match aggregate {
        Aggregate::Count => [None, None],
        Aggregate::Sum(value) => [Some(value), None],
        Aggregate::SumProduct(left, right) => [Some(left), Some(right)],
    };
    for column in selectors.chain(aggregate_columns.into_iter().flatten()) {
        if column.rows != shape.rows
            || column.page_rows != shape.page_rows
            || column.pages.len() != shape.pages.len()
            || !same_page_spans(column, shape)
        {
            return Err("a conjunctive query needs columns paged alike".into());
        }
    }
    if let Some(deleted) = deleted {
        deleted.check(shape)?;
    }
    Ok(())
}

fn same_page_spans(left: &PagedColumn, right: &PagedColumn) -> bool {
    left.pages
        .iter()
        .zip(&right.pages)
        .all(|(left, right)| left.start == right.start && left.rows() == right.rows())
}
