//! What a search or a code translation finds is checked before it selects a
//! row: a few reads on either side of its boundaries confirm the result, so
//! an executor that moves a boundary by one row or code is stopped. Also the
//! forged attestations of the fault study, and what the checks can and
//! cannot stop when a page's order is forged.

use proptest::prelude::*;
use witness::access_compiler::paged::{self, Page, PageAction, PagePolicy, PagedColumn, Planning};
use witness::access_compiler::{
    CheckedInvariants, ClosureMode, NullPlacement, ReadSession, Recipe, Span, push_range, vector,
};

fn bitpack() -> Box<Recipe> {
    Box::new(Recipe::BitPack)
}

fn only(recipe: Recipe) -> PagePolicy {
    PagePolicy::smallest(vec![recipe])
}

fn session(page: &Page) -> ReadSession<'_> {
    ReadSession::new_untracked(&page.column.page, ClosureMode::Selective)
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

/// Each boundary of `[first, end)` moved by one either way, within `limit`.
fn shifts((first, end): (usize, usize), limit: usize) -> Vec<(usize, usize)> {
    [
        (first.checked_sub(1), Some(end)),
        (Some(first + 1), Some(end)),
        (Some(first), end.checked_sub(1)),
        (Some(first), Some(end + 1)),
    ]
    .into_iter()
    .filter_map(|(first, end)| first.zip(end))
    .filter(|&(first, end)| first <= limit && end <= limit)
    .collect()
}

/// The rows a search that found `[start, end)` selects; `None` when the
/// span is inverted, which no executor could select.
fn search_spans((start, end): (usize, usize)) -> Option<Vec<Span>> {
    match start.cmp(&end) {
        std::cmp::Ordering::Less => Some(vec![Span::new(start, end).unwrap()]),
        std::cmp::Ordering::Equal => Some(Vec::new()),
        std::cmp::Ordering::Greater => None,
    }
}

/// Checks the search's own result and every result one boundary away: the
/// checker accepts the first and rejects each move that changes the answer.
/// Returns how many moves were harmful.
fn search_shifts_are_rejected(
    page: &Page,
    values: &[Option<i64>],
    nulls: NullPlacement,
    low: i64,
    high: i64,
) -> usize {
    let mut session = session(page);
    let all = Span::new(0, page.rows()).unwrap();
    let found =
        vector::monotone_bounds(&page.column.decoder, &mut session, all, low, high, nulls).unwrap();
    let truth = truth_ranges(values, low, high);
    assert_eq!(search_spans(found), Some(truth.clone()));
    assert!(paged::check_search(page, &mut session, nulls, low, high, found).is_ok());
    let mut harmful = 0;
    for moved in shifts(found, page.rows()) {
        if search_spans(moved).as_ref() != Some(&truth) {
            harmful += 1;
            assert!(
                paged::check_search(page, &mut session, nulls, low, high, moved).is_err(),
                "{low}..={high}: accepted {moved:?} where the search found {found:?}"
            );
        }
    }
    harmful
}

/// The translation's own code range and every range one code away, as
/// `search_shifts_are_rejected` checks a search.
fn translation_shifts_are_rejected(
    page: &Page,
    values: &[Option<i64>],
    low: i64,
    high: i64,
) -> usize {
    let mut session = session(page);
    let decoder = &page.column.decoder;
    let all = Span::new(0, page.rows()).unwrap();
    let (dictionary, entries) = vector::sorted_table(decoder).unwrap();
    let found = vector::dictionary_codes(&mut session, dictionary, entries, low, high).unwrap();
    let truth = truth_ranges(values, low, high);
    assert_eq!(
        vector::dictionary_code_range(decoder, &mut session, all, found).unwrap(),
        truth
    );
    assert!(paged::check_translation(page, &mut session, low, high, found).is_ok());
    let mut harmful = 0;
    for moved in shifts(found, entries) {
        let answer = vector::dictionary_code_range(decoder, &mut session, all, moved).unwrap();
        if moved.0 > moved.1 || answer != truth {
            harmful += 1;
            assert!(
                paged::check_translation(page, &mut session, low, high, moved).is_err(),
                "{low}..={high}: accepted codes {moved:?} where the table gives {found:?}"
            );
        }
    }
    harmful
}

