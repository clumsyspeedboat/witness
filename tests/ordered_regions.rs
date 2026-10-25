use std::fs::{self, File};
use std::sync::atomic::{AtomicUsize, Ordering};

use proptest::prelude::*;
use witness::access_compiler::paged::{
    self, Access, Aggregate, Conjunct, DeletionVector, FileStore, Page, PagePolicy, PageStore,
    PagedColumn, Planning,
};
use witness::access_compiler::{ReadSession, Recipe, Span};
use witness::access_compiler::{native_file, paged_file};

const PAGE: usize = 128;
fn policy() -> PagePolicy {
    PagePolicy::smallest(vec![Recipe::For(Box::new(Recipe::BitPack))])
}
fn batch(pages: usize, base: i64) -> Vec<Option<i64>> {
    (0..pages * PAGE)
        .map(|row| Some(base + row as i64))
        .collect()
}

#[test]
fn appended_batches_keep_independent_certificates_and_original_row_ids() {
    let mut column = PagedColumn::from_values(&batch(4, 0), PAGE, &policy()).unwrap();
    column.enable_ordered_regions();
    column.append(&batch(4, 0), &policy()).unwrap();
    assert!(!column.ordered());
    assert_eq!(
        column.ordered_regions(),
        [Span::new(0, 4).unwrap(), Span::new(4, 8).unwrap()]
    );
    assert_eq!(
        column.candidate_page_ranges(10, 19).unwrap(),
        [Span::new(0, 1).unwrap(), Span::new(4, 5).unwrap()]
    );
    let (ranges, counts) = paged::between(&column, 10, 19, Planning::Facts, 1).unwrap();
    assert_eq!(
        ranges,
        [
            Span::new(10, 20).unwrap(),
            Span::new(4 * PAGE + 10, 4 * PAGE + 20).unwrap()
        ]
    );
    assert_eq!(counts.skipped, 6);
    for planner in [
        Planning::Facts,
        Planning::StoredStatisticsPlus,
        Planning::StoredStatistics,
        Planning::ScanOnly,
    ] {
        assert_eq!(
            paged::sum_where(&column, &column, 10, 19, planner, 1)
                .unwrap()
                .0,
            290
        );
    }
    column.disable_ordered_regions();
    assert_eq!(
        column.candidate_page_ranges(10, 19).unwrap(),
        [Span::new(0, 8).unwrap()]
    );
}

#[test]
fn unordered_and_nullable_gaps_are_retained_and_appends_extend_only_valid_seams() {
    let mut values = batch(2, 0);
    values.extend((0..PAGE).map(|r| (r % 9 != 0).then_some((r * 7919 % 311) as i64)));
    values.extend(batch(2, 0));
    let mut column = PagedColumn::from_values(&values, PAGE, &policy()).unwrap();
    column.enable_ordered_regions();
    assert_eq!(
        column.ordered_regions(),
        [Span::new(0, 2).unwrap(), Span::new(3, 5).unwrap()]
    );
    // The gap is retained even for a predicate outside every certified region.
    assert_eq!(
        column.candidate_page_ranges(1_000, 2_000).unwrap(),
        [Span::new(2, 3).unwrap()]
    );
    column
        .append(&batch(1, 2 * PAGE as i64), &policy())
        .unwrap();
    assert_eq!(column.ordered_regions()[1], Span::new(3, 6).unwrap());
    assert!(column.candidate_page_ranges(20, 10).is_err());
}

#[test]
fn sparse_candidate_intersections_and_deletes_match_the_scan() {
    let mut values = batch(4, 0);
    values.extend(batch(4, 0));
    let mut a = PagedColumn::from_values(&values, PAGE, &policy()).unwrap();
    let mut other = values.clone();
    other.rotate_left(2 * PAGE);
    let mut b = PagedColumn::from_values(&other, PAGE, &policy()).unwrap();
    a.enable_ordered_regions();
    b.enable_ordered_regions();
    let conjuncts = [
        Conjunct {
            column: &a,
            low: 10,
            high: 200,
        },
        Conjunct {
            column: &b,
            low: 260,
            high: 450,
        },
    ];
    for aggregate in [
        Aggregate::Count,
        Aggregate::Sum(&a),
        Aggregate::SumProduct(&a, &b),
    ] {
        let expected =
            paged::aggregate_where(&conjuncts, aggregate, Planning::ScanOnly, 1).unwrap();
        for planning in [Planning::Facts, Planning::StoredStatisticsPlus] {
            let actual = paged::aggregate_where(&conjuncts, aggregate, planning, 4).unwrap();
            assert_eq!((actual.0, actual.1), (expected.0, expected.1));
        }
    }
    let mut deleted = DeletionVector::for_column(&a);
    for row in (0..values.len()).step_by(7) {
        deleted.delete(row).unwrap();
    }
    let expected = paged::sum_where_live(&a, &b, &deleted, 10, 200, Planning::ScanOnly, 1).unwrap();
    let actual = paged::sum_where_live(&a, &b, &deleted, 10, 200, Planning::Facts, 1).unwrap();
    assert_eq!((actual.0, actual.1), (expected.0, expected.1));
}

