//! The batch kernels that read a page only at the rows a predicate keeps
//! must answer exactly as a full decode does: single-predicate SUM, which
//! switches between selective and full batches, and conjunctions, which read
//! later conjuncts and the aggregated columns only at surviving rows. Also
//! the dictionary span sum, accepted pages with scattered nulls, and the
//! verification of dictionaries when a page is sealed.

use proptest::prelude::*;
use witness::access_compiler::paged::{
    self, Aggregate, Conjunct, PagePolicy, PagedColumn, Planning,
};
use witness::access_compiler::{
    ClosureMode, DecoderIr, DecoderNode, EncodedColumn, FieldLocation, InputColumn, ReadSession,
    Recipe, SerializedPage, Span, encode, vector,
};

const PLANNERS: [Planning; 6] = [
    Planning::Facts,
    Planning::ScanOnly,
    Planning::StoredStatistics,
    Planning::FactBounds,
    Planning::StoredStatisticsPlus,
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

/// One forced recipe, so a test reaches the kernel it means to.
fn only(recipe: Recipe) -> PagePolicy {
    PagePolicy::smallest(vec![recipe])
}

fn next(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state >> 33
}

/// `rows` values from a domain of `domain` distinct values spread over a
/// wide range (or a narrow one when `domain` is small), with one row in
/// `null_every` null (none when 0).
fn column(seed: u64, rows: usize, domain: u64, null_every: u64) -> Vec<Option<i64>> {
    let mut state = seed;
    (0..rows)
        .map(|_| {
            let value = (next(&mut state) % domain) as i64 * 7_919 - 400_000;
            (null_every == 0 || !next(&mut state).is_multiple_of(null_every)).then_some(value)
        })
        .collect()
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

/// A predicate keeping roughly `share` of the selector's non-null values.
fn window(values: &[Option<i64>], share: f64) -> (i64, i64) {
    let mut present = values.iter().flatten().copied().collect::<Vec<_>>();
    present.sort_unstable();
    let width = ((present.len() as f64 * share) as usize).max(1);
    let start = (present.len() - width) / 2;
    (present[start], present[start + width - 1])
}

/// Every planner answers `SUM(value) WHERE low <= selector <= high` exactly.
fn assert_sums(
    selector: &[Option<i64>],
    value: &[Option<i64>],
    page_rows: usize,
    policy: &PagePolicy,
) {
    let mut selector_pages = PagedColumn::from_values(selector, page_rows, policy).unwrap();
    let value_pages = PagedColumn::from_values(value, page_rows, policy).unwrap();
    for share in [0.0005, 0.01, 0.1, 0.5, 1.0] {
        let (low, high) = window(selector, share);
        let expected = truth(selector, value, low, high);
        for planning in PLANNERS {
            if planning == Planning::DerivedFooter {
                // The footer planner plans from derived bounds in place of
                // stored statistics.
                for index in 0..selector_pages.pages().len() {
                    let bound = paged::derived_bounds(&selector_pages.pages()[index]).unwrap();
                    selector_pages.replace_statistics(index, bound).unwrap();
                }
            }
            for threads in [1, 3] {
                let (sum, rows, _) =
                    paged::sum_where(&selector_pages, &value_pages, low, high, planning, threads)
                        .unwrap();
                assert_eq!(
                    (sum, rows),
                    expected,
                    "{planning:?} share {share} x{threads}"
                );
            }
        }
    }
}

#[test]
fn selective_and_full_batches_sum_exactly_on_forced_recipes() {
    // Pages of 3,000 rows end in a partial batch; columns end in a partial page.
    let rows = 7 * 3_000 + 917;
    let recipes = [Recipe::For(bitpack()), Recipe::Dictionary(bitpack())];
    for (selector_recipe, value_recipe) in recipes
        .iter()
        .flat_map(|left| recipes.iter().map(move |right| (left, right)))
    {
        for (null_every, domain) in [(0, 1 << 20), (0, 300), (9, 1 << 20), (9, 300), (2, 50)] {
            let selector = column(11, rows, domain, null_every);
            let value = column(29, rows, domain, null_every);
            let selector_pages =
                PagedColumn::from_values(&selector, 3_000, &only(selector_recipe.clone())).unwrap();
            let value_pages =
                PagedColumn::from_values(&value, 3_000, &only(value_recipe.clone())).unwrap();
            for share in [0.0005, 0.01, 0.1, 0.5] {
                let (low, high) = window(&selector, share);
                for planning in [
                    Planning::Facts,
                    Planning::ScanOnly,
                    Planning::StoredStatistics,
                ] {
                    let (sum, matched, _) =
                        paged::sum_where(&selector_pages, &value_pages, low, high, planning, 1)
                            .unwrap();
                    assert_eq!(
                        (sum, matched),
                        truth(&selector, &value, low, high),
                        "{} over {}, nulls 1/{null_every}, domain {domain}, share {share}, {planning:?}",
                        selector_recipe.name(),
                        value_recipe.name()
                    );
                }
            }
        }
    }
}

#[test]
fn the_direct_menu_sums_exactly_under_every_planner() {
    let rows = 5 * 4_096 + 333;
    for (null_every, domain) in [(0, 1 << 30), (0, 40), (5, 1 << 30), (5, 40), (1_000, 7)] {
        let selector = column(3, rows, domain, null_every);
        let value = column(5, rows, domain, null_every);
        assert_sums(&selector, &value, 4_096, &direct());
    }
}

#[test]
fn an_accepted_page_with_scattered_nulls_sums_exactly() {
    // Every value lies inside the predicate, so every page is accepted and
    // its non-null rows are many short runs.
    let rows = 4 * 4_096;
    let selector = column(41, rows, 1_000, 3);
    let value = column(43, rows, 300, 4);
    let selector_pages = PagedColumn::from_values(&selector, 4_096, &direct()).unwrap();
    let value_pages = PagedColumn::from_values(&value, 4_096, &direct()).unwrap();
    let (low, high) = (i64::MIN + 1, i64::MAX - 1);
    for planning in [Planning::Facts, Planning::StoredStatistics] {
        let (sum, rows, counts) =
            paged::sum_where(&selector_pages, &value_pages, low, high, planning, 1).unwrap();
        assert_eq!((sum, rows), truth(&selector, &value, low, high));
        assert_eq!(counts.accepted, selector_pages.pages().len());
    }
}

#[test]
fn dictionary_values_sum_over_spans_exactly() {
    // A sorted key is searched, so the dictionary value page is summed over
    // the spans the search returns, dense and with nulls.
    let rows = 6 * 2_048;
    let key = (0..rows).map(|row| Some(row as i64)).collect::<Vec<_>>();
    for null_every in [0, 7] {
        let value = column(17, rows, 120, null_every);
        let key_pages = PagedColumn::from_values(&key, 2_048, &direct()).unwrap();
        let value_pages =
            PagedColumn::from_values(&value, 2_048, &only(Recipe::Dictionary(bitpack()))).unwrap();
        for (low, high) in [(100, 5_000), (2_047, 2_048), (0, rows as i64)] {
            let (sum, matched, _) =
                paged::sum_where(&key_pages, &value_pages, low, high, Planning::Facts, 1).unwrap();
            assert_eq!((sum, matched), truth(&key, &value, low, high));
        }
    }
}

/// `SUM(a * b)`, `SUM(a)` and `COUNT(*)` over the rows every conjunct keeps.
// Rows index several parallel columns at once.
#[allow(clippy::needless_range_loop)]
fn conjunction_truth(
    columns: &[Vec<Option<i64>>],
    conjuncts: &[(usize, i64, i64)],
    left: usize,
    right: usize,
) -> (i128, i128, usize) {
    let rows = columns[0].len();
    let (mut product, mut sum, mut count) = (0_i128, 0_i128, 0);
    for row in 0..rows {
        let keep = conjuncts.iter().all(|&(column, low, high)| {
            columns[column][row].is_some_and(|value| low <= value && value <= high)
        });
        if !keep {
            continue;
        }
        count += 1;
        sum += columns[left][row].map_or(0, i128::from);
        if let (Some(a), Some(b)) = (columns[left][row], columns[right][row]) {
            product += i128::from(a) * i128::from(b);
        }
    }
    (product, sum, count)
}

#[test]
fn conjunctions_read_at_surviving_rows_exactly() {
    let rows = 9 * 4_096 + 77;
    let columns = vec![
        column(1, rows, 2_557, 0),
        column(2, rows, 11, 0),
        column(3, rows, 50, 13),
        column(4, rows, 1 << 24, 0),
        column(5, rows, 400, 6),
    ];
    let paged_columns = columns
        .iter()
        .map(|values| PagedColumn::from_values(values, 4_096, &direct()).unwrap())
        .collect::<Vec<_>>();
    let narrow = |column: usize, share: f64| {
        let (low, high) = window(&columns[column], share);
        (column, low, high)
    };
    let queries = [
        vec![narrow(0, 0.14), narrow(1, 0.27), narrow(2, 0.46)],
        vec![narrow(0, 0.01), narrow(2, 0.9)],
        vec![narrow(1, 0.9), narrow(0, 0.6), narrow(4, 0.5)],
        vec![narrow(4, 0.002)],
    ];
    for conjuncts in &queries {
        let expected = conjunction_truth(&columns, conjuncts, 3, 4);
        let terms = conjuncts
            .iter()
            .map(|&(column, low, high)| Conjunct {
                column: &paged_columns[column],
                low,
                high,
            })
            .collect::<Vec<_>>();
        for planning in [
            Planning::Facts,
            Planning::ScanOnly,
            Planning::StoredStatistics,
            Planning::StoredStatisticsPlus,
        ] {
            for threads in [1, 4] {
                let product = paged::aggregate_where(
                    &terms,
                    Aggregate::SumProduct(&paged_columns[3], &paged_columns[4]),
                    planning,
                    threads,
                )
                .unwrap();
                assert_eq!(
                    (product.0, product.1),
                    (expected.0, expected.2),
                    "{planning:?}"
                );
                let sum = paged::aggregate_where(
                    &terms,
                    Aggregate::Sum(&paged_columns[3]),
                    planning,
                    threads,
                )
                .unwrap();
                assert_eq!((sum.0, sum.1), (expected.1, expected.2), "{planning:?}");
                let count =
                    paged::aggregate_where(&terms, Aggregate::Count, planning, threads).unwrap();
                assert_eq!(count.1, expected.2, "{planning:?}");
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn selective_sums_equal_the_truth(
        seed in 0_u64..1_000_000,
        rows in 1_usize..9_000,
        domain_bits in 1_u32..40,
        null_every in prop::sample::select(vec![0_u64, 2, 3, 17, 500]),
        share in prop::sample::select(vec![0.0001_f64, 0.005, 0.03, 0.12, 0.7]),
        page_rows in prop::sample::select(vec![1_000_usize, 1_024, 2_500]),
    ) {
        let selector = column(seed, rows, 1 << domain_bits, null_every);
        let value = column(seed ^ 0x9e37, rows, 1 << (domain_bits / 2 + 1), null_every);
        if selector.iter().all(Option::is_none) {
            return Ok(());
        }
        let (low, high) = window(&selector, share);
        let selector_pages = PagedColumn::from_values(&selector, page_rows, &direct()).unwrap();
        let value_pages = PagedColumn::from_values(&value, page_rows, &direct()).unwrap();
        for planning in [Planning::Facts, Planning::ScanOnly, Planning::StoredStatistics] {
            let (sum, matched, _) =
                paged::sum_where(&selector_pages, &value_pages, low, high, planning, 1).unwrap();
            prop_assert_eq!((sum, matched), truth(&selector, &value, low, high));
        }
    }

    /// A selection sum reads a batch only at its selected rows when they are
    /// few, and unpacks it otherwise; both must add exactly the selected
    /// values, whatever row the selection's first row falls on.
    #[test]
    fn selection_sums_equal_the_truth_however_sparse(
        seed in 0_u64..1_000_000,
        rows in 1_usize..6_000,
        domain_bits in 1_u32..40,
        null_every in prop::sample::select(vec![0_u64, 2, 9]),
        per_mille in prop::sample::select(vec![1_u64, 20, 125, 400, 1_000]),
        recipe in prop::sample::select(vec![0_u8, 1]),
    ) {
        let values = column(seed, rows, 1 << domain_bits, null_every);
        let recipe = if recipe == 0 {
            Recipe::For(bitpack())
        } else {
            Recipe::Dictionary(bitpack())
        };
        let pages = PagedColumn::from_values(&values, rows, &only(recipe)).unwrap();
        let page = &pages.pages()[0];
        let mut state = seed ^ 0x51_ec7;
        let mut selection = vector::Selection::empty(Span::new(0, rows).unwrap());
        let mut expected = 0_i128;
        for (row, value) in values.iter().enumerate() {
            if next(&mut state) % 1_000 < per_mille {
                selection.set_range(row, row + 1);
                expected += value.map_or(0, i128::from);
            }
        }
        let mut session = ReadSession::new_untracked(&page.column.page, ClosureMode::Selective);
        let sum = vector::sum_selection(&page.column.decoder, &mut session, &selection).unwrap();
        prop_assert_eq!(sum, expected);
    }
}

/// A copy of `column` whose page bytes have two dictionary entries swapped.
fn with_swapped_entries(column: &EncodedColumn) -> EncodedColumn {
    let DecoderNode::Dictionary { dictionary, .. } =
        column.decoder.node(column.decoder.root()).unwrap().clone()
    else {
        panic!("expected a dictionary root");
    };
    let field = &column.page.layout().fields[dictionary.0];
    let FieldLocation::Direct { offset } = field.location else {
        panic!("expected a direct dictionary field");
    };
    let mut bytes = column.page.bytes().to_vec();
    let (first, second) = (offset, offset + 8);
    for index in 0..8 {
        bytes.swap(first + index, second + index);
    }
    EncodedColumn {
        recipe: column.recipe.clone(),
        decoder: column.decoder.clone(),
        page: SerializedPage::parse(bytes).unwrap(),
        truth: column.truth.clone(),
    }
}

#[test]
fn sealing_withdraws_the_order_of_an_unsorted_dictionary() {
    let values = (0..2_000)
        .map(|row| Some((row % 37) as i64 * 11))
        .collect::<Vec<_>>();
    let encoded = encode(
        &Recipe::Dictionary(bitpack()),
        InputColumn::nullable(values),
    )
    .unwrap();
    assert!(paged::dictionaries_sorted(&encoded).unwrap());
    let mut corrupt = with_swapped_entries(&encoded);
    assert!(!paged::dictionaries_sorted(&corrupt).unwrap());
    paged::verify_dictionaries(&mut corrupt).unwrap();
    assert!(matches!(
        corrupt.decoder.node(corrupt.decoder.root()).unwrap(),
        DecoderNode::Dictionary {
            sorted_unique: false,
            ..
        }
    ));
}

#[test]
fn sealing_rejects_an_id_outside_its_table() {
    let values = (0..2_000)
        .map(|row| Some((row % 37) as i64))
        .collect::<Vec<_>>();
    let mut encoded = encode(
        &Recipe::Dictionary(bitpack()),
        InputColumn::nullable(values),
    )
    .unwrap();
    // Claim one entry fewer than the ids use.
    let mut nodes = encoded.decoder.nodes().to_vec();
    for node in &mut nodes {
        if let DecoderNode::Dictionary { entries, .. } = node {
            *entries -= 1;
        }
    }
    encoded.decoder = DecoderIr::new(nodes, encoded.decoder.root()).unwrap();
    assert!(paged::verify_dictionaries(&mut encoded).is_err());
    // An intact page passes, and decodes as before.
    let values = (0..2_000)
        .map(|row| Some((row % 37) as i64))
        .collect::<Vec<_>>();
    let mut intact = encode(
        &Recipe::Dictionary(bitpack()),
        InputColumn::nullable(values),
    )
    .unwrap();
    paged::verify_dictionaries(&mut intact).unwrap();
    let mut session = ReadSession::new_untracked(&intact.page, ClosureMode::Selective);
    assert_eq!(
        witness::access_compiler::vector::get(&intact.decoder, &mut session, 5).unwrap(),
        Some(5)
    );
}
