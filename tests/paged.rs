use witness::access_compiler::paged::{
    self, Licence, PageAction, PageCounts, PagePolicy, PagedColumn, Planning, SumAction,
};
use witness::access_compiler::{
    Authorization, ClosureMode, FactScope, NullPlacement, Property, ReadSession, Recipe, Span,
    ValueInvariant, push_range,
};

const PAGE: usize = 1_024;

fn policy() -> PagePolicy {
    let bitpack = || Box::new(Recipe::BitPack);
    PagePolicy::smallest(vec![
        Recipe::For(bitpack()),
        Recipe::Dictionary(bitpack()),
        Recipe::Rle {
            index_interval: 32,
            values: Box::new(Recipe::For(bitpack())),
        },
        Recipe::Delta {
            restart_interval: 256,
            deltas: bitpack(),
        },
    ])
}

/// Pages of every kind the census finds: sorted, constant, all-null,
/// clustered, and random with sprinkled nulls.
fn mixed_column() -> Vec<Option<i64>> {
    let mut state = 17_u64;
    let mut random = move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        (state >> 33) as i64
    };
    let mut values = Vec::new();
    for page in 0..24 {
        for row in 0..PAGE as i64 {
            values.push(match page % 6 {
                0 => Some(page * 10_000 + row * 3),
                1 => Some(4_242),
                2 => None,
                3 => Some(50_000 + random() % 500),
                4 => (row % 7 != 0).then(|| random() % 1_000_000),
                _ => Some(page * 10_000 + 5_000 + random() % 2_000),
            });
        }
    }
    values.truncate(values.len() - 300);
    values
}

fn truth_ranges(values: &[Option<i64>], low: i64, high: i64) -> Vec<Span> {
    let mut ranges = Vec::new();
    for (row, value) in values.iter().enumerate() {
        if value.is_some_and(|value| low <= value && value <= high) {
            push_range(&mut ranges, row, row + 1);
        }
    }
    ranges
}

fn predicates() -> Vec<(i64, i64)> {
    vec![
        (4_242, 4_242),
        (0, 100_000),
        (50_100, 50_300),
        (i64::MIN, i64::MAX),
        (10_000, 12_000),
        (-5, -1),
        (130_000, 170_000),
    ]
}

#[test]
fn paged_answers_equal_the_truth_with_and_without_facts() {
    let selector_values = mixed_column();
    let value_values = selector_values
        .iter()
        .enumerate()
        .map(|(row, _)| Some((row as i64 % 97) - 40))
        .collect::<Vec<_>>();
    let selector = PagedColumn::from_values(&selector_values, PAGE, &policy()).unwrap();
    let value = PagedColumn::from_values(&value_values, PAGE, &policy()).unwrap();
    assert_eq!(selector.rows(), selector_values.len());
    for (low, high) in predicates() {
        let expected = truth_ranges(&selector_values, low, high);
        let expected_sum: i128 = expected
            .iter()
            .flat_map(|range| &value_values[range.start..range.end])
            .flatten()
            .map(|&value| i128::from(value))
            .sum();
        let matched: usize = expected.iter().map(|range| range.len()).sum();
        for planning in [
            Planning::Facts,
            Planning::ScanOnly,
            Planning::StoredStatistics,
            Planning::FactBounds,
            Planning::StoredStatisticsPlus,
        ] {
            for threads in [1, 8] {
                let (ranges, _) = paged::between(&selector, low, high, planning, threads).unwrap();
                assert_eq!(ranges, expected, "{low}..={high} {planning:?} x{threads}");
                let (sum, rows, _) =
                    paged::sum_where(&selector, &value, low, high, planning, threads).unwrap();
                assert_eq!(
                    (sum, rows),
                    (expected_sum, matched),
                    "{low}..={high} {planning:?}"
                );
            }
        }
    }
}

#[test]
fn every_page_takes_the_action_its_facts_license() {
    let values = mixed_column();
    let column = PagedColumn::from_values(&values, PAGE, &policy()).unwrap();
    let (_, counts) = paged::between(&column, 4_242, 4_242, Planning::Facts, 1).unwrap();
    // Constant pages are accepted, all-null pages and every page whose
    // bound excludes 4,242 are skipped; nothing needs a scan.
    assert!(counts.accepted >= 4, "{counts:?}");
    assert!(counts.skipped >= 12, "{counts:?}");
    let (_, scan_only) = paged::between(&column, 4_242, 4_242, Planning::ScanOnly, 1).unwrap();
    assert_eq!(
        scan_only,
        PageCounts {
            scanned: column.pages().len(),
            ..PageCounts::default()
        }
    );

    for page in column.pages() {
        let mut session = ReadSession::new(&page.column.page, ClosureMode::Selective);
        let action = paged::plan_between(page, &mut session, 0, 100_000, Planning::Facts).unwrap();
        let values = page.column.truth.iter().flatten().collect::<Vec<_>>();
        if values.is_empty() {
            assert!(matches!(action, PageAction::Skip(_)), "all-null page");
        } else if values.windows(2).all(|pair| pair[0] <= pair[1])
            && page.column.truth.iter().all(Option::is_some)
            && !matches!(action, PageAction::Skip(_) | PageAction::Accept(_))
        {
            assert!(
                matches!(action, PageAction::Search { .. }),
                "sorted page: {action:?}"
            );
        }
    }
}

