//! Preparing reuses only immutable page alignment. Bounds, action licenses,
//! result checks, and exact aggregate semantics remain per-query obligations.

use std::fs::{self, File};
use std::sync::atomic::{AtomicUsize, Ordering};

use proptest::prelude::*;
use witness::access_compiler::paged::{
    self, Aggregate, Conjunct, DeletionVector, FileStore, PagePolicy, PagedColumn, Planning,
    PreparedAggregate, PreparedSum, WholePageFileStore,
};
use witness::access_compiler::{Recipe, native_file, paged_file};

const PAGE: usize = 128;
const PLANNERS: [Planning; 8] = [
    Planning::Facts,
    Planning::FactsCost,
    Planning::FactBounds,
    Planning::StoredStatistics,
    Planning::StoredStatisticsPlus,
    Planning::StoredStatisticsPlusCost,
    Planning::StoredStatisticsDictionary,
    Planning::ScanOnly,
];

fn policy() -> PagePolicy {
    PagePolicy::smallest(vec![
        Recipe::For(Box::new(Recipe::BitPack)),
        Recipe::Dictionary(Box::new(Recipe::BitPack)),
        Recipe::Rle {
            index_interval: 32,
            values: Box::new(Recipe::For(Box::new(Recipe::BitPack))),
        },
    ])
}

fn encoded(values: &[Option<i64>]) -> PagedColumn {
    PagedColumn::from_values(values, PAGE, &policy()).unwrap()
}

fn oracle(
    selector: &[Option<i64>],
    value: &[Option<i64>],
    low: i64,
    high: i64,
    deleted: Option<&DeletionVector>,
) -> (i128, usize) {
    let mut answer = (0, 0);
    for (row, (&selector, &value)) in selector.iter().zip(value).enumerate() {
        if !deleted.is_some_and(|deleted| deleted.is_deleted(row))
            && selector.is_some_and(|value| low <= value && value <= high)
        {
            answer.0 += value.map_or(0, i128::from);
            answer.1 += 1;
        }
    }
    answer
}

#[test]
fn prepared_bindings_reject_equal_totals_with_different_partial_page_boundaries() {
    let mut left = encoded(&[Some(1); 17]);
    left.append(&[Some(2); 19], &policy()).unwrap();
    let mut right = encoded(&[Some(3); 19]);
    right.append(&[Some(4); 17], &policy()).unwrap();
    assert_eq!(left.rows(), right.rows());
    assert_eq!(left.pages().len(), right.pages().len());
    assert!(PreparedSum::new(&left, &right).is_err());
    assert!(PreparedAggregate::new(&[&left, &right], Aggregate::Count).is_err());
    assert!(PreparedAggregate::new(&[&left], Aggregate::Sum(&right)).is_err());
    assert!(PreparedAggregate::new(&[&left], Aggregate::SumProduct(&left, &right)).is_err());
    assert!(paged::sum_where(&left, &right, 0, 10, Planning::Facts, 1).is_err());

    let wrong_length = encoded(&[Some(1); 35]);
    assert!(PreparedSum::new(&left, &wrong_length).is_err());
    let wrong_page_size = PagedColumn::from_values(&[Some(1); 36], 64, &policy()).unwrap();
    assert!(PreparedSum::new(&left, &wrong_page_size).is_err());
    assert!(PreparedAggregate::new(&[], Aggregate::Count).is_err());
}

#[test]
fn empty_and_all_null_tables_keep_zero_aggregate_semantics() {
    for values in [vec![], vec![None; PAGE * 2 + 7]] {
        let column = encoded(&values);
        let prepared = PreparedSum::new(&column, &column).unwrap();
        let aggregate = PreparedAggregate::new(&[&column], Aggregate::Count).unwrap();
        for planning in PLANNERS {
            let sum = prepared.execute(i64::MIN, i64::MAX, planning, 4).unwrap();
            let count = aggregate
                .execute(&[(i64::MIN, i64::MAX)], planning, 4)
                .unwrap();
            assert_eq!((sum.0, sum.1), (0, 0));
            assert_eq!((count.0, count.1), (0, 0));
        }
        assert!(prepared.execute(1, 0, Planning::Facts, 1).is_err());
    }
}

