//! A sorted dictionary lists every value its page holds, so the planner
//! skips a page whose table has no entry in the range even when the page's
//! bounds straddle it, and the checker searches the table again before the
//! skip runs. A zone map with a dictionary filter skips the same pages by
//! testing every entry, without the order, and the checker tests them all
//! again. Also the boundary search that a page of runs answers run by run
//! instead of row by row.

use proptest::prelude::*;
use witness::access_compiler::paged::{
    self, Licence, PageAction, PagePolicy, PagedColumn, Planning,
};
use witness::access_compiler::{
    Authorization, ClosureMode, FactScope, MappingInvariant, NodeId, NullPlacement, Property,
    ReadSession, Recipe, Span, push_range, vector,
};

const PLANNERS: [Planning; 6] = [
    Planning::Facts,
    Planning::ScanOnly,
    Planning::StoredStatistics,
    Planning::FactBounds,
    Planning::StoredStatisticsPlus,
    Planning::StoredStatisticsDictionary,
];

fn bitpack() -> Box<Recipe> {
    Box::new(Recipe::BitPack)
}

fn only(recipe: Recipe) -> PagePolicy {
    PagePolicy::smallest(vec![recipe])
}

fn runs_of(values: Box<Recipe>) -> Box<Recipe> {
    Box::new(Recipe::Rle {
        index_interval: 32,
        values,
    })
}

fn truth(selector: &[Option<i64>], value: &[Option<i64>], low: i64, high: i64) -> (i128, usize) {
    let (mut sum, mut rows) = (0_i128, 0);
    for (selected, value) in selector.iter().zip(value) {
        if selected.is_some_and(|selected| low <= selected && selected <= high) {
            rows += 1;
            sum += value.map_or(0, i128::from);
        }
    }
    (sum, rows)
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

fn session(page: &paged::Page) -> ReadSession<'_> {
    ReadSession::new_untracked(&page.column.page, ClosureMode::Selective)
}

/// One page whose values are the multiples of ten from 10 to 100.
fn tens(nulls: bool) -> PagedColumn {
    let values = (0..1_000_i64)
        .map(|row| (!nulls || row % 7 != 0).then_some((row % 10 + 1) * 10))
        .collect::<Vec<_>>();
    PagedColumn::from_values(&values, 1_000, &only(Recipe::Dictionary(bitpack()))).unwrap()
}

#[test]
fn a_dictionary_page_without_a_value_in_range_is_skipped() {
    let column = tens(false);
    let page = &column.pages()[0];
    let mut session = session(page);
    let action = paged::plan_between(page, &mut session, 11, 19, Planning::Facts).unwrap();
    let PageAction::Skip(licence) = &action else {
        panic!("expected a skip, planned {action:?}");
    };
    assert!(matches!(
        licence.fact,
        Authorization::Fact {
            scope: FactScope::Mapping,
            property: Property::Mapping(MappingInvariant::OrderPreserving),
            ..
        }
    ));
    // Entries below 11 number one, and so do entries at or below 19.
    assert_eq!(licence.evidence, Some((1, 1)));
    // A range that holds an entry compares codes instead.
    assert!(matches!(
        paged::plan_between(page, &mut session, 11, 20, Planning::Facts).unwrap(),
        PageAction::Translate(_)
    ));
    // The skip is a shortcut of the encoding: derived bounds alone scan the
    // page, and a zone map with the encoding's shortcuts skips it too.
    assert_eq!(
        paged::plan_between(page, &mut session, 11, 19, Planning::FactBounds).unwrap(),
        PageAction::Scan
    );
    assert_eq!(
        paged::plan_between(page, &mut session, 11, 19, Planning::StoredStatistics).unwrap(),
        PageAction::Scan
    );
    assert_eq!(
        paged::plan_between(page, &mut session, 11, 19, Planning::StoredStatisticsPlus).unwrap(),
        action
    );
    let (sum, rows, counts) =
        paged::sum_where(&column, &column, 11, 19, Planning::Facts, 1).unwrap();
    assert_eq!(
        (sum, rows, counts.skipped, counts.code_skipped),
        (0, 0, 1, 1)
    );
    let (_, _, counts) =
        paged::sum_where(&column, &column, 11, 19, Planning::FactBounds, 1).unwrap();
    assert_eq!(
        (counts.skipped, counts.code_skipped, counts.scanned),
        (0, 0, 1)
    );
}