/// Non-decreasing values with runs, and nulls placed as `nulls` says.
fn ordered(rows: usize, nulls: NullPlacement) -> Vec<Option<i64>> {
    let present = (0..rows as i64).map(|row| Some(row / 3 * 5 - 200));
    let gap = rows / 5;
    match nulls {
        NullPlacement::NoNulls => present.collect(),
        NullPlacement::First => std::iter::repeat_n(None, gap)
            .chain(present.skip(gap))
            .collect(),
        _ => present
            .take(rows - gap)
            .chain(std::iter::repeat_n(None, gap))
            .collect(),
    }
}

#[test]
fn every_search_boundary_moved_by_a_row_is_rejected() {
    for nulls in [
        NullPlacement::NoNulls,
        NullPlacement::First,
        NullPlacement::Last,
    ] {
        let values = ordered(900, nulls);
        let column = PagedColumn::from_values(&values, 900, &only(Recipe::For(bitpack()))).unwrap();
        let page = &column.pages()[0];
        let mut harmful = 0;
        for (low, high) in [
            (-300, -250),
            (-300, -200),
            (-200, -200),
            (-198, -192),
            (0, 0),
            (1, 4),
            (100, 400),
            (1_000, 1_295),
            (1_295, 2_000),
            (i64::MIN, i64::MAX),
        ] {
            let action =
                paged::plan_between(page, &mut session(page), low, high, Planning::Facts).unwrap();
            assert!(
                matches!(action, PageAction::Search { nulls: planned, .. } if planned == nulls)
                    || matches!(action, PageAction::Skip(_) | PageAction::Accept(_)),
                "{nulls:?} {low}..={high}: planned {action:?}"
            );
            harmful += search_shifts_are_rejected(page, &values, nulls, low, high);
        }
        assert!(harmful > 0, "{nulls:?}: no move changed an answer");
    }
}

#[test]
fn a_search_result_checked_under_another_null_placement_is_rejected() {
    let values = ordered(600, NullPlacement::Last);
    let column = PagedColumn::from_values(&values, 600, &only(Recipe::For(bitpack()))).unwrap();
    let page = &column.pages()[0];
    let mut session = session(page);
    let all = Span::new(0, page.rows()).unwrap();
    // Above every value, the matching rows are none, just before the nulls.
    let found = vector::monotone_bounds(
        &page.column.decoder,
        &mut session,
        all,
        10_000,
        20_000,
        NullPlacement::Last,
    )
    .unwrap();
    assert_eq!(found, (480, 480));
    assert!(
        paged::check_search(
            page,
            &mut session,
            NullPlacement::Last,
            10_000,
            20_000,
            found
        )
        .is_ok()
    );
    for nulls in [
        NullPlacement::First,
        NullPlacement::NoNulls,
        NullPlacement::Arbitrary,
    ] {
        assert!(paged::check_search(page, &mut session, nulls, 10_000, 20_000, found).is_err());
    }
    // A span reaching past the page.
    assert!(paged::check_search(page, &mut session, NullPlacement::Last, 0, 1, (0, 601)).is_err());
}

#[test]
fn every_translated_code_moved_by_one_is_rejected() {
    // Multiples of ten, so ranges start and end between entries and on them.
    for nulls in [false, true] {
        let values = (0..1_000_i64)
            .map(|row| (!nulls || row % 7 != 0).then_some((row * 7 % 50) * 10))
            .collect::<Vec<_>>();
        let column =
            PagedColumn::from_values(&values, 1_000, &only(Recipe::Dictionary(bitpack()))).unwrap();
        let page = &column.pages()[0];
        // A nullable page is translated through the dictionary under its
        // nulls, and its result is checked on the same table.
        assert!(matches!(
            paged::plan_between(page, &mut session(page), 5, 15, Planning::Facts).unwrap(),
            PageAction::Translate(_)
        ));
        let mut harmful = 0;
        for (low, high) in [
            (-5, 5),
            (0, 0),
            (5, 15),
            (10, 20),
            (11, 19),
            (100, 400),
            (485, 495),
            (490, 600),
            (500, 600),
        ] {
            harmful += translation_shifts_are_rejected(page, &values, low, high);
        }
        assert!(harmful > 0);
    }
}

