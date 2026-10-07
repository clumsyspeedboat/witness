#![cfg(feature = "experiment")]

use witness::access_compiler::paged::{self, PageAction, PagePolicy, PagedColumn, Planning};
use witness::access_compiler::{ClosureMode, ReadSession, Recipe, Span, vector};

fn policy() -> PagePolicy {
    PagePolicy::smallest(vec![
        Recipe::For(Box::new(Recipe::BitPack)),
        Recipe::Dictionary(Box::new(Recipe::BitPack)),
        Recipe::Delta {
            restart_interval: 8,
            deltas: Box::new(Recipe::BitPack),
        },
    ])
}

#[test]
fn reversed_predicates_fail_even_when_no_page_would_execute() {
    let empty = PagedColumn::new(8).unwrap();
    let values = (0..64).map(Some).collect::<Vec<_>>();
    let ordered = PagedColumn::from_values(&values, 8, &policy()).unwrap();
    assert!(ordered.ordered());
    let page = &ordered.pages()[0];
    let mut session = ReadSession::new(&page.column.page, ClosureMode::Selective);
    assert!(paged::check_between(page, &mut session, &PageAction::Scan, 100, -100).is_err());
    for planning in [
        Planning::Facts,
        Planning::ScanOnly,
        Planning::StoredStatistics,
    ] {
        for column in [&empty, &ordered] {
            for threads in [1, 8] {
                assert!(paged::between(column, 100, -100, planning, threads).is_err());
                assert!(
                    paged::sum_where(column, column, 100, -100, planning, threads).is_err(),
                    "{planning:?} with {} rows",
                    column.rows()
                );
            }
        }
    }
}

#[test]
fn parallel_queries_preserve_answers_and_page_order() {
    const PAGE_ROWS: usize = 32;
    // More than 16 pages per worker exercises the former scheduler's
    // parallel path too; the last page is deliberately short.
    let rows = 67 * PAGE_ROWS - 11;
    let selected = (0..rows)
        .map(|row| match row / PAGE_ROWS % 5 {
            0 => Some((row % PAGE_ROWS) as i64 - 16),
            1 => Some(7),
            2 => None,
            3 => (row % 7 != 0).then_some(((row * 19) % 43) as i64 - 20),
            _ => Some(((row * 13) % 71) as i64 - 30),
        })
        .collect::<Vec<_>>();
    let values = (0..rows)
        .map(|row| (row % 11 != 0).then_some((row % 53) as i64 - 26))
        .collect::<Vec<_>>();
    let selector = PagedColumn::from_values(&selected, PAGE_ROWS, &policy()).unwrap();
    let value = PagedColumn::from_values(&values, PAGE_ROWS, &policy()).unwrap();
    for (low, high) in [(-100, 100), (7, 7), (-8, 10), (90, 99)] {
        let expected_rows = selected
            .iter()
            .enumerate()
            .filter_map(|(row, value)| {
                value
                    .is_some_and(|value| low <= value && value <= high)
                    .then_some(row)
            })
            .collect::<Vec<_>>();
        let expected_sum = expected_rows
            .iter()
            .filter_map(|&row| values[row])
            .map(i128::from)
            .sum::<i128>();
        for planning in [
            Planning::Facts,
            Planning::ScanOnly,
            Planning::StoredStatistics,
        ] {
            let serial = paged::between(&selector, low, high, planning, 1).unwrap();
            let serial_sum = paged::sum_where(&selector, &value, low, high, planning, 1).unwrap();
            assert_eq!(
                serial
                    .0
                    .iter()
                    .flat_map(|span| span.start..span.end)
                    .collect::<Vec<_>>(),
                expected_rows
            );
            assert_eq!(
                (serial_sum.0, serial_sum.1),
                (expected_sum, expected_rows.len())
            );
            for threads in [2, 4, 64] {
                assert_eq!(
                    paged::between(&selector, low, high, planning, threads).unwrap(),
                    serial
                );
                assert_eq!(
                    paged::sum_where(&selector, &value, low, high, planning, threads).unwrap(),
                    serial_sum
                );
            }
        }
    }
}

#[test]
fn concurrent_queries_keep_their_results_separate() {
    let rows = 96 * 8;
    let selected = (0..rows).map(|row| Some(row as i64)).collect::<Vec<_>>();
    let selector = PagedColumn::from_values(&selected, 8, &policy()).unwrap();
    let value = PagedColumn::from_values(&vec![Some(3); rows], 8, &policy()).unwrap();
    std::thread::scope(|scope| {
        for query in 0..8 {
            let selector = &selector;
            let value = &value;
            scope.spawn(move || {
                let low = query * 64;
                let high = low + 127;
                for _ in 0..4 {
                    let (sum, matched, _) =
                        paged::sum_where(selector, value, low, high, Planning::ScanOnly, 4)
                            .unwrap();
                    assert_eq!((sum, matched), (384, 128));
                    let (ranges, _) =
                        paged::between(selector, low, high, Planning::ScanOnly, 4).unwrap();
                    assert_eq!(
                        ranges,
                        vec![Span::new(low as usize, high as usize + 1).unwrap()]
                    );
                }
            });
        }
    });
}

#[test]
fn constant_sums_reject_the_same_invalid_selections_as_decoded_sums() {
    let column = PagedColumn::from_values(&[Some(7); 8], 8, &policy()).unwrap();
    let page = &column.pages()[0];
    for planning in [
        Planning::Facts,
        Planning::ScanOnly,
        Planning::StoredStatistics,
    ] {
        let mut session = ReadSession::new(&page.column.page, ClosureMode::Selective);
        let action = paged::plan_sum(page, &mut session, planning).unwrap();
        for ranges in [
            vec![Span { start: 0, end: 9 }],
            vec![Span { start: 4, end: 3 }],
            vec![Span { start: 0, end: 4 }, Span { start: 3, end: 6 }],
        ] {
            assert!(paged::execute_sum(page, &mut session, &action, &ranges).is_err());
        }
        for selection in [
            vector::Selection {
                rows: Span { start: 0, end: 9 },
                words: vec![1],
            },
            vector::Selection {
                rows: Span { start: 0, end: 8 },
                words: vec![1 << 8],
            },
            vector::Selection {
                rows: Span { start: 0, end: 8 },
                words: vec![],
            },
        ] {
            assert!(paged::execute_sum_selection(page, &mut session, &action, &selection).is_err());
        }
    }
}