struct CountingStore<'a> {
    inner: FileStore<'a>,
    operations: AtomicUsize,
}
impl PageStore for CountingStore<'_> {
    fn session<'a>(
        &'a self,
        page: &'a Page,
        index: usize,
        access: Access,
    ) -> Result<ReadSession<'a>, String> {
        self.operations.fetch_add(1, Ordering::Relaxed);
        self.inner.session(page, index, access)
    }
}

#[test]
fn certificate_confirmations_are_counted_and_fresh_readers_rebuild_regions() {
    let mut values = batch(3, 0);
    values.extend(batch(3, 0));
    let writer = PagedColumn::from_values(&values, PAGE, &policy()).unwrap();
    let bytes = native_file::to_bytes(&writer).unwrap();
    drop(writer);
    let mut loaded = native_file::from_bytes(&bytes).unwrap().column;
    loaded.enable_ordered_regions();
    let path = std::env::temp_dir().join(format!("witness-region-{}.wpf", std::process::id()));
    let layout = paged_file::write(&loaded, &path).unwrap();
    let file = File::open(&path).unwrap();
    let store = CountingStore {
        inner: FileStore {
            file: &file,
            offsets: layout.offsets,
        },
        operations: AtomicUsize::new(0),
    };
    // Both regions are skipped: the only file operations are confirmations.
    let (sum, matched, counts) = paged::sum_where_in(
        (&loaded, &store),
        (&loaded, &store),
        -20,
        -10,
        Planning::Facts,
        1,
    )
    .unwrap();
    assert_eq!((sum, matched), (0, 0));
    // Each confirmation reads a FOR base and its packed stream.
    assert_eq!(counts.io_operations, 4);
    assert_eq!(store.operations.load(Ordering::Relaxed), 2);
    assert!(counts.io_bytes > 0);
    fs::remove_file(path).unwrap();
}

#[test]
fn losing_order_evidence_rebuilds_regions_and_keeps_the_affected_page() {
    let mut values = batch(3, 0);
    values.extend(batch(3, 0));
    let mut column = PagedColumn::from_values(&values, PAGE, &policy()).unwrap();
    column.enable_ordered_regions();
    column
        .forge_descriptor(
            1,
            witness::access_compiler::CheckedInvariants {
                non_decreasing: false,
                non_decreasing_non_null: false,
                null_placement: witness::access_compiler::NullPlacement::NoNulls,
            },
        )
        .unwrap();
    assert!(
        !column
            .ordered_regions()
            .iter()
            .any(|range| range.start <= 1 && 1 < range.end)
    );
    assert!(
        column
            .candidate_page_ranges(-20, -10)
            .unwrap()
            .iter()
            .any(|range| range.start <= 1 && 1 < range.end)
    );
    let actual = paged::sum_where(&column, &column, 130, 150, Planning::Facts, 1).unwrap();
    let expected = paged::sum_where(&column, &column, 130, 150, Planning::ScanOnly, 1).unwrap();
    assert_eq!((actual.0, actual.1), (expected.0, expected.1));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]
    #[test]
    fn arbitrary_page_order_and_predicates_match_full_scan(bases in prop::collection::vec(-1_000_i64..1_000, 1..30), a in -1_100_i64..1_100, b in -1_100_i64..1_100) {
        let values = bases.iter().flat_map(|&base| batch(1, base)).collect::<Vec<_>>();
        let mut column = PagedColumn::from_values(&values, PAGE, &policy()).unwrap();
        column.enable_ordered_regions();
        let (low, high) = (a.min(b), a.max(b));
        let actual = paged::sum_where(&column, &column, low, high, Planning::Facts, 1).unwrap();
        let expected = paged::sum_where(&column, &column, low, high, Planning::ScanOnly, 1).unwrap();
        prop_assert_eq!((actual.0, actual.1), (expected.0, expected.1));
    }
}
