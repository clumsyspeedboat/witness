//! Every paged query path against a brute-force reading of the same rows:
//! columns grown by uneven appends (so short pages sit inside a column),
//! every encoding menu, every planner, several thread counts, deleted rows,
//! conjunctions with each aggregate, and pages read back from files.

use std::fs::{self, File};

use proptest::prelude::*;
use witness::access_compiler::paged::{
    self, Aggregate, Conjunct, DeletionVector, FileStore, PagePolicy, PagedColumn, Planning,
    WholePageFileStore,
};
use witness::access_compiler::{Recipe, paged_file, push_range};

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

/// The paged studies' direct menu, the same menu with delta chains, patches
/// and frames, and a menu of one framed recipe.
fn menu(choice: u8) -> PagePolicy {
    let direct = vec![
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
    ];
    match choice % 3 {
        0 => PagePolicy::smallest(direct),
        1 => {
            let mut menu = direct;
            menu.extend([
                Recipe::Delta {
                    restart_interval: 64,
                    deltas: bitpack(),
                },
                Recipe::UnsignedDelta {
                    restart_interval: 64,
                    deltas: bitpack(),
                },
                Recipe::Patch {
                    index_interval: 16,
                    values: Box::new(Recipe::For(bitpack())),
                },
                Recipe::Frame(Box::new(Recipe::For(bitpack()))),
            ]);
            PagePolicy::smallest(menu)
        }
        _ => PagePolicy::smallest(vec![
            Recipe::ChunkedFrame {
                chunk_bytes: 512,
                values: Box::new(Recipe::For(bitpack())),
            },
            Recipe::Dictionary(bitpack()),
        ]),
    }
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

/// `rows` values of one shape, continuing from `base` so that ordered
/// shapes stay ordered across appends.
fn chunk(random: &mut Random, rows: usize, shape: u8, base: &mut i64) -> Vec<Option<i64>> {
    (0..rows)
        .map(|row| match shape % 8 {
            0 => {
                *base += (random.next() % 3) as i64;
                Some(*base)
            }
            1 => Some(*base + row as i64 - 500),
            2 => Some((row / 113) as i64 * 7 + *base % 5),
            3 => Some((random.next() % 17) as i64 * 13),
            4 => Some(random.next() as i64 - (1 << 30)),
            5 => (!random.next().is_multiple_of(3)).then(|| (random.next() % 4_000) as i64),
            6 => None,
            _ => Some((row % 50) as i64 + (row / 50) as i64),
        })
        .collect()
}

struct Table {
    columns: [PagedColumn; 3],
    values: [Vec<Option<i64>>; 3],
}

/// Three columns paged alike, grown by the same uneven appends.
fn table(
    seed: u64,
    page_rows: usize,
    appends: &[usize],
    shapes: [u8; 3],
    menu: &PagePolicy,
) -> Table {
    let mut random = Random(seed);
    let mut columns = [0, 1, 2].map(|_| PagedColumn::new(page_rows).unwrap());
    let mut values: [Vec<Option<i64>>; 3] = Default::default();
    let mut bases = [0_i64, 10_000, -3_000];
    for (step, &rows) in appends.iter().enumerate() {
        for slot in 0..3 {
            // An all-null shape only for some appends, so most pages hold values.
            let shape = if shapes[slot] % 8 == 6 && step % 2 == 0 {
                5
            } else {
                shapes[slot]
            };
            let part = chunk(&mut random, rows, shape, &mut bases[slot]);
            columns[slot].append(&part, menu).unwrap();
            values[slot].extend(part);
        }
    }
    Table { columns, values }
}

fn live(deleted: &DeletionVector, row: usize) -> bool {
    !deleted.is_deleted(row)
}

fn inside(value: Option<i64>, (low, high): (i64, i64)) -> bool {
    value.is_some_and(|value| low <= value && value <= high)
}

fn footer(column: &PagedColumn) -> PagedColumn {
    let mut column = column.clone();
    for index in 0..column.pages().len() {
        let bound = paged::derived_bounds(&column.pages()[index]).unwrap();
        column.replace_statistics(index, bound).unwrap();
    }
    column
}

fn deletions(column: &PagedColumn, seed: u64, per_mille: u64) -> DeletionVector {
    let mut deleted = DeletionVector::for_column(column);
    let mut random = Random(seed);
    for row in 0..column.rows() {
        if random.next() % 1_000 < per_mille {
            deleted.delete(row).unwrap();
        }
    }
    deleted
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("witness-differential-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn every_paged_query_path_answers_as_a_scan_of_the_rows(
        seed in any::<u64>(),
        page_rows in prop_oneof![Just(64_usize), Just(100), 1_usize..1_500],
        appends in prop::collection::vec(1_usize..2_000, 1..5),
        shapes in [0_u8..8, 0_u8..8, 0_u8..8],
        menu_choice in 0_u8..3,
        per_mille in prop_oneof![Just(0_u64), Just(10), Just(300), 0_u64..1_000],
        low_share in 0.0_f64..1.0,
        width_share in 0.0_f64..0.7,
        other_share in 0.0_f64..1.0,
    ) {
        let policy = menu(menu_choice);
        let Table { columns, values } = table(seed, page_rows, &appends, shapes, &policy);
        let [selector, value, other] = &columns;
        let deleted = deletions(selector, seed ^ 0xd1e, per_mille);
        let window = |column: &[Option<i64>], from: f64, width: f64| {
            let mut present = column.iter().flatten().copied().collect::<Vec<_>>();
            present.sort_unstable();
            if present.is_empty() {
                return (0, 0);
            }
            let at = |share: f64| present[((present.len() - 1) as f64 * share.min(1.0)) as usize];
            (at(from), at(from + width))
        };
        let predicate = window(&values[0], low_share, width_share);
        let second = window(&values[2], other_share, 0.5);

        // Brute force over the rows that remain.
        let (mut sum, mut matched) = (0_i128, 0_usize);
        let (mut both_sum, mut both_rows, mut product) = (0_i128, 0_usize, 0_i128);
        let mut spans = Vec::new();
        let rows_of = values[0].iter().zip(&values[1]).zip(&values[2]).enumerate();
        for (row, ((&selected, &summed), &constrained)) in rows_of {
            if inside(selected, predicate) {
                push_range(&mut spans, row, row + 1);
            }
            if !live(&deleted, row) || !inside(selected, predicate) {
                continue;
            }
            matched += 1;
            sum += summed.map_or(0, i128::from);
            if inside(constrained, second) {
                both_rows += 1;
                both_sum += summed.map_or(0, i128::from);
                if let (Some(x), Some(y)) = (summed, constrained) {
                    product += i128::from(x) * i128::from(y);
                }
            }
        }
        let none = DeletionVector::for_column(selector);
        let (low, high) = predicate;
        for planning in PLANNERS {
            let footer_selector;
            let footer_other;
            let (selector, other) = if planning == Planning::DerivedFooter {
                footer_selector = footer(selector);
                footer_other = footer(other);
                (&footer_selector, &footer_other)
            } else {
                (selector, other)
            };
            for threads in [1, 3, 8] {
                let answer = paged::sum_where_live(
                    selector, value, &deleted, low, high, planning, threads,
                ).unwrap();
                prop_assert_eq!((answer.0, answer.1), (sum, matched), "{:?} x{}", planning, threads);
                let plain = paged::sum_where(selector, value, low, high, planning, threads).unwrap();
                let empty = paged::sum_where_live(selector, value, &none, low, high, planning, threads).unwrap();
                prop_assert_eq!(plain, empty);
                let (found, _) = paged::between(selector, low, high, planning, threads).unwrap();
                prop_assert_eq!(&found, &spans);
            }
            let conjuncts = [
                Conjunct { column: selector, low, high },
                Conjunct { column: other, low: second.0, high: second.1 },
            ];
            for threads in [1, 4] {
                let count = paged::aggregate_where_live(&conjuncts, Aggregate::Count, &deleted, planning, threads).unwrap();
                prop_assert_eq!((count.0, count.1), (both_rows as i128, both_rows), "{:?}", planning);
                let summed = paged::aggregate_where_live(&conjuncts, Aggregate::Sum(value), &deleted, planning, threads).unwrap();
                prop_assert_eq!((summed.0, summed.1), (both_sum, both_rows), "{:?}", planning);
                let multiplied = paged::aggregate_where_live(&conjuncts, Aggregate::SumProduct(value, other), &deleted, planning, threads).unwrap();
                prop_assert_eq!((multiplied.0, multiplied.1), (product, both_rows), "{:?}", planning);
            }
        }

        // The same queries over pages read back from files.
        let tag = format!("{seed:x}-{page_rows}-{}", appends.len());
        let (selector_path, value_path) = (scratch(&format!("{tag}-s.wpf")), scratch(&format!("{tag}-v.wpf")));
        let selector_layout = paged_file::write(selector, &selector_path).unwrap();
        let value_layout = paged_file::write(value, &value_path).unwrap();
        let (selector_file, value_file) = (File::open(&selector_path).unwrap(), File::open(&value_path).unwrap());
        let stores = || {
            (
                FileStore { file: &selector_file, offsets: selector_layout.offsets.clone() },
                FileStore { file: &value_file, offsets: value_layout.offsets.clone() },
            )
        };
        for planning in [
            Planning::Facts,
            Planning::StoredStatistics,
            Planning::StoredStatisticsDictionary,
            Planning::ScanOnly,
        ] {
            for threads in [1, 4] {
                let (selector_store, value_store) = stores();
                let stored = paged::sum_where_live_in(
                    (selector, &selector_store), (value, &value_store), &deleted, low, high, planning, threads,
                ).unwrap();
                prop_assert_eq!((stored.0, stored.1), (sum, matched), "file {:?}", planning);
                let (selector_store, value_store) = stores();
                let (whole_selector, whole_value) = (WholePageFileStore(selector_store), WholePageFileStore(value_store));
                let whole = paged::sum_where_live_in(
                    (selector, &whole_selector), (value, &whole_value), &deleted, low, high, planning, threads,
                ).unwrap();
                prop_assert_eq!((whole.0, whole.1), (sum, matched), "whole-page file {:?}", planning);
            }
        }
        fs::remove_file(&selector_path).unwrap();
        fs::remove_file(&value_path).unwrap();
    }
}