#[test]
fn queries_over_searched_and_translated_pages_still_answer_exactly() {
    let mut values = ordered(3_000, NullPlacement::NoNulls);
    values.extend((0..3_000_i64).map(|row| Some(row * 13 % 40 * 25)));
    let policy = PagePolicy::smallest(vec![Recipe::Dictionary(bitpack()), Recipe::For(bitpack())]);
    let selector = PagedColumn::from_values(&values, 1_000, &policy).unwrap();
    let value = PagedColumn::from_values(&values, 1_000, &policy).unwrap();
    for (low, high) in [(0, 500), (-200, -150), (125, 125), (330, 870)] {
        let truth = truth_ranges(&values, low, high);
        let rows = truth.iter().map(|span| span.len()).sum::<usize>();
        let sum = values
            .iter()
            .flatten()
            .filter(|&&value| low <= value && value <= high)
            .map(|&value| i128::from(value))
            .sum::<i128>();
        for threads in [1, 4] {
            let (total, matched, counts) =
                paged::sum_where(&selector, &value, low, high, Planning::Facts, threads).unwrap();
            assert_eq!((total, matched), (sum, rows), "{low}..={high}");
            assert!(counts.searched + counts.translated > 0, "{low}..={high}");
        }
        let (spans, _) = paged::between(&selector, low, high, Planning::Facts, 1).unwrap();
        assert_eq!(spans, truth);
    }
}

/// One page that is not ordered, claiming order through a forged descriptor.
fn forged_order(values: &[Option<i64>]) -> PagedColumn {
    let mut column =
        PagedColumn::from_values(values, values.len(), &only(Recipe::For(bitpack()))).unwrap();
    let checked = column.pages()[0].column.page.invariants();
    assert!(!checked.non_decreasing);
    let forged = CheckedInvariants {
        non_decreasing: true,
        ..checked
    };
    assert!(column.forge_descriptor(0, forged).unwrap());
    column
}

#[test]
fn a_forged_order_passes_the_result_check() {
    let values = [1, 10, 2, 3].map(Some);
    let column = forged_order(&values);
    let page = &column.pages()[0];
    assert!(matches!(
        paged::plan_between(page, &mut session(page), 2, 3, Planning::Facts).unwrap(),
        PageAction::Search { .. }
    ));
    // The search finds rows 1..4. The row before lies below 2, the first and
    // last rows found are at least 2 and at most 3, and no row follows: each
    // boundary bears the result out, and row 1, holding 10, is selected.
    let (spans, _) = paged::between(&column, 2, 3, Planning::Facts, 1).unwrap();
    assert_eq!(spans, vec![Span::new(1, 4).unwrap()]);
    assert_ne!(spans, truth_ranges(&values, 2, 3));
    // For 9..=10 the search finds no row, after row 3, which lies below the
    // range; the 10 in row 1 is lost.
    let (sum, matched, _) = paged::sum_where(&column, &column, 9, 10, Planning::Facts, 1).unwrap();
    assert_eq!((sum, matched), (0, 0));
    // A zone map plans without the forged fact and answers exactly.
    let (spans, _) = paged::between(&column, 2, 3, Planning::StoredStatistics, 1).unwrap();
    assert_eq!(spans, truth_ranges(&values, 2, 3));
}

#[test]
fn a_forged_null_placement_licenses_a_search_the_result_check_can_stop() {
    // Ordered values with a null in the middle: placed anywhere, not first.
    let mut values = (0..64_i64).map(Some).collect::<Vec<_>>();
    values[40] = None;
    let mut column = PagedColumn::from_values(&values, 64, &only(Recipe::For(bitpack()))).unwrap();
    let checked = column.pages()[0].column.page.invariants();
    assert_eq!(checked.null_placement, NullPlacement::Arbitrary);
    assert!(checked.non_decreasing_non_null);
    let page = &column.pages()[0];
    assert_eq!(
        paged::plan_between(page, &mut session(page), 30, 50, Planning::Facts).unwrap(),
        PageAction::Scan
    );
    let forged = CheckedInvariants {
        null_placement: NullPlacement::Last,
        ..checked
    };
    assert!(column.forge_descriptor(0, forged).unwrap());
    let page = &column.pages()[0];
    assert!(matches!(
        paged::plan_between(page, &mut session(page), 30, 50, Planning::Facts).unwrap(),
        PageAction::Search {
            nulls: NullPlacement::Last,
            ..
        }
    ));
    // The search for 30..=50 finds rows 30..51 without probing row 40, and
    // its boundary rows bear the result out: the null is selected.
    let (spans, _) = paged::between(&column, 30, 50, Planning::Facts, 1).unwrap();
    assert_eq!(spans, vec![Span::new(30, 51).unwrap()]);
    assert_ne!(spans, truth_ranges(&values, 30, 50));
    // Claimed first instead, the null at row 40 sorts below 39, so the
    // search for 0..=39 ends just after it, and the last row it found holds
    // no value: the check stops the result.
    let forged = CheckedInvariants {
        null_placement: NullPlacement::First,
        ..checked
    };
    column.forge_descriptor(0, forged).unwrap();
    assert!(paged::between(&column, 0, 39, Planning::Facts, 1).is_err());
    assert!(paged::sum_where(&column, &column, 0, 39, Planning::Facts, 1).is_err());
}

