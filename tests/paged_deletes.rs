//! Deleting rows changes no page and no fact. A plan made without looking at
//! the deletions stays sound, because every fact it cites holds for each row
//! of its page and so for the rows that remain; executing it clears the
//! deleted rows. Every planner and kernel must answer over the remaining
//! rows exactly as a scan of those rows does.

use proptest::prelude::*;
use witness::access_compiler::Recipe;
use witness::access_compiler::paged::{
    self, Aggregate, Conjunct, DeletionVector, PagePolicy, PagedColumn, Planning,
};

const PLANNERS: [Planning; 7] = [
    Planning::Facts,
    Planning::ScanOnly,
    Planning::StoredStatistics,
    Planning::FactBounds,
    Planning::StoredStatisticsPlus,
    Planning::StoredStatisticsDictionary,
    Planning::DerivedFooter,
];

fn bitpack() -> Box<Recipe> {
    Box::new(Recipe::BitPack)
}

/// The paged studies' direct menu.
fn direct() -> PagePolicy {
    PagePolicy::smallest(vec![
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
    ])
}

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

/// A column of one of the shapes the census finds: sorted, constant runs,
/// a small domain, wide random values, or random values with nulls.
fn column(seed: u64, rows: usize, shape: u8) -> Vec<Option<i64>> {
    let mut random = Random(seed);
    (0..rows)
        .map(|row| match shape % 5 {
            0 => Some(row as i64 * 3 - 500),
            1 => Some((row / 97) as i64 * 11),
            2 => Some((random.next() % 13) as i64 * 7),
            3 => Some(random.next() as i64 - (1 << 30)),
            _ => (!random.next().is_multiple_of(4)).then(|| (random.next() % 5_000) as i64),
        })
        .collect()
}

/// Deletes about `rate` of `rows`, or a contiguous block of that share when
/// `clustered`.
fn deletions(column: &PagedColumn, seed: u64, rate: f64, clustered: bool) -> DeletionVector {
    let mut deleted = DeletionVector::for_column(column);
    let rows = column.rows();
    let mut random = Random(seed);
    if clustered {
        let width = (rows as f64 * rate) as usize;
        let start = random.next() as usize % (rows - width + 1);
        for row in start..start + width {
            deleted.delete(row).unwrap();
        }
    } else {
        for row in 0..rows {
            if (random.next() % 10_000) as f64 / 10_000.0 < rate {
                deleted.delete(row).unwrap();
            }
        }
    }
    deleted
}

fn truth(
    selector: &[Option<i64>],
    value: &[Option<i64>],
    deleted: &DeletionVector,
    low: i64,
    high: i64,
) -> (i128, usize) {
    let (mut sum, mut rows) = (0_i128, 0);
    for (row, (selected, value)) in selector.iter().zip(value).enumerate() {
        if !deleted.is_deleted(row)
            && selected.is_some_and(|selected| low <= selected && selected <= high)
        {
            rows += 1;
            sum += value.map_or(0, i128::from);
        }
    }
    (sum, rows)
}

/// The footer planner plans from derived bounds in place of statistics.
fn footer(column: &PagedColumn) -> PagedColumn {
    let mut column = column.clone();
    for index in 0..column.pages().len() {
        let bound = paged::derived_bounds(&column.pages()[index]).unwrap();
        column.replace_statistics(index, bound).unwrap();
    }
    column
}

#[test]
fn a_deletion_vector_marks_each_row_once_and_only_rows_of_its_table() {
    let values = column(3, 2_000, 0);
    let pages = PagedColumn::from_values(&values, 300, &direct()).unwrap();
    let mut deleted = DeletionVector::for_column(&pages);
    assert_eq!(deleted.delete(299), Ok(true));
    assert_eq!(deleted.delete(299), Ok(false));
    assert_eq!(deleted.delete(1_999), Ok(true));
    assert!(deleted.delete(2_000).is_err());
    assert!(deleted.is_deleted(299) && deleted.is_deleted(1_999) && !deleted.is_deleted(300));
    assert_eq!(deleted.deleted(), 2);
    // A vector shaped for other pages is refused.
    let other = PagedColumn::from_values(&values, 256, &direct()).unwrap();
    assert!(paged::sum_where_live(&other, &other, &deleted, 0, 10, Planning::Facts, 1).is_err());
    // Rows appended after the deletions are live.
    let mut grown = pages.clone();
    grown.append(&column(5, 500, 0), &direct()).unwrap();
    let grown_values = [values.clone(), column(5, 500, 0)].concat();
    let (low, high) = (i64::MIN, i64::MAX);
    let answer =
        paged::sum_where_live(&grown, &grown, &deleted, low, high, Planning::Facts, 1).unwrap();
    assert_eq!(
        (answer.0, answer.1),
        truth(&grown_values, &grown_values, &deleted, low, high)
    );
}

