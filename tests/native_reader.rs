use std::fs;
use std::process::Command;

use proptest::prelude::*;
use witness::access_compiler::native_file;
use witness::access_compiler::paged::{self, PagePolicy, PagedColumn, Planning};
use witness::access_compiler::{
    Answer, ClosureMode, InputColumn, Query, Recipe, Span, compile, encode, execute_full_decode,
    execute_interpreted,
};

fn policy(recipe: Recipe) -> PagePolicy {
    PagePolicy::smallest(vec![recipe])
}

fn recipes() -> Vec<Recipe> {
    let bp = || Box::new(Recipe::BitPack);
    vec![
        Recipe::For(bp()),
        Recipe::Dictionary(bp()),
        Recipe::Rle {
            index_interval: 4,
            values: Box::new(Recipe::For(bp())),
        },
        Recipe::Dictionary(Box::new(Recipe::Rle {
            index_interval: 4,
            values: bp(),
        })),
        Recipe::Delta {
            restart_interval: 128,
            deltas: bp(),
        },
        Recipe::UnsignedDelta {
            restart_interval: 128,
            deltas: bp(),
        },
        Recipe::Patch {
            index_interval: 4,
            values: Box::new(Recipe::For(bp())),
        },
        Recipe::Frame(Box::new(Recipe::For(bp()))),
        Recipe::For(Box::new(Recipe::ChunkedFrame {
            chunk_bytes: 128,
            values: bp(),
        })),
    ]
}

#[test]
fn all_codec_families_reopen_without_writer_values_and_rederive_the_same_facts() {
    for recipe in recipes() {
        for nulls in [false, true] {
            let values = (0..1_217)
                .map(|row| (!nulls || row % 11 != 0).then_some((row / 7) as i64 * 10))
                .collect::<Vec<_>>();
            let writer = PagedColumn::from_values(&values, 512, &policy(recipe.clone())).unwrap();
            let expected_facts = writer
                .pages()
                .iter()
                .map(|page| page.facts.clone())
                .collect::<Vec<_>>();
            let bytes = native_file::to_bytes(&writer).unwrap();
            drop(writer);
            let read = native_file::from_bytes(&bytes).unwrap();
            assert_eq!(read.column.rows(), values.len());
            assert_eq!(read.file_bytes, read.page_bytes + read.descriptor_bytes);
            assert_eq!(native_file::to_bytes(&read.column).unwrap(), bytes);
            for (page, expected) in read.column.pages().iter().zip(expected_facts) {
                assert!(page.column.truth.is_empty());
                assert_eq!(page.facts, expected);
                let rows = Span::new(0, page.rows()).unwrap();
                let start = page.start;
                let expected: i128 = values[start..start + page.rows()]
                    .iter()
                    .flatten()
                    .map(|&x| i128::from(x))
                    .sum();
                let query = Query::Sum { rows };
                assert!(compile(&page.column, query.clone()).is_ok());
                assert_eq!(
                    execute_interpreted(&page.column, &query, ClosureMode::Selective)
                        .unwrap()
                        .answer,
                    Answer::Sum(expected)
                );
                assert_eq!(
                    execute_full_decode(&page.column, &query, ClosureMode::Selective)
                        .unwrap()
                        .answer,
                    Answer::Sum(expected)
                );
            }
            for planning in [
                Planning::Facts,
                Planning::StoredStatistics,
                Planning::StoredStatisticsPlus,
                Planning::ScanOnly,
            ] {
                let (sum, count, _) =
                    paged::sum_where(&read.column, &read.column, 110, 550, planning, 1).unwrap();
                let selected = values
                    .iter()
                    .flatten()
                    .filter(|&&x| (110..=550).contains(&x))
                    .copied()
                    .collect::<Vec<_>>();
                assert_eq!(
                    (sum, count),
                    (
                        selected.iter().map(|&x| i128::from(x)).sum(),
                        selected.len()
                    )
                );
            }
        }
    }
}

#[test]
fn empty_all_null_extreme_and_partially_appended_pages_round_trip() {
    for values in [
        vec![],
        vec![None; 1_025],
        vec![Some(i64::MIN), None, Some(i64::MAX)],
    ] {
        let mut writer = PagedColumn::from_values(
            &values,
            512,
            &policy(Recipe::For(Box::new(Recipe::BitPack))),
        )
        .unwrap();
        if !values.is_empty() {
            writer
                .append(&[Some(17)], &policy(Recipe::For(Box::new(Recipe::BitPack))))
                .unwrap();
        }
        let bytes = native_file::to_bytes(&writer).unwrap();
        let rows = writer.rows();
        drop(writer);
        let reader = native_file::from_bytes(&bytes).unwrap();
        assert_eq!(reader.column.rows(), rows);
        let (sum, matched, _) = paged::sum_where(
            &reader.column,
            &reader.column,
            i64::MIN,
            i64::MAX,
            Planning::Facts,
            1,
        )
        .unwrap();
        let extra = usize::from(!values.is_empty());
        assert_eq!(
            sum,
            values
                .iter()
                .flatten()
                .map(|&x| i128::from(x))
                .sum::<i128>()
                + 17 * extra as i128
        );
        assert_eq!(matched, values.iter().flatten().count() + extra);
    }
}