#[test]
fn bounds_and_deletion_shapes_are_checked_without_weakening_preparation() {
    let column = encoded(&[Some(1); 300]);
    let prepared = PreparedSum::new(&column, &column).unwrap();
    assert!(prepared.execute(2, 1, Planning::Facts, 1).is_err());
    let aggregate = PreparedAggregate::new(&[&column], Aggregate::Count).unwrap();
    assert!(aggregate.execute(&[], Planning::Facts, 1).is_err());
    assert!(
        aggregate
            .execute(&[(1, 1), (1, 1)], Planning::Facts, 1)
            .is_err()
    );
    assert!(aggregate.execute(&[(2, 1)], Planning::ScanOnly, 1).is_err());
    assert_eq!(
        aggregate.execute(&[(1, 1)], Planning::Facts, 1).unwrap().0,
        300
    );

    let mut other = encoded(&[Some(1); 127]);
    other.append(&[Some(1); 173], &policy()).unwrap();
    let wrong_deletions = DeletionVector::for_column(&other);
    assert!(PreparedSum::with_deletions(&column, &column, &wrong_deletions).is_err());
    assert!(
        PreparedAggregate::with_deletions(&[&column], Aggregate::Count, &wrong_deletions).is_err()
    );
}

#[test]
fn appends_after_releasing_context_require_fresh_alignment_and_keep_prefix_deletes() {
    let mut selector = encoded(&[Some(1); 71]);
    let mut value = encoded(&[Some(3); 71]);
    let mut deleted = DeletionVector::for_column(&selector);
    deleted.delete(7).unwrap();
    {
        let prepared = PreparedSum::with_deletions(&selector, &value, &deleted).unwrap();
        assert_eq!(
            prepared.execute(1, 1, Planning::Facts, 1).unwrap().0,
            70 * 3
        );
    }
    selector.append(&[Some(2); 47], &policy()).unwrap();
    value.append(&[Some(5); 47], &policy()).unwrap();
    let prepared = PreparedSum::with_deletions(&selector, &value, &deleted).unwrap();
    let actual = prepared.execute(1, 2, Planning::Facts, 1).unwrap();
    assert_eq!((actual.0, actual.1), (70 * 3 + 47 * 5, 117));
}

#[test]
fn loaded_native_pages_and_regional_queries_need_no_oracle() {
    let values = (0..(PAGE * 16 + 17))
        .map(|row| Some((row % (PAGE * 4)) as i64))
        .collect::<Vec<_>>();
    let writer = encoded(&values);
    let bytes = native_file::to_bytes(&writer).unwrap();
    drop(writer);
    let mut loaded = native_file::from_bytes(&bytes).unwrap().column;
    assert!(
        loaded
            .pages()
            .iter()
            .all(|page| page.column.truth.is_empty())
    );
    loaded.enable_ordered_regions();
    assert!(!loaded.ordered());
    let prepared = PreparedSum::new(&loaded, &loaded).unwrap();
    for planning in PLANNERS {
        for (low, high) in [(3, 11), (128, 256), (-10, -1), (0, 10_000)] {
            let expected = oracle(&values, &values, low, high, None);
            let actual = prepared.execute(low, high, planning, 4).unwrap();
            assert_eq!((actual.0, actual.1), expected);
            let plain = paged::sum_where(&loaded, &loaded, low, high, planning, 4).unwrap();
            assert_eq!((actual.0, actual.1), (plain.0, plain.1));
            assert_eq!(actual.2, plain.2);
        }
    }
}

#[test]
fn prepared_conjunctions_preserve_nullable_aggregates_products_and_deletes() {
    let a = (0..(PAGE * 18 + 11))
        .map(|row| (row % 11 != 0).then_some((row % 401) as i64 - 200))
        .collect::<Vec<_>>();
    let b = (0..a.len())
        .map(|row| (row % 13 != 0).then_some((row % 17) as i64 - 8))
        .collect::<Vec<_>>();
    let columns = [encoded(&a), encoded(&b)];
    let mut deleted = DeletionVector::for_column(&columns[0]);
    for row in (0..a.len()).step_by(7) {
        deleted.delete(row).unwrap();
    }
    for aggregate in [
        Aggregate::Count,
        Aggregate::Sum(&columns[0]),
        Aggregate::SumProduct(&columns[0], &columns[1]),
    ] {
        let prepared =
            PreparedAggregate::with_deletions(&[&columns[0], &columns[1]], aggregate, &deleted)
                .unwrap();
        for bounds in [
            [(-100, 80), (-3, 4)],
            [(i64::MIN, i64::MAX), (8, 8)],
            [(1000, 2000), (-8, 8)],
        ] {
            let selected = (0..a.len())
                .filter(|&row| {
                    !deleted.is_deleted(row)
                        && a[row].is_some_and(|value| bounds[0].0 <= value && value <= bounds[0].1)
                        && b[row].is_some_and(|value| bounds[1].0 <= value && value <= bounds[1].1)
                })
                .collect::<Vec<_>>();
            let expected = match aggregate {
                Aggregate::Count => selected.len() as i128,
                Aggregate::Sum(_) => selected
                    .iter()
                    .map(|&row| a[row].map_or(0, i128::from))
                    .sum(),
                Aggregate::SumProduct(..) => selected
                    .iter()
                    .map(|&row| {
                        a[row]
                            .zip(b[row])
                            .map_or(0, |(a, b)| i128::from(a) * i128::from(b))
                    })
                    .sum(),
            };
            let conjuncts = [
                Conjunct {
                    column: &columns[0],
                    low: bounds[0].0,
                    high: bounds[0].1,
                },
                Conjunct {
                    column: &columns[1],
                    low: bounds[1].0,
                    high: bounds[1].1,
                },
            ];
            for planning in PLANNERS {
                let actual = prepared.execute(&bounds, planning, 4).unwrap();
                assert_eq!((actual.0, actual.1), (expected, selected.len()));
                let plain =
                    paged::aggregate_where_live(&conjuncts, aggregate, &deleted, planning, 4)
                        .unwrap();
                assert_eq!(actual.2, plain.2);
            }
        }
    }
}