#[test]
fn the_checker_rejects_actions_their_licences_do_not_support() {
    let values = mixed_column();
    let column = PagedColumn::from_values(&values, PAGE, &policy()).unwrap();
    let constant = column
        .pages()
        .iter()
        .find(|page| page.column.truth.iter().all(|value| *value == Some(4_242)))
        .unwrap();
    let mut session = ReadSession::new(&constant.column.page, ClosureMode::Selective);
    let accept =
        paged::plan_between(constant, &mut session, 4_000, 5_000, Planning::Facts).unwrap();
    let PageAction::Accept(licence) = accept.clone() else {
        panic!("a constant page inside the predicate is accepted: {accept:?}")
    };
    // The same licence cannot skip the page, nor accept a predicate that
    // excludes its value.
    assert!(
        paged::check_between(
            constant,
            &mut session,
            &PageAction::Skip(licence.clone()),
            4_000,
            5_000
        )
        .is_err()
    );
    assert!(paged::check_between(constant, &mut session, &accept, 4_300, 5_000).is_err());
    // Evidence that does not match the page fails, as does a fact the page
    // does not prove.
    let stale = Licence {
        evidence: Some((0, 10)),
        ..licence.clone()
    };
    assert!(
        paged::check_between(
            constant,
            &mut session,
            &PageAction::Accept(stale),
            4_000,
            5_000
        )
        .is_err()
    );
    let unproven = Licence {
        fact: Authorization::Fact {
            node: constant.column.decoder.root(),
            scope: FactScope::AllRows,
            property: Property::Value(ValueInvariant::AllNull),
        },
        evidence: None,
    };
    assert!(
        paged::check_between(constant, &mut session, &PageAction::Skip(unproven), 0, 1).is_err()
    );
    let order_as_null_skip = Licence {
        fact: Authorization::Fact {
            node: constant.column.decoder.root(),
            scope: FactScope::AllRows,
            property: Property::Value(ValueInvariant::NonDecreasing),
        },
        evidence: None,
    };
    assert!(
        paged::check_between(
            constant,
            &mut session,
            &PageAction::Skip(order_as_null_skip.clone()),
            0,
            1
        )
        .is_err()
    );
    assert!(
        paged::check_between(
            constant,
            &mut session,
            &PageAction::Search {
                licence: order_as_null_skip,
                nulls: NullPlacement::First,
            },
            0,
            1
        )
        .is_err()
    );
}

#[test]
fn stored_statistics_decide_what_an_exact_zone_map_decides() {
    let values = mixed_column();
    let column = PagedColumn::from_values(&values, PAGE, &policy()).unwrap();
    for (low, high) in predicates() {
        let (mut skipped, mut accepted) = (0, 0);
        for page in values.chunks(PAGE) {
            let present = page.iter().flatten();
            match present.clone().min().zip(present.max()) {
                None => skipped += 1,
                Some((&minimum, &maximum)) if maximum < low || minimum > high => skipped += 1,
                Some((&minimum, &maximum)) if low <= minimum && maximum <= high => accepted += 1,
                Some(_) => {}
            }
        }
        let (_, counts) =
            paged::between(&column, low, high, Planning::StoredStatistics, 1).unwrap();
        assert_eq!(
            (counts.skipped, counts.accepted, counts.scanned),
            (skipped, accepted, column.pages().len() - skipped - accepted),
            "{low}..={high}"
        );
    }
}

#[test]
fn the_checker_holds_statistics_licences_to_the_stored_statistics() {
    let values = mixed_column();
    let column = PagedColumn::from_values(&values, PAGE, &policy()).unwrap();
    let constant = column
        .pages()
        .iter()
        .find(|page| page.column.truth.iter().all(|value| *value == Some(4_242)))
        .unwrap();
    let mut session = ReadSession::new(&constant.column.page, ClosureMode::Selective);
    let accept = paged::plan_between(
        constant,
        &mut session,
        4_000,
        5_000,
        Planning::StoredStatistics,
    )
    .unwrap();
    let PageAction::Accept(licence) = accept.clone() else {
        panic!("a constant page inside the predicate is accepted: {accept:?}")
    };
    assert_eq!(licence.fact, Authorization::StoredStatistics);
    assert!(
        paged::check_between(
            constant,
            &mut session,
            &PageAction::Skip(licence.clone()),
            4_000,
            5_000
        )
        .is_err()
    );
    assert!(paged::check_between(constant, &mut session, &accept, 4_300, 5_000).is_err());
    let forged = Licence {
        evidence: Some((4_000, 5_000)),
        ..licence
    };
    assert!(
        paged::check_between(
            constant,
            &mut session,
            &PageAction::Accept(forged),
            4_000,
            5_000
        )
        .is_err()
    );
    // Equal stored ends on a dense page fix its SUM.
    let plan = paged::plan_sum(constant, &mut session, Planning::StoredStatistics).unwrap();
    assert!(matches!(plan, SumAction::Constant(_)), "{plan:?}");
}

