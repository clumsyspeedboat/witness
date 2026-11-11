use witness::access_compiler::paged::{
    self, DeletionVector, Page, PageAction, PagePolicy, PagedColumn, Planning,
};
use witness::access_compiler::{ClosureMode, FactScope, ReadSession, Recipe, Span, native_file};

fn only(recipe: Recipe) -> PagePolicy {
    PagePolicy::smallest(vec![recipe])
}

fn delta(restart_interval: usize) -> Recipe {
    Recipe::UnsignedDelta {
        restart_interval,
        deltas: Box::new(Recipe::BitPack),
    }
}

fn plan(page: &Page, low: i64, high: i64, planning: Planning) -> PageAction {
    paged::plan_between(
        page,
        &mut ReadSession::new_untracked(&page.column.page, ClosureMode::Selective),
        low,
        high,
        planning,
    )
    .unwrap()
}

fn exact_all_planners(column: &PagedColumn, low: i64, high: i64) {
    let expected = paged::sum_where(column, column, low, high, Planning::ScanOnly, 1).unwrap();
    for planning in [
        Planning::Facts,
        Planning::FactsCost,
        Planning::StoredStatisticsPlus,
        Planning::StoredStatisticsPlusCost,
    ] {
        let actual = paged::sum_where(column, column, low, high, planning, 1).unwrap();
        assert_eq!(
            (actual.0, actual.1),
            (expected.0, expected.1),
            "{planning:?}"
        );
    }
}

#[test]
fn piecewise_delta_search_is_legal_but_reconstruction_makes_scan_cheaper() {
    let values = (0..4096)
        .map(|row| Some((row % 256) as i64))
        .collect::<Vec<_>>();
    let column = PagedColumn::from_values(&values, 4096, &only(delta(256))).unwrap();
    let page = &column.pages()[0];
    assert!(matches!(
        plan(page, 10, 20, Planning::Facts),
        PageAction::SearchPiecewise { .. }
    ));
    assert!(matches!(
        plan(page, 10, 20, Planning::StoredStatisticsPlus),
        PageAction::SearchPiecewise { .. }
    ));
    for planning in [Planning::FactsCost, Planning::StoredStatisticsPlusCost] {
        assert!(matches!(plan(page, 10, 20, planning), PageAction::Scan));
    }
    exact_all_planners(&column, 10, 20);
}

#[test]
fn long_restart_search_falls_back_for_direct_whole_and_chunked_frames() {
    let values = (0..4096).map(|row| Some(row as i64)).collect::<Vec<_>>();
    for recipe in [
        delta(1024),
        Recipe::Frame(Box::new(delta(1024))),
        Recipe::ChunkedFrame {
            chunk_bytes: 4096,
            values: Box::new(delta(1024)),
        },
    ] {
        let column = PagedColumn::from_values(&values, 4096, &only(recipe)).unwrap();
        let page = &column.pages()[0];
        assert!(matches!(
            plan(page, 1500, 1550, Planning::Facts),
            PageAction::Search { .. }
        ));
        assert!(matches!(
            plan(page, 1500, 1550, Planning::FactsCost),
            PageAction::Scan
        ));
        assert!(matches!(
            plan(page, 1500, 1550, Planning::StoredStatisticsPlusCost),
            PageAction::Scan
        ));
        exact_all_planners(&column, 1500, 1550);
    }
}

#[test]
fn cheap_packed_search_is_retained_and_checker_still_rejects_wrong_scope() {
    let values = (0..4096)
        .map(|row| Some(row as i64 - 2048))
        .collect::<Vec<_>>();
    let column =
        PagedColumn::from_values(&values, 4096, &only(Recipe::For(Box::new(Recipe::BitPack))))
            .unwrap();
    let page = &column.pages()[0];
    let action = plan(page, -20, 30, Planning::FactsCost);
    assert!(matches!(action, PageAction::Search { .. }));
    let mut forged = action.clone();
    if let PageAction::Search { licence, .. } = &mut forged {
        licence.fact = match licence.fact {
            witness::access_compiler::Authorization::Fact { node, property, .. } => {
                witness::access_compiler::Authorization::Fact {
                    node,
                    property,
                    scope: FactScope::NonNullRows,
                }
            }
            _ => panic!("a search must cite a fact"),
        };
    }
    let mut session = ReadSession::new_untracked(&page.column.page, ClosureMode::Selective);
    assert!(paged::check_between(page, &mut session, &forged, -20, 30).is_err());
    exact_all_planners(&column, -20, 30);
}

