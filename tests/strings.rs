use std::ops::Bound;

use witness::access_compiler::Recipe;
use witness::access_compiler::paged::{PagePolicy, PagedColumn, Planning};
use witness::access_compiler::strings::{
    self, StringAction, StringColumn, StringRange, check_string, plan_string, prefix_range,
    string_sum_where,
};

const PAGE: usize = 512;

fn policy() -> PagePolicy {
    let bitpack = || Box::new(Recipe::BitPack);
    PagePolicy::smallest(vec![
        Recipe::For(bitpack()),
        Recipe::Rle {
            index_interval: 32,
            values: Box::new(Recipe::For(bitpack())),
        },
    ])
}

/// Pages of every kind: many distinct strings, a few repeated ones, all
/// null, one string, and sorted runs; with sprinkled nulls and bytes above
/// 0x7f.
fn values() -> Vec<Option<Vec<u8>>> {
    let mut state = 5_u64;
    let mut next = move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        state >> 33
    };
    let mut out = Vec::new();
    for page in 0..16 {
        for row in 0..PAGE {
            out.push(match page % 5 {
                0 => Some(format!("k{:05}", next() % 100_000).into_bytes()),
                1 => (next() % 7 != 0).then(|| {
                    ["apple", "apricot", "banana", "cherry"][(next() % 4) as usize]
                        .as_bytes()
                        .to_vec()
                }),
                2 => None,
                3 => Some(b"only".to_vec()),
                _ => Some(vec![b'z', 0xff, (row % 251) as u8]),
            });
        }
    }
    out.truncate(out.len() - 100);
    out
}

/// A string range with owned bounds.
type OwnedRange = (Bound<Vec<u8>>, Bound<Vec<u8>>);

fn ranges() -> Vec<OwnedRange> {
    let (apple, next) = prefix_range(b"ap");
    vec![
        (
            Bound::Included(b"k10000".to_vec()),
            Bound::Included(b"k20000".to_vec()),
        ),
        (
            Bound::Included(apple),
            next.map_or(Bound::Unbounded, Bound::Excluded),
        ),
        (
            Bound::Included(b"only".to_vec()),
            Bound::Included(b"only".to_vec()),
        ),
        (Bound::Excluded(b"banana".to_vec()), Bound::Unbounded),
        (Bound::Unbounded, Bound::Excluded(b"a".to_vec())),
        (Bound::Included(b"z".to_vec()), Bound::Unbounded),
        (
            Bound::Included(b"apz".to_vec()),
            Bound::Included(b"b".to_vec()),
        ),
        (Bound::Unbounded, Bound::Unbounded),
    ]
}

fn as_range(range: &OwnedRange) -> StringRange<'_> {
    StringRange {
        low: strings::bound_view(&range.0),
        high: strings::bound_view(&range.1),
    }
}

#[test]
fn string_predicates_equal_the_truth_under_every_planner() {
    let values = values();
    let views = values
        .iter()
        .map(|value| value.as_deref())
        .collect::<Vec<_>>();
    let column = StringColumn::from_values(&views, PAGE, &policy()).unwrap();
    let measure = (0..values.len())
        .map(|row| (row % 13 != 0).then_some(row as i64 % 1_000 - 300))
        .collect::<Vec<_>>();
    let measure = PagedColumn::from_values(&measure, PAGE, &policy()).unwrap();
    for range in ranges() {
        let view = as_range(&range);
        let (mut sum, mut matched) = (0_i128, 0_usize);
        for (row, value) in values.iter().enumerate() {
            if value.as_deref().is_some_and(|value| view.contains(value)) {
                matched += 1;
                if row % 13 != 0 {
                    sum += i128::from(row as i64 % 1_000 - 300);
                }
            }
        }
        let mut probes = Vec::new();
        for planning in [
            Planning::Facts,
            Planning::StoredStatistics,
            Planning::ScanOnly,
            Planning::FactBounds,
            Planning::StoredStatisticsPlus,
            Planning::DerivedFooter,
        ] {
            let (total, rows, _, read) =
                string_sum_where(&column, view, &measure, planning).unwrap();
            assert_eq!((total, rows), (sum, matched), "{range:?} {planning:?}");
            probes.push(read);
        }
        // Facts read a logarithmic number of entries per page; a scan reads
        // every entry of every page it does not skip.
        assert!(probes[0] <= probes[2], "{range:?} {probes:?}");
    }
}

#[test]
fn prefix_ranges_hold_exactly_the_extensions_of_their_prefix() {
    for prefix in [&b"ap"[..], b"a\xff", b"\xff\xff", b""] {
        let (low, high) = prefix_range(prefix);
        let range = StringRange {
            low: Bound::Included(&low),
            high: high.as_deref().map_or(Bound::Unbounded, Bound::Excluded),
        };
        for candidate in [
            &b"a"[..],
            b"ap",
            b"apple",
            b"aq",
            b"a\xff",
            b"a\xff\x01",
            b"b",
            b"\xff\xff\x00",
            b"\xff\xfe",
        ] {
            assert_eq!(
                range.contains(candidate),
                candidate.starts_with(prefix),
                "{prefix:?} {candidate:?}"
            );
        }
    }
}

#[test]
fn the_string_checker_rejects_plans_its_dictionary_does_not_support() {
    let values = values();
    let views = values
        .iter()
        .map(|value| value.as_deref())
        .collect::<Vec<_>>();
    let column = StringColumn::from_values(&views, PAGE, &policy()).unwrap();
    let (low, high) = (b"k10000".to_vec(), b"k20000".to_vec());
    let range = StringRange::between(&low, &high);
    let mut probes = 0;
    let action = plan_string(&column, 0, range, Planning::Facts, &mut probes).unwrap();
    let StringAction::Translate { first, last } = action else {
        panic!("a page of many keys translates the range: {action:?}");
    };
    // Probes are logarithmic in the dictionary.
    let entries = column.dictionary(0).len();
    assert!(probes <= 2 * (usize::BITS - entries.leading_zeros()) as usize);
    let dictionary = column.dictionary(0);
    for wrong in [
        StringAction::Translate {
            first: first + 1,
            last,
        },
        StringAction::Translate {
            first,
            last: last + 1,
        },
        StringAction::Translate {
            first: first.saturating_sub(1),
            last,
        },
        StringAction::SkipAt(first),
        StringAction::Accept,
        StringAction::Skip,
    ] {
        assert!(
            check_string(dictionary, wrong, range, Planning::Facts).is_err(),
            "{wrong:?}"
        );
    }
    assert!(check_string(dictionary, action, range, Planning::Facts).is_ok());
    // Only dictionary order licenses translations and inner skips.
    assert!(check_string(dictionary, action, range, Planning::StoredStatistics).is_err());
    let (gap_low, gap_high) = (b"apz".to_vec(), b"b".to_vec());
    let gap = StringRange::between(&gap_low, &gap_high);
    let fruit = column.dictionary(1);
    let mut probes = 0;
    let skip = plan_string(&column, 1, gap, Planning::Facts, &mut probes).unwrap();
    assert!(matches!(skip, StringAction::SkipAt(_)), "{skip:?}");
    assert!(check_string(fruit, skip, gap, Planning::StoredStatistics).is_err());
    assert!(strings::prefix_range(b"").1.is_none());
}