#[test]
fn deleting_every_row_leaves_nothing_and_no_deletion_changes_nothing() {
    let values = column(9, 3_000, 2);
    let stored = PagedColumn::from_values(&values, 512, &direct()).unwrap();
    let none = DeletionVector::for_column(&stored);
    let mut all = DeletionVector::for_column(&stored);
    for row in 0..stored.rows() {
        all.delete(row).unwrap();
    }
    for planning in PLANNERS {
        let pages = if planning == Planning::DerivedFooter {
            footer(&stored)
        } else {
            stored.clone()
        };
        for (low, high) in [(0, 40), (14, 14), (i64::MIN, i64::MAX)] {
            let plain = paged::sum_where(&pages, &pages, low, high, planning, 1).unwrap();
            let live =
                paged::sum_where_live(&pages, &pages, &none, low, high, planning, 1).unwrap();
            assert_eq!(plain, live, "{planning:?}");
            let gone = paged::sum_where_live(&pages, &pages, &all, low, high, planning, 3).unwrap();
            assert_eq!((gone.0, gone.1), (0, 0), "{planning:?}");
            // Plans do not depend on the deletions.
            assert_eq!(plain.2.skipped, gone.2.skipped, "{planning:?}");
            let conjuncts = [Conjunct {
                column: &pages,
                low,
                high,
            }];
            let count =
                paged::aggregate_where_live(&conjuncts, Aggregate::Count, &all, planning, 1)
                    .unwrap();
            assert_eq!((count.0, count.1), (0, 0), "{planning:?}");
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn every_planner_answers_over_the_rows_that_remain(
        seed in any::<u64>(),
        rows in 1_usize..5_000,
        page_rows in 64_usize..2_100,
        shape in 0_u8..5,
        value_shape in 0_u8..5,
        rate in prop_oneof![Just(0.0), Just(0.01), Just(0.1), Just(0.5), Just(1.0), 0.0_f64..1.0],
        clustered in any::<bool>(),
        low_share in 0.0_f64..1.0,
        width_share in 0.0_f64..0.6,
    ) {
        let selector = column(seed, rows, shape);
        let value = column(seed ^ 0xfeed, rows, value_shape);
        let selector_pages = PagedColumn::from_values(&selector, page_rows, &direct()).unwrap();
        let value_pages = PagedColumn::from_values(&value, page_rows, &direct()).unwrap();
        let deleted = deletions(&selector_pages, seed ^ 0xde1e7e, rate, clustered);
        let mut present = selector.iter().flatten().copied().collect::<Vec<_>>();
        present.sort_unstable();
        let (low, high) = if present.is_empty() {
            (0, 0)
        } else {
            let at = |share: f64| present[((present.len() - 1) as f64 * share) as usize];
            (at(low_share), at((low_share + width_share).min(1.0)))
        };
        let expected = truth(&selector, &value, &deleted, low, high);
        for planning in PLANNERS {
            let selector_pages = if planning == Planning::DerivedFooter {
                footer(&selector_pages)
            } else {
                selector_pages.clone()
            };
            for threads in [1, 4] {
                let (sum, matched, _) = paged::sum_where_live(
                    &selector_pages, &value_pages, &deleted, low, high, planning, threads,
                ).unwrap();
                prop_assert_eq!((sum, matched), expected, "{:?} x{}", planning, threads);
            }
            // Conjunctions: the same predicate, a second conjunct on the
            // value column, and the three aggregates.
            let one = [Conjunct { column: &selector_pages, low, high }];
            let (count, rows_counted, _) = paged::aggregate_where_live(
                &one, Aggregate::Count, &deleted, planning, 3,
            ).unwrap();
            prop_assert_eq!((count, rows_counted), (expected.1 as i128, expected.1));
            let (sum, rows_summed, _) = paged::aggregate_where_live(
                &one, Aggregate::Sum(&value_pages), &deleted, planning, 1,
            ).unwrap();
            prop_assert_eq!((sum, rows_summed), expected);
            let two = [
                Conjunct { column: &selector_pages, low, high },
                Conjunct { column: &value_pages, low: -1_000, high: 1_000 },
            ];
            let (product, rows_both, _) = paged::aggregate_where_live(
                &two, Aggregate::SumProduct(&selector_pages, &value_pages), &deleted, planning, 1,
            ).unwrap();
            let (mut want, mut want_rows) = (0_i128, 0_usize);
            for (row, (left, right)) in selector.iter().zip(&value).enumerate() {
                let keep = !deleted.is_deleted(row)
                    && left.is_some_and(|left| low <= left && left <= high)
                    && right.is_some_and(|right| (-1_000..=1_000).contains(&right));
                if keep {
                    want_rows += 1;
                    if let (Some(left), Some(right)) = (left, right) {
                        want += i128::from(*left) * i128::from(*right);
                    }
                }
            }
            prop_assert_eq!((product, rows_both), (want, want_rows), "{:?}", planning);
        }
    }
}