#[test]
fn a_fresh_process_queries_files_after_all_writer_objects_are_dropped() {
    let path =
        std::env::temp_dir().join(format!("witness-native-process-{}.wnf", std::process::id()));
    let writer = PagedColumn::from_values(
        &(0..2_048).map(Some).collect::<Vec<_>>(),
        512,
        &policy(Recipe::For(Box::new(Recipe::BitPack))),
    )
    .unwrap();
    native_file::write(&writer, &path).unwrap();
    drop(writer);
    let output = Command::new(env!("CARGO_BIN_EXE_native_query"))
        .arg("--regions")
        .arg(&path)
        .arg(&path)
        .args(["100", "199"])
        .output()
        .unwrap();
    fs::remove_file(&path).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("\"sum\":14950,"));
    assert!(text.contains("\"matched\":100,"));
    assert!(text.contains("\"rows\":2048,"));
}

fn rechecksum(bytes: &mut [u8]) {
    let end = bytes.len() - 8;
    let hash = bytes[..end].iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, x| {
        (h ^ u64::from(*x)).wrapping_mul(0x1000_0000_01b3)
    });
    bytes[end..].copy_from_slice(&hash.to_le_bytes());
}

#[test]
fn malformed_descriptors_and_corruption_fail_without_panicking() {
    let writer = PagedColumn::from_values(
        &(0..512).map(Some).collect::<Vec<_>>(),
        512,
        &policy(Recipe::For(Box::new(Recipe::BitPack))),
    )
    .unwrap();
    let bytes = native_file::to_bytes(&writer).unwrap();
    for length in [0, 7, 23, 31, 63, bytes.len() - 1] {
        assert!(native_file::from_bytes(&bytes[..length]).is_err());
    }
    let recipe_len = u64::from_le_bytes(bytes[32..40].try_into().unwrap()) as usize;
    let decoder = 89 + recipe_len;
    // Alter lengths, node count/root, field id, width, and miniblock shape,
    // recomputing integrity checks so parser validation is exercised.
    for offset in [
        8,
        16,
        24,
        32,
        40,
        decoder,
        decoder + 8,
        decoder + 17,
        decoder + 25,
        decoder + 41,
    ] {
        let mut corrupt = bytes.clone();
        corrupt[offset..offset + 8].fill(0xff);
        rechecksum(&mut corrupt);
        assert!(
            native_file::from_bytes(&corrupt).is_err(),
            "offset {offset}"
        );
    }
    for offset in (0..bytes.len()).step_by(17) {
        let mut corrupt = bytes.clone();
        corrupt[offset] ^= 0x80;
        assert!(native_file::from_bytes(&corrupt).is_err());
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]
    #[test]
    fn random_nullable_queries_equal_the_original_values(values in prop::collection::vec(prop::option::of(-10_000_i64..10_000), 1..1_200), a in -12_000_i64..12_000, b in -12_000_i64..12_000) {
        let writer = PagedColumn::from_values(&values, 128, &policy(Recipe::Dictionary(Box::new(Recipe::BitPack)))).unwrap();
        let bytes = native_file::to_bytes(&writer).unwrap(); drop(writer);
        let read = native_file::from_bytes(&bytes).unwrap();
        let (low, high) = (a.min(b), a.max(b));
        let expected = values.iter().flatten().filter(|&&x| low <= x && x <= high).copied().collect::<Vec<_>>();
        let (sum, count, _) = paged::sum_where(&read.column, &read.column, low, high, Planning::Facts, 1).unwrap();
        prop_assert_eq!((sum, count), (expected.iter().map(|&x| i128::from(x)).sum(), expected.len()));
    }

    #[test]
    fn arbitrary_small_files_do_not_panic(bytes in prop::collection::vec(any::<u8>(), 0..2_048)) {
        let _ = native_file::from_bytes(&bytes);
    }
}

#[test]
fn generic_execution_uses_decoder_rows_when_the_oracle_is_absent() {
    let mut column = encode(
        &Recipe::For(Box::new(Recipe::BitPack)),
        InputColumn::dense(vec![7, 11, 19]),
    )
    .unwrap();
    column.truth.clear();
    assert_eq!(column.len(), 3);
    assert_eq!(
        execute_interpreted(&column, &Query::Get { row: 2 }, ClosureMode::Selective)
            .unwrap()
            .answer,
        Answer::Value(Some(19))
    );
    assert!(execute_interpreted(&column, &Query::Get { row: 3 }, ClosureMode::Selective).is_err());
}