#[test]
fn a_nullable_page_skips_on_the_dictionary_under_its_nulls() {
    let column = tens(true);
    let page = &column.pages()[0];
    let mut session = session(page);
    let action = paged::plan_between(page, &mut session, 41, 49, Planning::Facts).unwrap();
    let PageAction::Skip(Licence {
        fact: Authorization::Fact { node, .. },
        evidence,
    }) = action
    else {
        panic!("expected a skip, planned {action:?}");
    };
    assert_ne!(
        node,
        page.column.decoder.root(),
        "cites the dictionary, not the nulls"
    );
    assert_eq!(evidence, Some((4, 4)));
    // With an entry in range, the nullable page is scanned as before.
    assert_eq!(
        paged::plan_between(page, &mut session, 41, 50, Planning::Facts).unwrap(),
        PageAction::Scan
    );
}

#[test]
fn the_checker_rejects_a_dictionary_skip_the_table_does_not_bear_out() {
    let column = tens(false);
    let page = &column.pages()[0];
    let mut session = session(page);
    let skip = paged::plan_between(page, &mut session, 11, 19, Planning::Facts).unwrap();
    let PageAction::Skip(licence) = &skip else {
        panic!("expected a skip, planned {skip:?}");
    };
    assert!(paged::check_between(page, &mut session, &skip, 11, 19).is_ok());
    assert!(paged::check_between(page, &mut session, &skip, 12, 18).is_ok());
    // The same skip for a range that holds an entry.
    assert!(paged::check_between(page, &mut session, &skip, 11, 20).is_err());
    assert!(paged::check_between(page, &mut session, &skip, 0, 1_000).is_err());
    // A skip recording the very code range the table gives, which is not
    // empty: entry 20 matches.
    let forged = PageAction::Skip(Licence {
        evidence: Some((1, 2)),
        ..licence.clone()
    });
    assert!(paged::check_between(page, &mut session, &forged, 11, 20).is_err());
    assert_eq!(
        paged::execute_between(page, &mut session, &forged, 11, 20).unwrap(),
        Vec::new(),
        "run anyway, the forged skip would lose the rows holding 20"
    );
    // A recorded code range that the table does not give.
    for evidence in [Some((1, 2)), Some((0, 0)), Some((2, 2)), None] {
        let moved = PageAction::Skip(Licence {
            evidence,
            ..licence.clone()
        });
        assert!(
            paged::check_between(page, &mut session, &moved, 11, 19).is_err(),
            "{evidence:?}"
        );
    }
    // The licence on a page without a dictionary.
    let values = (0..1_000_i64)
        .map(|row| Some((row % 10 + 1) * 10))
        .collect::<Vec<_>>();
    let offsets = PagedColumn::from_values(&values, 1_000, &only(Recipe::For(bitpack()))).unwrap();
    let other = &offsets.pages()[0];
    assert!(paged::check_between(other, &mut self::session(other), &skip, 11, 19).is_err());
    // Executed anyway, the skip selects nothing, which is the truth here.
    assert!(
        paged::execute_between(page, &mut session, &skip, 11, 19)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_dictionary_filter_skips_a_page_by_testing_every_entry() {
    for nulls in [false, true] {
        let column = tens(nulls);
        let page = &column.pages()[0];
        let mut session = session(page);
        let plan = |session: &mut ReadSession<'_>, low, high, planning| {
            paged::plan_between(page, session, low, high, planning).unwrap()
        };
        let skip = plan(&mut session, 41, 49, Planning::StoredStatisticsDictionary);
        let PageAction::Skip(licence) = &skip else {
            panic!("expected a skip, planned {skip:?}");
        };
        let Authorization::DictionaryEntries { node } = licence.fact else {
            panic!("expected the dictionary's entries, cited {licence:?}");
        };
        assert_eq!(licence.evidence, None);
        // It cites the dictionary, under the nulls on a nullable page.
        assert_eq!(node == page.column.decoder.root(), !nulls);
        // A range that holds an entry, or that the statistics decide, is
        // planned as the zone map plans it.
        for (low, high) in [(41, 50), (0, 5), (10, 100), (0, 1_000), (35, 65)] {
            assert_eq!(
                plan(
                    &mut session,
                    low,
                    high,
                    Planning::StoredStatisticsDictionary
                ),
                plan(&mut session, low, high, Planning::StoredStatistics),
                "{low}..={high}"
            );
        }
        // The checker tests every entry again.
        assert!(paged::check_between(page, &mut session, &skip, 41, 49).is_ok());
        assert!(paged::check_between(page, &mut session, &skip, 42, 48).is_ok());
        for (low, high) in [(41, 50), (40, 49), (0, 1_000)] {
            assert!(
                paged::check_between(page, &mut session, &skip, low, high).is_err(),
                "{low}..={high}"
            );
        }
        // The licence records no evidence, cites the page's own dictionary,
        // and admits no other action.
        let wrong = [
            PageAction::Skip(Licence {
                evidence: Some((4, 4)),
                ..licence.clone()
            }),
            PageAction::Skip(Licence {
                fact: Authorization::DictionaryEntries {
                    node: NodeId(node.0 + 1),
                },
                evidence: None,
            }),
            PageAction::Accept(licence.clone()),
            PageAction::Translate(licence.clone()),
            PageAction::Search {
                licence: licence.clone(),
                nulls: NullPlacement::NoNulls,
            },
        ];
        for action in wrong {
            assert!(
                paged::check_between(page, &mut session, &action, 41, 49).is_err(),
                "{action:?}"
            );
        }
        let (sum, rows, counts) = paged::sum_where(
            &column,
            &column,
            41,
            49,
            Planning::StoredStatisticsDictionary,
            1,
        )
        .unwrap();
        assert_eq!(
            (
                sum,
                rows,
                counts.skipped,
                counts.code_skipped,
                counts.scanned
            ),
            (0, 0, 1, 1, 0)
        );
    }
    // The licence on a page without a dictionary.
    let column = tens(false);
    let page = &column.pages()[0];
    let skip = paged::plan_between(
        page,
        &mut session(page),
        11,
        19,
        Planning::StoredStatisticsDictionary,
    )
    .unwrap();
    let values = (0..1_000_i64)
        .map(|row| Some((row % 10 + 1) * 10))
        .collect::<Vec<_>>();
    let offsets = PagedColumn::from_values(&values, 1_000, &only(Recipe::For(bitpack()))).unwrap();
    let other = &offsets.pages()[0];
    assert!(paged::check_between(other, &mut session(other), &skip, 11, 19).is_err());
}

#[test]
fn a_page_of_runs_is_searched_run_by_run() {
    // Runs of uneven length over a non-decreasing column, spanning several
    // blocks of the run index, with values that repeat across runs.
    let mut values = Vec::new();
    let mut value = -50_i64;
    for run in 0..700_usize {
        value += (run % 3) as i64;
        values.extend(std::iter::repeat_n(Some(value), 1 + run * 7 % 23));
    }
    let rows = values.len();
    let minimum = values[0].unwrap();
    let maximum = values[rows - 1].unwrap();
    for recipe in [
        *runs_of(Box::new(Recipe::For(bitpack()))),
        Recipe::Dictionary(runs_of(bitpack())),
    ] {
        let column = PagedColumn::from_values(&values, rows, &only(recipe.clone())).unwrap();
        let page = &column.pages()[0];
        let mut session = session(page);
        for (low, high) in [
            (minimum - 10, minimum - 1),
            (minimum, minimum),
            (minimum - 5, minimum + 3),
            (0, 0),
            (17, 230),
            (100, 101),
            (maximum, maximum + 4),
            (maximum + 1, maximum + 9),
            (minimum, maximum),
            (i64::MIN, i64::MAX),
        ] {
            let action =
                paged::plan_between(page, &mut session, low, high, Planning::Facts).unwrap();
            let expected = truth_ranges(&values, low, high);
            if let PageAction::Search { .. } = action {
                let spans = paged::execute_between(page, &mut session, &action, low, high).unwrap();
                assert_eq!(spans, expected, "{recipe:?} {low}..={high}");
            }
            let found = vector::search_monotone(
                &page.column.decoder,
                &mut session,
                Span::new(0, rows).unwrap(),
                low,
                high,
                NullPlacement::NoNulls,
            )
            .unwrap();
            assert_eq!(found, expected, "{recipe:?} {low}..={high}");
            // Within a stretch of rows, too.
            let inner = Span::new(rows / 3, 2 * rows / 3).unwrap();
            let found = vector::search_monotone(
                &page.column.decoder,
                &mut session,
                inner,
                low,
                high,
                NullPlacement::NoNulls,
            )
            .unwrap();
            let clipped = expected
                .iter()
                .filter_map(|span| {
                    let (start, end) = (span.start.max(inner.start), span.end.min(inner.end));
                    (start < end).then(|| Span::new(start, end).unwrap())
                })
                .collect::<Vec<_>>();
            assert_eq!(found, clipped, "{recipe:?} {low}..={high} within {inner:?}");
        }
    }
}

/// Values drawn from `domain` multiples of `step`, so most ranges fall
/// between entries, sorted when `sorted`, with one row in `null_every`
/// null (none when 0).
fn gapped(
    seed: u64,
    rows: usize,
    domain: u64,
    step: i64,
    sorted: bool,
    null_every: u64,
) -> Vec<Option<i64>> {
    let mut state = seed;
    let mut next = move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state >> 33
    };
    let mut values = (0..rows)
        .map(|_| (next() % domain) as i64 * step)
        .collect::<Vec<_>>();
    if sorted {
        values.sort_unstable();
    }
    values
        .into_iter()
        .map(|value| (null_every == 0 || !next().is_multiple_of(null_every)).then_some(value))
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn dictionary_skips_answer_exactly_and_never_skip_less_than_a_zone_map(
        seed in any::<u64>(),
        rows in 1_usize..4_000,
        page_rows in 64_usize..1_500,
        domain in 1_u64..40,
        step in 1_i64..50,
        sorted in any::<bool>(),
        null_every in prop_oneof![Just(0_u64), 2_u64..9],
        low in -100_i64..2_100,
        width in 0_i64..400,
    ) {
        let selector = gapped(seed, rows, domain, step, sorted, null_every);
        let value = gapped(seed ^ 0x5eed, rows, 1_000, 3, false, 5);
        let high = low + width;
        let expected = truth(&selector, &value, low, high);
        for (dictionaries_only, policy) in [
            (true, only(Recipe::Dictionary(bitpack()))),
            (true, only(Recipe::Dictionary(runs_of(bitpack())))),
            (false, PagePolicy::smallest(vec![
                Recipe::For(bitpack()),
                Recipe::Dictionary(bitpack()),
                Recipe::Dictionary(runs_of(bitpack())),
                *runs_of(Box::new(Recipe::For(bitpack()))),
            ])),
        ] {
            let selector_pages = PagedColumn::from_values(&selector, page_rows, &policy).unwrap();
            let value_pages = PagedColumn::from_values(&value, page_rows, &policy).unwrap();
            let mut skipped = Vec::new();
            for planning in PLANNERS {
                for threads in [1, 4] {
                    let (sum, matched, counts) = paged::sum_where(
                        &selector_pages, &value_pages, low, high, planning, threads,
                    ).unwrap();
                    prop_assert_eq!((sum, matched), expected, "{:?} x{}", planning, threads);
                    prop_assert!(counts.code_skipped <= counts.skipped);
                    if !planning.uses_shortcuts() && !planning.uses_dictionary_filter() {
                        prop_assert_eq!(counts.code_skipped, 0);
                    }
                    skipped.push((planning, counts.skipped));
                }
            }
            // Facts never skip fewer pages than the encoding's bounds alone.
            let of = |wanted: Planning| skipped.iter().find(|(planning, _)| *planning == wanted).unwrap().1;
            prop_assert!(of(Planning::Facts) >= of(Planning::FactBounds));
            prop_assert!(of(Planning::StoredStatisticsPlus) >= of(Planning::StoredStatistics));
            prop_assert!(of(Planning::StoredStatisticsDictionary) >= of(Planning::StoredStatistics));
            // On dictionary pages, testing every entry skips exactly the
            // pages a search of the sorted table skips.
            if dictionaries_only {
                prop_assert_eq!(
                    of(Planning::StoredStatisticsDictionary),
                    of(Planning::StoredStatisticsPlus)
                );
            }
        }
    }

    #[test]
    fn run_searches_find_the_rows_a_scan_finds(
        seed in any::<u64>(),
        runs in 1_usize..400,
        longest in 1_usize..60,
        low in -20_i64..900,
        width in 0_i64..300,
    ) {
        let mut state = seed;
        let mut next = move || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as usize
        };
        let mut values = Vec::new();
        let mut value = 0_i64;
        for _ in 0..runs {
            value += (next() % 4) as i64;
            values.extend(std::iter::repeat_n(Some(value), 1 + next() % longest));
        }
        let rows = values.len();
        let high = low + width;
        for recipe in [
            *runs_of(Box::new(Recipe::For(bitpack()))),
            Recipe::Dictionary(runs_of(bitpack())),
        ] {
            let column = PagedColumn::from_values(&values, rows, &only(recipe)).unwrap();
            let page = &column.pages()[0];
            let found = vector::search_monotone(
                &page.column.decoder,
                &mut session(page),
                Span::new(0, rows).unwrap(),
                low,
                high,
                NullPlacement::NoNulls,
            ).unwrap();
            prop_assert_eq!(found, truth_ranges(&values, low, high));
        }
    }
}