#[test]
fn a_forged_dictionary_order_reaches_the_planner_but_not_a_seal() {
    let values = (0..2_000_i64)
        .map(|row| Some((row * 7 % 40 - 20) * 10))
        .collect::<Vec<_>>();
    let mut column =
        PagedColumn::from_values(&values, 2_000, &only(Recipe::Dictionary(bitpack()))).unwrap();
    let mut table = values.iter().flatten().copied().collect::<Vec<_>>();
    table.sort_unstable();
    table.dedup();
    // The page's own order is no forgery.
    assert!(!column.forge_dictionary(0, &table).unwrap());
    assert!(column.forge_dictionary(0, &table[1..]).is_err());
    table.reverse();
    assert!(column.forge_dictionary(0, &table).unwrap());
    let page = &column.pages()[0];
    // The page decodes to the same values, and claims a sorted dictionary
    // that sealing it would have refused.
    let mut reader = session(page);
    for (row, value) in values.iter().enumerate().step_by(97) {
        assert_eq!(
            vector::get(&page.column.decoder, &mut reader, row).unwrap(),
            value.map(i128::from)
        );
    }
    assert!(!paged::dictionaries_sorted(&page.column).unwrap());
    // A zone map does not read the dictionary's order and answers exactly.
    for (low, high) in [(-200, -100), (0, 0), (35, 95), (-1_000, 1_000)] {
        let (spans, _) = paged::between(&column, low, high, Planning::StoredStatistics, 1).unwrap();
        assert_eq!(spans, truth_ranges(&values, low, high));
    }
    // The fact planner does. Its bound is the table's first and last entry,
    // here the largest and the smallest value, so a range inside them is
    // skipped as though it lay outside; and a search of the table finds
    // codes whose neighbouring entries, the ones it probed, bear them out.
    let (spans, _) = paged::between(&column, 0, 0, Planning::Facts, 1).unwrap();
    assert!(spans.is_empty());
    assert_ne!(spans, truth_ranges(&values, 0, 0));
}

#[test]
fn forged_ends_inside_the_true_ones_are_caught_by_the_certificate() {
    let values = (0..8_000_i64).map(|row| Some(row / 2)).collect::<Vec<_>>();
    let mut column =
        PagedColumn::from_values(&values, 1_000, &only(Recipe::For(bitpack()))).unwrap();
    assert!(column.ordered());
    let (first, last) = column.pages()[3].ends.unwrap();
    let quarter = (last - first) / 4;
    assert!(
        column
            .forge_ends(3, Some((first + quarter, last - quarter)))
            .unwrap()
    );
    // The seams still never decrease, so the certificate stands. A range
    // the forged ends move the page out of has the page just outside it,
    // where the certificate's confirming read finds the page's true end.
    let mut stopped = 0;
    for (low, high) in [(1_500, 1_510), (1_990, 2_100), (1_300, 1_700), (0, 10)] {
        match paged::sum_where(&column, &column, low, high, Planning::Facts, 1) {
            Ok((_, matched, _)) => assert_eq!(
                matched,
                truth_ranges(&values, low, high)
                    .iter()
                    .map(|span| span.len())
                    .sum::<usize>(),
                "{low}..={high}"
            ),
            Err(error) => {
                assert!(error.contains("certificate"), "{error}");
                stopped += 1;
            }
        }
    }
    assert_eq!(stopped, 2);
    // Ends that make a seam decrease end the certificate.
    assert!(column.forge_ends(3, Some((0, 10_000))).unwrap());
    assert!(!column.ordered());
}