#[test]
fn stale_statistics_plan_wrongly_and_facts_do_not() {
    let values = mixed_column();
    let mut column = PagedColumn::from_values(&values, PAGE, &policy()).unwrap();
    let (low, high) = (4_242, 4_242);
    let truth = truth_ranges(&values, low, high);
    let constant = column
        .pages()
        .iter()
        .position(|page| page.column.truth.iter().all(|value| *value == Some(4_242)))
        .unwrap();
    // Statistics recorded before the page was rewritten: they claim the
    // page lies far above the predicate, so the zone map skips it.
    column
        .replace_statistics(constant, Some((1_000_000, 2_000_000)))
        .unwrap();
    let (stale, _) = paged::between(&column, low, high, Planning::StoredStatistics, 1).unwrap();
    assert_ne!(stale, truth);
    let (facts, _) = paged::between(&column, low, high, Planning::Facts, 1).unwrap();
    assert_eq!(facts, truth);
}

#[test]
fn derived_bounds_contain_every_value_of_their_page() {
    let column = PagedColumn::from_values(&mixed_column(), PAGE, &policy()).unwrap();
    let mut bounded = 0;
    for page in column.pages() {
        let Some((lower, upper)) = paged::derived_bounds(page).unwrap() else {
            continue;
        };
        bounded += 1;
        if let Some((minimum, maximum)) = page.stats {
            assert!(lower <= minimum && maximum <= upper, "{page:?}");
        }
    }
    assert!(
        bounded >= column.pages().len() / 2,
        "{bounded} bounded pages"
    );
}

#[test]
fn appending_seals_new_pages_and_keeps_old_ones() {
    let values = mixed_column();
    let (head, tail) = values.split_at(10 * PAGE);
    let mut column = PagedColumn::from_values(head, PAGE, &policy()).unwrap();
    let before = column.pages()[..10]
        .iter()
        .map(|page| page.column.page.bytes().to_vec())
        .collect::<Vec<_>>();
    column.append(tail, &policy()).unwrap();
    assert_eq!(column.rows(), values.len());
    for (page, bytes) in column.pages()[..10].iter().zip(before) {
        assert_eq!(page.column.page.bytes(), &bytes[..]);
    }
    for (low, high) in predicates() {
        let (ranges, _) = paged::between(&column, low, high, Planning::Facts, 4).unwrap();
        assert_eq!(ranges, truth_ranges(&values, low, high));
    }
}

#[test]
fn an_ordered_column_plans_only_the_pages_that_can_match() {
    let selector_values = (0..40 * PAGE as i64)
        .map(|row| Some(row / 3))
        .collect::<Vec<_>>();
    let value_values = (0..40 * PAGE as i64)
        .map(|row| Some(row % 11))
        .collect::<Vec<_>>();
    let mut selector = PagedColumn::from_values(&selector_values, PAGE, &policy()).unwrap();
    let mut value = PagedColumn::from_values(&value_values, PAGE, &policy()).unwrap();
    assert!(selector.ordered());
    let check = |selector: &PagedColumn,
                 value: &PagedColumn,
                 selected: &[Option<i64>],
                 values: &[Option<i64>]| {
        for (low, high) in [
            (5_000, 5_100),
            (0, 10),
            (13_000, 20_000),
            (-10, -1),
            (100, 100),
        ] {
            let expected = truth_ranges(selected, low, high);
            let sum: i128 = expected
                .iter()
                .flat_map(|range| &values[range.start..range.end])
                .flatten()
                .map(|&value| i128::from(value))
                .sum();
            let rows: usize = expected.iter().map(|range| range.len()).sum();
            for threads in [1, 4] {
                let (got, matched, _) =
                    paged::sum_where(selector, value, low, high, Planning::Facts, threads).unwrap();
                assert_eq!((got, matched), (sum, rows), "{low}..={high} x{threads}");
            }
        }
    };
    check(&selector, &value, &selector_values, &value_values);
    let (_, _, counts) =
        paged::sum_where(&selector, &value, 5_000, 5_010, Planning::Facts, 1).unwrap();
    assert!(
        counts.skipped >= 38,
        "a narrow predicate plans at most two pages: {counts:?}"
    );

    // A page that starts below the previous page's end breaks the column
    // certificate; answers stay exact through per-page planning.
    let tail = vec![Some(0_i64); PAGE];
    selector.append(&tail, &policy()).unwrap();
    value.append(&tail, &policy()).unwrap();
    assert!(!selector.ordered());
    let selected = [selector_values, tail.clone()].concat();
    let values = [value_values, tail].concat();
    check(&selector, &value, &selected, &values);
}

