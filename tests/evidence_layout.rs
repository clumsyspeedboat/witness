use witness::access_compiler::paged::{self, PageAction, PagePolicy, PagedColumn, Planning};
use witness::access_compiler::{
    Answer, ClosureMode, FieldLocation, InputColumn, Query, ReadSession, Recipe, encode,
    execute_interpreted,
};
use witness::experiment::evidence_layout::{Compression, compress_payload, compression_menu};

const ROWS: usize = 16_384;

fn compressions() -> [Compression; 2] {
    [Compression::Whole, Compression::Chunked { bytes: 4_096 }]
}

#[test]
fn bounds_and_dictionary_exclusion_need_no_frame_decoding() {
    for dictionary in [false, true] {
        let values = (0..ROWS)
            .map(|row| {
                Some(if dictionary {
                    (row % 127) as i64 * 10
                } else {
                    row as i64 * 3
                })
            })
            .collect::<Vec<_>>();
        let base = if dictionary {
            Recipe::Dictionary(Box::new(Recipe::BitPack))
        } else {
            Recipe::For(Box::new(Recipe::BitPack))
        };
        for compression in compressions() {
            for payload_only in [false, true] {
                let recipe = if payload_only {
                    compress_payload(&base, compression).unwrap()
                } else {
                    compression.wrap(base.clone()).unwrap()
                };
                let column =
                    PagedColumn::from_values(&values, ROWS, &PagePolicy::smallest(vec![recipe]))
                        .unwrap();
                let page = &column.pages()[0];
                assert!(!page.column.page.layout().frames.is_empty());
                let mut session = ReadSession::new(&page.column.page, ClosureMode::Selective);
                // The dictionary's bounds overlap [11, 19], but no entry does.
                let (low, high) = if dictionary { (11, 19) } else { (-5, -1) };
                let action =
                    paged::plan_between(page, &mut session, low, high, Planning::Facts).unwrap();
                assert!(matches!(action, PageAction::Skip(_)));
                assert_eq!(session.metrics().frames_decoded == 0, payload_only);
                if payload_only {
                    assert!(
                        page.column
                            .page
                            .layout()
                            .fields
                            .iter()
                            .filter(|field| field.name != "bitpack.miniblocks")
                            .all(|field| matches!(field.location, FieldLocation::Direct { .. }))
                    );
                }
            }
        }
    }
}

#[test]
fn exposed_chunked_payload_decodes_only_the_chunks_a_probe_needs() {
    let base = Recipe::For(Box::new(Recipe::BitPack));
    let recipe = compress_payload(&base, Compression::Chunked { bytes: 4_096 }).unwrap();
    let values = (0..ROWS)
        .map(|row| row as i64 * 7919 % 1_000_003)
        .collect::<Vec<_>>();
    let column = encode(&recipe, InputColumn::dense(values.clone())).unwrap();
    assert!(column.page.layout().frames.len() > 1);
    let result = execute_interpreted(
        &column,
        &Query::Get { row: ROWS / 2 },
        ClosureMode::Selective,
    )
    .unwrap();
    assert_eq!(result.answer, Answer::Value(Some(values[ROWS / 2])));
    assert_eq!(result.metrics.frames_decoded, 1);
}

#[test]
fn all_planners_agree_on_nullable_runs_extremes_and_partial_pages() {
    let values = (0..2_203)
        .map(|row| match row / 512 {
            0 => (row % 7 != 0).then_some((row % 31) as i64 * 10 - 170),
            1 => None,
            2 => Some(i64::MIN + row as i64),
            3 => Some(i64::MAX - (row / 17) as i64),
            _ => Some(42),
        })
        .collect::<Vec<_>>();
    let sums = (0..values.len())
        .map(|row| (row % 11 != 0).then_some(row as i64 - 1_000))
        .collect::<Vec<_>>();
    let recipes = vec![
        Recipe::For(Box::new(Recipe::BitPack)),
        Recipe::Dictionary(Box::new(Recipe::Rle {
            index_interval: 32,
            values: Box::new(Recipe::BitPack),
        })),
        Recipe::Rle {
            index_interval: 32,
            values: Box::new(Recipe::For(Box::new(Recipe::BitPack))),
        },
    ];
    for compression in compressions() {
        // Force each composition instead of letting the menu evade a broken one.
        for base in &recipes {
            let recipe = compress_payload(base, compression).unwrap();
            let policy = PagePolicy::smallest(vec![recipe]);
            let selector = PagedColumn::from_values(&values, 512, &policy).unwrap();
            let value = PagedColumn::from_values(&sums, 512, &policy).unwrap();
            for (low, high) in [
                (i64::MIN, i64::MAX),
                (-20, 20),
                (11, 19),
                (42, 42),
                (i64::MIN, i64::MIN + 2_000),
                (i64::MAX - 200, i64::MAX),
            ] {
                let selected = values
                    .iter()
                    .zip(&sums)
                    .filter(|(x, _)| x.is_some_and(|x| low <= x && x <= high))
                    .collect::<Vec<_>>();
                let expected: i128 = selected
                    .iter()
                    .filter_map(|(_, value)| **value)
                    .map(i128::from)
                    .sum();
                for planning in [
                    Planning::Facts,
                    Planning::ScanOnly,
                    Planning::StoredStatistics,
                    Planning::FactBounds,
                    Planning::StoredStatisticsPlus,
                    Planning::StoredStatisticsDictionary,
                ] {
                    let (sum, matched, _) =
                        paged::sum_where(&selector, &value, low, high, planning, 1).unwrap();
                    assert_eq!(
                        (sum, matched),
                        (expected, selected.len()),
                        "{base:?} {compression:?} {planning:?} {low}..={high}"
                    );
                }
            }
        }
    }
}

#[test]
fn menu_retains_direct_fallback_and_rejects_invalid_compression() {
    let base = Recipe::For(Box::new(Recipe::BitPack));
    let menu = compression_menu(
        std::slice::from_ref(&base),
        Compression::Chunked { bytes: 4_096 },
        true,
    )
    .unwrap();
    assert_eq!(menu[0].name(), base.name());
    let values = (0..ROWS as i64)
        .map(|x| Some(x.wrapping_mul(7_919)))
        .collect::<Vec<_>>();
    let direct = PagePolicy::smallest(vec![base.clone()])
        .encode(&values)
        .unwrap();
    let selected = PagePolicy::smallest(menu).encode(&values).unwrap();
    assert!(selected.page.bytes().len() <= direct.page.bytes().len());
    assert!(compress_payload(&base, Compression::Chunked { bytes: 0 }).is_err());
    assert!(compress_payload(&Recipe::Frame(Box::new(base)), Compression::Whole).is_err());
}