#[test]
fn footer_audits_still_run_for_prepared_queries() {
    let values = (0..(PAGE * 3))
        .map(|row| Some((row * 7919 % 997) as i64))
        .collect::<Vec<_>>();
    let mut column = encoded(&values);
    for index in 0..column.pages().len() {
        let bounds = paged::derived_bounds(&column.pages()[index]).unwrap();
        column.replace_statistics(index, bounds).unwrap();
    }
    let prepared = PreparedSum::new(&column, &column).unwrap();
    let actual = prepared
        .execute(100, 300, Planning::DerivedFooter, 1)
        .unwrap();
    assert_eq!(
        (actual.0, actual.1),
        oracle(&values, &values, 100, 300, None)
    );
    column.replace_statistics(0, Some((0, 200))).unwrap();
    let prepared = PreparedSum::new(&column, &column).unwrap();
    assert!(
        prepared
            .execute(140, 300, Planning::DerivedFooter, 1)
            .is_err()
    );
}

#[test]
fn prepared_file_bindings_preserve_answer_and_read_accounting() {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "witness-prepared-{}-{}.wpf",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let values = (0..(PAGE * 7 + 3))
        .map(|row| Some((row * 37 % 401) as i64))
        .collect::<Vec<_>>();
    let column = encoded(&values);
    let layout = paged_file::write(&column, &path).unwrap();
    {
        let file = File::open(&path).unwrap();
        let small = FileStore {
            file: &file,
            offsets: layout.offsets.clone(),
        };
        let whole = WholePageFileStore(FileStore {
            file: &file,
            offsets: layout.offsets.clone(),
        });
        for store in [&small as &dyn paged::PageStore, &whole] {
            let prepared = PreparedSum::new_in((&column, store), (&column, store)).unwrap();
            for planning in PLANNERS {
                let actual = prepared.execute(100, 200, planning, 1).unwrap();
                let plain =
                    paged::sum_where_in((&column, store), (&column, store), 100, 200, planning, 1)
                        .unwrap();
                assert_eq!(
                    (actual.0, actual.1),
                    oracle(&values, &values, 100, 200, None)
                );
                assert_eq!(actual.2, plain.2);
            }
        }
    }
    fs::remove_file(path).unwrap();
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]
    #[test]
    fn varying_prepared_bounds_match_nullable_row_oracle(
        selector in prop::collection::vec(prop::option::of(-500_i64..500), 1..2048),
        low in -700_i64..700,
        high in -700_i64..700,
        deletion_step in 1_usize..33,
    ) {
        let (low, high) = (low.min(high), low.max(high));
        let value = selector.iter().enumerate().map(|(row, value)| (row % 5 != 0).then_some(value.unwrap_or(0) * -3)).collect::<Vec<_>>();
        let selector_pages = encoded(&selector);
        let value_pages = encoded(&value);
        let mut deleted = DeletionVector::for_column(&selector_pages);
        for row in (0..selector.len()).step_by(deletion_step) { deleted.delete(row).unwrap(); }
        let prepared = PreparedSum::with_deletions(&selector_pages, &value_pages, &deleted).unwrap();
        let expected = oracle(&selector, &value, low, high, Some(&deleted));
        for planning in PLANNERS {
            let actual = prepared.execute(low, high, planning, 4).unwrap();
            prop_assert_eq!((actual.0, actual.1), expected);
        }
    }
}