#[test]
fn run_index_and_tiny_dictionary_work_are_charged_to_both_gated_arms() {
    let runs = (0..4096)
        .map(|row| Some((row / 1024) as i64))
        .collect::<Vec<_>>();
    let column = PagedColumn::from_values(
        &runs,
        4096,
        &only(Recipe::Rle {
            index_interval: 16,
            values: Box::new(Recipe::BitPack),
        }),
    )
    .unwrap();
    assert!(matches!(
        plan(&column.pages()[0], 1, 2, Planning::Facts),
        PageAction::Search { .. }
    ));
    assert!(matches!(
        plan(&column.pages()[0], 1, 2, Planning::FactsCost),
        PageAction::Scan
    ));
    exact_all_planners(&column, 1, 2);

    for entries in [3, 64] {
        let values = (0..4096)
            .map(|row| Some(((row * 17) % entries) as i64 * 10))
            .collect::<Vec<_>>();
        let column = PagedColumn::from_values(
            &values,
            4096,
            &only(Recipe::Dictionary(Box::new(Recipe::BitPack))),
        )
        .unwrap();
        let page = &column.pages()[0];
        assert!(matches!(
            plan(page, 10, 10, Planning::Facts),
            PageAction::Translate(_)
        ));
        for planning in [Planning::FactsCost, Planning::StoredStatisticsPlusCost] {
            let action = plan(page, 10, 10, planning);
            assert_eq!(matches!(action, PageAction::Scan), entries == 3);
        }
        // A gap exclusion and whole-page acceptance survive the cost gate.
        assert!(matches!(
            plan(page, 11, 19, Planning::FactsCost),
            PageAction::Skip(_)
        ));
        assert!(matches!(
            plan(page, 0, 1000, Planning::FactsCost),
            PageAction::Accept(_)
        ));
        exact_all_planners(&column, 10, 10);
    }
}

#[test]
fn native_nullable_extremes_and_deletions_remain_exact() {
    let values = (0..512)
        .map(|row| match row {
            0..=15 => None,
            16 => Some(i64::MIN),
            511 => Some(i64::MAX),
            _ => Some((row as i64 - 256) * 100),
        })
        .collect::<Vec<_>>();
    let mut policy = only(Recipe::Dictionary(Box::new(Recipe::BitPack)));
    policy.rank_interval = 128;
    let writer = PagedColumn::from_values(&values, 128, &policy).unwrap();
    let bytes = native_file::to_bytes(&writer).unwrap();
    drop(writer);
    let column = native_file::from_bytes(&bytes).unwrap().column;
    assert!(
        column
            .pages()
            .iter()
            .all(|page| page.column.truth.is_empty())
    );
    let mut deleted = DeletionVector::for_column(&column);
    for row in (0..512).step_by(7) {
        deleted.delete(row).unwrap();
    }
    for (low, high) in [(i64::MIN, i64::MAX), (-500, 500), (i64::MAX, i64::MAX)] {
        exact_all_planners(&column, low, high);
        let expected =
            paged::sum_where_live(&column, &column, &deleted, low, high, Planning::ScanOnly, 1)
                .unwrap();
        for planning in [Planning::FactsCost, Planning::StoredStatisticsPlusCost] {
            let actual =
                paged::sum_where_live(&column, &column, &deleted, low, high, planning, 2).unwrap();
            assert_eq!((actual.0, actual.1), (expected.0, expected.1));
        }
    }
    assert!(paged::sum_where(&column, &column, 2, 1, Planning::FactsCost, 1).is_err());
    let page = &column.pages()[1];
    let mut session = ReadSession::new_untracked(&page.column.page, ClosureMode::Selective);
    let action = plan(page, -20000, -15000, Planning::FactsCost);
    let ranges = paged::execute_between(page, &mut session, &action, -20000, -15000).unwrap();
    assert!(
        ranges
            .iter()
            .all(|range| Span::new(0, page.rows()).unwrap().end >= range.end)
    );
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config::with_cases(32))]
    #[test]
    fn gated_and_fixed_planners_agree_on_random_compositions(
        raw in proptest::collection::vec(proptest::num::i16::ANY, 1..200),
        left in proptest::num::i16::ANY,
        right in proptest::num::i16::ANY,
    ) {
        let low = i64::from(left.min(right));
        let high = i64::from(left.max(right));
        for mode in 0..6 {
            let mut values = raw.iter().map(|&value| Some(i64::from(value))).collect::<Vec<_>>();
            let recipe = match mode {
                0 => Recipe::For(Box::new(Recipe::BitPack)),
                1 => Recipe::Dictionary(Box::new(Recipe::BitPack)),
                2 => Recipe::Rle { index_interval: 8, values: Box::new(Recipe::For(Box::new(Recipe::BitPack))) },
                3 => Recipe::Delta { restart_interval: 128, deltas: Box::new(Recipe::BitPack) },
                4 => {
                    for block in values.chunks_mut(128) { block.sort_unstable(); }
                    delta(128)
                }
                _ => {
                    values.sort_unstable();
                    values[0] = None;
                    Recipe::Frame(Box::new(Recipe::Dictionary(Box::new(Recipe::BitPack))))
                }
            };
            let writer = PagedColumn::from_values(&values, 128, &only(recipe)).unwrap_or_else(|error| panic!("mode {mode}: {error}"));
            let bytes = native_file::to_bytes(&writer).unwrap();
            let column = native_file::from_bytes(&bytes).unwrap().column;
            for planning in [Planning::FactsCost, Planning::StoredStatisticsPlusCost] {
                let actual = paged::sum_where(&column, &column, low, high, planning, 1).unwrap();
                let selected = values.iter().flatten().filter(|&&value| low <= value && value <= high).copied().collect::<Vec<_>>();
                let expected = selected.iter().map(|&value| i128::from(value)).sum::<i128>();
                proptest::prop_assert_eq!((actual.0, actual.1), (expected, selected.len()));
            }
        }
    }
}
