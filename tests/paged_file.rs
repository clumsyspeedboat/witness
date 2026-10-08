use std::fs::{self, File};

use witness::access_compiler::Recipe;
use witness::access_compiler::paged::{
    self, FileStore, PagePolicy, PagedColumn, Planning, sum_where_in,
};
use witness::access_compiler::paged_file::{self, Summary};

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
    ])
}

fn columns() -> (Vec<Option<i64>>, Vec<Option<i64>>) {
    let mut state = 11_u64;
    let mut next = move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        state >> 33
    };
    let rows = 30 * PAGE - 77;
    let selector = (0..rows)
        .map(|row| {
            let page = (row / PAGE) as i64;
            match page % 4 {
                0 => Some(page * 1_000 + (next() % 1_000) as i64),
                1 => None,
                2 => Some(7),
                _ => (next() % 5 != 0).then(|| (next() % 100_000) as i64),
            }
        })
        .collect();
    let value = (0..rows)
        .map(|_| Some((next() % 9_999) as i64 - 500))
        .collect();
    (selector, value)
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("witness-paged-file-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

#[test]
fn a_paged_file_round_trips_its_layout_and_summaries() {
    let (selector, _) = columns();
    let column = PagedColumn::from_values(&selector, PAGE, &policy()).unwrap();
    let path = scratch("roundtrip.wpf");
    let written = paged_file::write(&column, &path).unwrap();
    let file = File::open(&path).unwrap();
    assert_eq!(paged_file::open(&file).unwrap(), written);
    let zone_map = paged_file::read_summary(&file, &written, Summary::ZoneMap).unwrap();
    let bounds = paged_file::read_summary(&file, &written, Summary::DerivedBounds).unwrap();
    for ((page, stats), bound) in column.pages().iter().zip(&zone_map).zip(&bounds) {
        assert_eq!(stats.interval, page.stats);
        assert_eq!(stats.non_null, page.non_null);
        assert_eq!(bound.interval, paged::derived_bounds(page).unwrap());
        // A derived bound contains the exact range it summarizes.
        if let (Some((low, high)), Some((lower, upper))) = (page.stats, bound.interval) {
            assert!(lower <= low && high <= upper);
        }
    }
    fs::remove_file(&path).unwrap();
}

#[test]
fn queries_over_a_file_store_equal_queries_in_memory_and_count_reads() {
    let (selector, value) = columns();
    let selector = PagedColumn::from_values(&selector, PAGE, &policy()).unwrap();
    let value = PagedColumn::from_values(&value, PAGE, &policy()).unwrap();
    let (selector_path, value_path) = (scratch("selector.wpf"), scratch("value.wpf"));
    let selector_layout = paged_file::write(&selector, &selector_path).unwrap();
    let value_layout = paged_file::write(&value, &value_path).unwrap();
    let (selector_file, value_file) = (
        File::open(&selector_path).unwrap(),
        File::open(&value_path).unwrap(),
    );
    let selector_store = FileStore {
        file: &selector_file,
        offsets: selector_layout.offsets.clone(),
    };
    let value_store = FileStore {
        file: &value_file,
        offsets: value_layout.offsets.clone(),
    };
    for (low, high) in [(0, 2_500), (7, 7), (40_000, 60_000), (-9, -1)] {
        for planning in [
            Planning::Facts,
            Planning::StoredStatistics,
            Planning::ScanOnly,
        ] {
            for threads in [1, 4] {
                let memory =
                    paged::sum_where(&selector, &value, low, high, planning, threads).unwrap();
                let stored = sum_where_in(
                    (&selector, &selector_store),
                    (&value, &value_store),
                    low,
                    high,
                    planning,
                    threads,
                )
                .unwrap();
                assert_eq!((memory.0, memory.1), (stored.0, stored.1), "{low}..={high}");
                assert_eq!(memory.2.io_operations, 0);
                if planning == Planning::ScanOnly {
                    assert!(stored.2.io_operations > 0 && stored.2.io_bytes > 0);
                }
            }
        }
    }
    // Facts read fewer bytes than a scan on a query their bounds decide.
    let read = |planning| {
        sum_where_in(
            (&selector, &selector_store),
            (&value, &value_store),
            7,
            7,
            planning,
            1,
        )
        .unwrap()
        .2
        .io_bytes
    };
    assert!(read(Planning::Facts) < read(Planning::ScanOnly));
    fs::remove_file(&selector_path).unwrap();
    fs::remove_file(&value_path).unwrap();
}

#[test]
fn a_damaged_trailer_is_rejected_and_an_audit_rereads_the_bounds() {
    let (selector, _) = columns();
    let column = PagedColumn::from_values(&selector, PAGE, &policy()).unwrap();
    let path = scratch("damaged.wpf");
    let layout = paged_file::write(&column, &path).unwrap();
    // Auditing the bounds footer against the pages reads only bound fields.
    let file = File::open(&path).unwrap();
    let store = FileStore {
        file: &file,
        offsets: layout.offsets.clone(),
    };
    let footer = paged_file::read_summary(&file, &layout, Summary::DerivedBounds).unwrap();
    for (index, (page, entry)) in column.pages().iter().zip(&footer).enumerate() {
        let mut session =
            paged::PageStore::session(&store, page, index, paged::Access::Plan).unwrap();
        assert_eq!(
            paged::derived_bounds_in(page, &mut session).unwrap(),
            entry.interval
        );
    }
    drop(file);
    let mut bytes = fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    fs::write(&path, &bytes).unwrap();
    assert!(paged_file::open(&File::open(&path).unwrap()).is_err());
    fs::remove_file(&path).unwrap();
}
