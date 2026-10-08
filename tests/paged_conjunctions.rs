use witness::access_compiler::Recipe;
use witness::access_compiler::paged::{
    self, Aggregate, Conjunct, PagePolicy, PagedColumn, Planning,
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

fn next(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state >> 33
}

/// Four columns over the same rows: a sorted key, a clustered date with
/// constant pages, a small-domain discount with nulls, and a random quantity.
fn table() -> [Vec<Option<i64>>; 4] {
    let mut state = 7;
    let rows = 40 * PAGE - 123;
    let key = (0..rows).map(|row| Some(row as i64 * 3)).collect();
    let date = (0..rows)
        .map(|row| {
            let page = (row / PAGE) as i64;
            Some(if page % 5 == 2 {
                9_000
            } else {
                page * 100 + (next(&mut state) % 100) as i64
            })
        })
        .collect();
    let discount = (0..rows)
        .map(|_| (!next(&mut state).is_multiple_of(9)).then(|| (next(&mut state) % 11) as i64))
        .collect();
    let quantity = (0..rows)
        .map(|_| Some((next(&mut state) % 50) as i64 + 1))
        .collect();
    [key, date, discount, quantity]
}

#[test]
fn conjunctive_aggregates_equal_the_truth_under_every_planner() {
    let values = table();
    let columns = values
        .iter()
        .map(|column| PagedColumn::from_values(column, PAGE, &policy()).unwrap())
        .collect::<Vec<_>>();
    let queries: [&[(usize, i64, i64)]; 6] = [
        &[(1, 900, 2_500), (2, 5, 7), (3, 1, 23)],
        &[(0, 30_000, 60_000), (2, 5, 7), (3, 1, 23)],
        &[(1, 9_000, 9_000), (3, 10, 10)],
        &[(0, -10, -1), (2, 0, 10)],
        &[(2, 0, 10)],
        &[(1, i64::MIN, i64::MAX), (0, 0, i64::MAX), (2, 3, 3)],
    ];
    for query in queries {
        let conjuncts = query
            .iter()
            .map(|&(column, low, high)| Conjunct {
                column: &columns[column],
                low,
                high,
            })
            .collect::<Vec<_>>();
        let rows = values[0].len();
        let selected = (0..rows)
            .filter(|&row| {
                query.iter().all(|&(column, low, high)| {
                    values[column][row].is_some_and(|value| low <= value && value <= high)
                })
            })
            .collect::<Vec<_>>();
        let sum = |column: usize| {
            selected
                .iter()
                .filter_map(|&row| values[column][row])
                .map(i128::from)
                .sum::<i128>()
        };
        let product = selected
            .iter()
            .filter_map(|&row| Some(i128::from(values[3][row]?) * i128::from(values[2][row]?)))
            .sum::<i128>();
        for planning in [
            Planning::Facts,
            Planning::StoredStatistics,
            Planning::ScanOnly,
        ] {
            for threads in [1, 8] {
                for (aggregate, expected) in [
                    (Aggregate::Count, selected.len() as i128),
                    (Aggregate::Sum(&columns[3]), sum(3)),
                    (Aggregate::Sum(&columns[1]), sum(1)),
                    (Aggregate::SumProduct(&columns[3], &columns[2]), product),
                ] {
                    let (value, matched, _) =
                        paged::aggregate_where(&conjuncts, aggregate, planning, threads).unwrap();
                    assert_eq!(
                        (value, matched),
                        (expected, selected.len()),
                        "{query:?} {aggregate:?} {planning:?} x{threads}"
                    );
                }
            }
        }
    }
}

#[test]
fn a_single_conjunct_matches_sum_where() {
    let values = table();
    let columns = values
        .iter()
        .map(|column| PagedColumn::from_values(column, PAGE, &policy()).unwrap())
        .collect::<Vec<_>>();
    for (low, high) in [(900, 2_500), (9_000, 9_000), (-5, -1)] {
        for planning in [
            Planning::Facts,
            Planning::StoredStatistics,
            Planning::ScanOnly,
        ] {
            let single =
                paged::sum_where(&columns[1], &columns[3], low, high, planning, 1).unwrap();
            let conjunctive = paged::aggregate_where(
                &[Conjunct {
                    column: &columns[1],
                    low,
                    high,
                }],
                Aggregate::Sum(&columns[3]),
                planning,
                1,
            )
            .unwrap();
            assert_eq!((single.0, single.1), (conjunctive.0, conjunctive.1));
        }
    }
}

#[test]
fn facts_skip_pages_any_conjunct_excludes() {
    let values = table();
    let columns = values
        .iter()
        .map(|column| PagedColumn::from_values(column, PAGE, &policy()).unwrap())
        .collect::<Vec<_>>();
    // The key is ordered across pages, so a narrow key range leaves a few
    // pages; the other conjunct cannot widen that.
    let conjuncts = [
        Conjunct {
            column: &columns[3],
            low: 1,
            high: 50,
        },
        Conjunct {
            column: &columns[0],
            low: 3 * 5_000,
            high: 3 * 6_000,
        },
    ];
    let (_, _, counts) =
        paged::aggregate_where(&conjuncts, Aggregate::Count, Planning::Facts, 1).unwrap();
    assert!(counts.skipped >= 36, "{counts:?}");
    let (_, _, scan) =
        paged::aggregate_where(&conjuncts, Aggregate::Count, Planning::ScanOnly, 1).unwrap();
    assert_eq!(scan.skipped, 0);
    assert!(paged::aggregate_where(&[], Aggregate::Count, Planning::Facts, 1).is_err());
}