/// Non-decreasing values from `seed`, `nulls` placed, with steps of up to
/// `step` and repeats.
fn monotone(
    seed: u64,
    rows: usize,
    step: u64,
    nulls: NullPlacement,
    gap: usize,
) -> Vec<Option<i64>> {
    let mut state = seed;
    let mut value = -((seed % 1_000) as i64);
    let mut present = Vec::with_capacity(rows);
    for _ in 0..rows {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        value += ((state >> 33) % (step + 1)) as i64;
        present.push(Some(value));
    }
    let gap = gap.min(rows - 1);
    match nulls {
        NullPlacement::First => present[..gap].fill(None),
        NullPlacement::Last => present[rows - gap..].fill(None),
        _ => {}
    }
    present
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn search_results_moved_by_a_row_never_pass_the_check(
        seed in any::<u64>(),
        rows in 2_usize..2_000,
        step in 0_u64..6,
        nulls in prop_oneof![
            Just(NullPlacement::NoNulls),
            Just(NullPlacement::First),
            Just(NullPlacement::Last),
        ],
        gap in 1_usize..300,
        low in -1_200_i64..6_000,
        width in 0_i64..800,
    ) {
        let values = monotone(seed, rows, step, nulls, gap);
        for recipe in [
            Recipe::For(bitpack()),
            Recipe::Rle { index_interval: 32, values: Box::new(Recipe::For(bitpack())) },
        ] {
            let column = PagedColumn::from_values(&values, rows, &only(recipe)).unwrap();
            let page = &column.pages()[0];
            let checked = page.column.page.invariants();
            let placed = if checked.non_decreasing { NullPlacement::NoNulls } else { checked.null_placement };
            prop_assume!(placed != NullPlacement::Arbitrary);
            search_shifts_are_rejected(page, &values, placed, low, low + width);
        }
    }

    #[test]
    fn translated_codes_moved_by_one_never_pass_the_check(
        seed in any::<u64>(),
        rows in 1_usize..2_000,
        domain in 1_i64..60,
        step in 1_i64..40,
        low in -100_i64..2_500,
        width in 0_i64..600,
    ) {
        let mut state = seed;
        let values = (0..rows)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                Some(((state >> 33) as i64 % domain) * step - step)
            })
            .collect::<Vec<_>>();
        for recipe in [
            Recipe::Dictionary(bitpack()),
            Recipe::Dictionary(Box::new(Recipe::Rle { index_interval: 32, values: bitpack() })),
        ] {
            let column = PagedColumn::from_values(&values, rows, &only(recipe)).unwrap();
            translation_shifts_are_rejected(&column.pages()[0], &values, low, low + width);
        }
    }

    /// The result check confirms the boundaries a search finds, not the
    /// order that licenses it: a binary search reads the rows on either
    /// side of each boundary it settles on, so on any page, ordered or not,
    /// what it finds passes.
    #[test]
    fn a_binary_search_passes_the_result_check_on_any_dense_page(
        seed in any::<u64>(),
        rows in 2_usize..1_000,
        low in -50_i64..1_050,
        width in 0_i64..300,
    ) {
        let mut state = seed;
        let values = (0..rows)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                Some((state >> 33) as i64 % 1_000)
            })
            .collect::<Vec<_>>();
        let mut sorted = values.clone();
        sorted.sort_unstable();
        prop_assume!(sorted != values);
        let column = forged_order(&values);
        let page = &column.pages()[0];
        let mut session = session(page);
        let all = Span::new(0, rows).unwrap();
        let high = low + width;
        let found = vector::monotone_bounds(
            &page.column.decoder, &mut session, all, low, high, NullPlacement::NoNulls,
        ).unwrap();
        prop_assert!(
            paged::check_search(page, &mut session, NullPlacement::NoNulls, low, high, found).is_ok()
        );
    }

    #[test]
    fn a_forged_dictionary_order_never_misleads_a_zone_map(
        seed in any::<u64>(),
        rows in 2_usize..1_500,
        domain in 2_i64..50,
        low in -300_i64..300,
        width in 0_i64..300,
        reversed in any::<bool>(),
    ) {
        let mut state = seed;
        let values = (0..rows)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                ((state >> 33) % 9 != 0).then_some(((state >> 40) as i64 % domain) * 10 - 100)
            })
            .collect::<Vec<_>>();
        let mut column = PagedColumn::from_values(
            &values,
            rows,
            &only(Recipe::For(bitpack())),
        ).unwrap();
        let mut table = values.iter().flatten().copied().collect::<Vec<_>>();
        table.sort_unstable();
        table.dedup();
        if reversed {
            table.reverse();
        } else {
            table.sort_by_key(|&value| value as u64);
        }
        column.forge_dictionary(0, &table).unwrap();
        let high = low + width;
        let (spans, _) = paged::between(&column, low, high, Planning::StoredStatistics, 1).unwrap();
        prop_assert_eq!(spans, truth_ranges(&values, low, high));
        let (sum, matched, _) =
            paged::sum_where(&column, &column, low, high, Planning::StoredStatistics, 1).unwrap();
        let expected = values
            .iter()
            .flatten()
            .filter(|&&value| low <= value && value <= high)
            .map(|&value| i128::from(value))
            .sum::<i128>();
        prop_assert_eq!((sum, matched), (expected, truth_ranges(&values, low, high).iter().map(|span| span.len()).sum::<usize>()));
    }
}