#[test]
fn a_constant_value_page_sums_without_decoding() {
    let selector_values = (0..4 * PAGE as i64).map(Some).collect::<Vec<_>>();
    let value_values = vec![Some(7_i64); 4 * PAGE];
    let selector = PagedColumn::from_values(&selector_values, PAGE, &policy()).unwrap();
    let value = PagedColumn::from_values(&value_values, PAGE, &policy()).unwrap();
    let page = &value.pages()[0];
    let mut session = ReadSession::new(&page.column.page, ClosureMode::Selective);
    let action = paged::plan_sum(page, &mut session, Planning::Facts).unwrap();
    assert!(matches!(action, SumAction::Constant(_)), "{action:?}");
    let (sum, rows, counts) =
        paged::sum_where(&selector, &value, 100, 3_000, Planning::Facts, 2).unwrap();
    assert_eq!((sum, rows), (7 * 2_901, 2_901));
    assert_eq!(counts.constant_sums, 3);
}

#[test]
fn ablation_planners_take_only_the_actions_they_are_allowed() {
    let values = mixed_column();
    let column = PagedColumn::from_values(&values, PAGE, &policy()).unwrap();
    for (low, high) in predicates() {
        let (_, bounds) = paged::between(&column, low, high, Planning::FactBounds, 1).unwrap();
        assert_eq!(
            (bounds.searched, bounds.translated),
            (0, 0),
            "{low}..={high}"
        );
        let (_, zone) = paged::between(&column, low, high, Planning::StoredStatistics, 1).unwrap();
        let (_, plus) =
            paged::between(&column, low, high, Planning::StoredStatisticsPlus, 1).unwrap();
        // The zone map's skips and accepts, plus shortcuts on the rest.
        assert_eq!((plus.skipped, plus.accepted), (zone.skipped, zone.accepted));
        assert!(plus.scanned <= zone.scanned);
    }
}

#[test]
fn a_derived_footer_is_checked_on_every_page_the_query_reads() {
    let values = mixed_column();
    let measure = (0..values.len())
        .map(|row| Some(row as i64 % 50))
        .collect::<Vec<_>>();
    let mut column = PagedColumn::from_values(&values, PAGE, &policy()).unwrap();
    let value = PagedColumn::from_values(&measure, PAGE, &policy()).unwrap();
    // The footer holds each page's derived bounds.
    for index in 0..column.pages().len() {
        let bound = paged::derived_bounds(&column.pages()[index]).unwrap();
        column.replace_statistics(index, bound).unwrap();
    }
    let truth = |low: i64, high: i64| {
        values
            .iter()
            .zip(&measure)
            .fold(
                (0_i128, 0_usize),
                |(sum, rows), (value, measure)| match value {
                    Some(value) if low <= *value && *value <= high => {
                        (sum + i128::from(measure.unwrap()), rows + 1)
                    }
                    _ => (sum, rows),
                },
            )
    };
    for (low, high) in predicates() {
        let (sum, rows, _) =
            paged::sum_where(&column, &value, low, high, Planning::DerivedFooter, 1).unwrap();
        assert_eq!((sum, rows), truth(low, high), "{low}..={high}");
    }
    // A footer entry that disagrees with a page the query reads fails the
    // query; one for a page it skips without reading is trusted.
    let scanned = column
        .pages()
        .iter()
        // A bound wide enough that a two-value predicate at its lower end
        // must scan the page.
        .position(|page| page.stats.is_some_and(|(low, high)| high - low > 4))
        .unwrap();
    let original = column.pages()[scanned].stats;
    let (low, high) = original.unwrap();
    let mut stale = column.clone();
    stale
        .replace_statistics(scanned, Some((low - 1, high + 1)))
        .unwrap();
    let (query_low, query_high) = (low as i64, low as i64 + 1);
    assert!(
        paged::sum_where(
            &stale,
            &value,
            query_low,
            query_high,
            Planning::DerivedFooter,
            1
        )
        .is_err()
    );
    let mut misleading = column.clone();
    misleading
        .replace_statistics(scanned, Some((high + 10, high + 20)))
        .unwrap();
    let (sum, rows, _) = paged::sum_where(
        &misleading,
        &value,
        query_low,
        query_high,
        Planning::DerivedFooter,
        1,
    )
    .unwrap();
    assert_ne!((sum, rows), truth(query_low, query_high));
}
