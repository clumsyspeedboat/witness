use std::fs::{self, File};
use std::io::Write;

use witness::access_compiler::{
    Answer, ClosureMode, EncodedColumn, FieldLocation, Predicate, Query, ReadSession,
    SerializedPage, Span, encode, heldout_cases, input_for, primitive_rule_fingerprint,
};

#[allow(dead_code)]
mod generated {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/experiments/generated/access_compiler/generated.rs"
    ));
}

#[test]
fn generated_operators_reject_corrupt_nullable_patch_and_frame_data() {
    let cases = heldout_cases();
    let nullable = encode(&cases[0].recipe, input_for(&cases[0], ROWS)).unwrap();
    let mut bytes = nullable.page.bytes().to_vec();
    let rank = nullable
        .page
        .layout()
        .fields
        .iter()
        .find(|f| f.name == "nullable.rank")
        .unwrap();
    let FieldLocation::Direct { offset } = rank.location else {
        panic!("rank must be direct")
    };
    bytes[offset..offset + 4].fill(0xff);
    let nullable = replace_page(nullable, bytes);
    assert!(generated::GET_FNS[0](&nullable, 1, ClosureMode::Selective).is_err());

    let patch = encode(&cases[1].recipe, input_for(&cases[1], ROWS)).unwrap();
    let mut bytes = patch.page.bytes().to_vec();
    let positions = patch
        .page
        .layout()
        .fields
        .iter()
        .find(|f| f.name == "patch.positions")
        .unwrap();
    let FieldLocation::Direct { offset } = positions.location else {
        panic!("positions must be direct")
    };
    let first: [u8; 4] = bytes[offset..offset + 4].try_into().unwrap();
    let second: [u8; 4] = bytes[offset + 4..offset + 8].try_into().unwrap();
    bytes[offset..offset + 4].copy_from_slice(&second);
    bytes[offset + 4..offset + 8].copy_from_slice(&first);
    let patch = replace_page(patch, bytes);
    assert!(
        generated::SUM_FNS[1](&patch, Span::new(0, ROWS).unwrap(), ClosureMode::Selective).is_err()
    );

    let framed = encode(&cases[3].recipe, input_for(&cases[3], ROWS)).unwrap();
    let mut bytes = framed.page.bytes().to_vec();
    let frame = &framed.page.layout().frames[0];
    bytes[frame.offset..frame.offset + frame.compressed_length].fill(0);
    let framed = replace_page(framed, bytes);
    assert!(generated::GET_FNS[3](&framed, 1, ClosureMode::Selective).is_err());
}

fn replace_page(column: EncodedColumn, bytes: Vec<u8>) -> EncodedColumn {
    EncodedColumn {
        recipe: column.recipe,
        decoder: column.decoder,
        page: SerializedPage::parse(bytes).unwrap(),
        truth: column.truth,
    }
}

const ROWS: usize = 16_384;

#[test]
fn generated_rust_is_frozen_reproducible_and_correct() {
    assert_eq!(generated::CASE_COUNT, 19);
    assert_eq!(generated::RULE_FINGERPRINT, primitive_rule_fingerprint());
    assert!(
        generated::PLAN_SIGNATURES
            .iter()
            .all(|signature| *signature != 0)
    );
    let source = fs::read_to_string("experiments/generated/access_compiler/generated.rs").unwrap();
    assert!(!source.contains("DecoderNode"));
    assert!(!source.contains("column.decoder"));
    for case in heldout_cases() {
        assert!(!source.contains(&case.name()));
        let column = encode(&case.recipe, input_for(&case, ROWS)).unwrap();
        let saved = fs::read(format!(
            "experiments/results/access_compiler/pages/case_{:02}.acp",
            case.id
        ))
        .unwrap();
        assert_eq!(saved, column.page.bytes(), "{}", case.name());

        let get_row = ROWS / 2 + 7;
        let get = generated::GET_FNS[case.id](&column, get_row, ClosureMode::Selective).unwrap();
        assert_eq!(get.answer, Answer::Value(column.truth[get_row]));
        let static_get =
            generated::STATIC_GET_FNS[case.id](&column, get_row, ClosureMode::Selective).unwrap();
        assert_eq!(static_get.answer, get.answer);
        assert_eq!(static_get.metrics, get.metrics);
        let fused_get =
            generated::FUSED_GET_FNS[case.id](&column, get_row, ClosureMode::Selective).unwrap();
        let materialized_get =
            generated::MATERIALIZED_GET_FNS[case.id](&column, get_row, ClosureMode::Selective)
                .unwrap();
        assert_eq!(fused_get.answer, get.answer);
        assert_eq!(materialized_get.answer, get.answer);

        let width = ROWS / 100;
        let rows = Span::new(ROWS / 2 - width / 2, ROWS / 2 + width - width / 2).unwrap();
        let sum = generated::SUM_FNS[case.id](&column, rows, ClosureMode::Selective).unwrap();
        assert_eq!(sum.answer, expected(&column.truth, &Query::Sum { rows }));
        let static_sum =
            generated::STATIC_SUM_FNS[case.id](&column, rows, ClosureMode::Selective).unwrap();
        assert_eq!(static_sum.answer, sum.answer);
        assert_access_subset(
            &format!("case {} SUM", case.id),
            static_sum.metrics,
            sum.metrics,
        );
        let fused_sum =
            generated::FUSED_SUM_FNS[case.id](&column, rows, ClosureMode::Selective).unwrap();
        let materialized_sum =
            generated::MATERIALIZED_SUM_FNS[case.id](&column, rows, ClosureMode::Selective)
                .unwrap();
        assert_eq!(fused_sum.answer, sum.answer);
        assert_eq!(materialized_sum.answer, sum.answer);

        let mut present = column.truth.iter().flatten().copied().collect::<Vec<_>>();
        present.sort_unstable();
        let low = present[present.len() / 4];
        let high = present[present.len() * 3 / 4];
        let between =
            generated::BETWEEN_FNS[case.id](&column, rows, low, high, ClosureMode::Selective)
                .unwrap();
        let static_between = generated::STATIC_BETWEEN_FNS[case.id](
            &column,
            rows,
            low,
            high,
            ClosureMode::Selective,
        )
        .unwrap();
        assert_eq!(static_between.answer, between.answer);
        assert_access_subset(
            &format!("case {} BETWEEN", case.id),
            static_between.metrics,
            between.metrics,
        );
        let fused_between =
            generated::FUSED_BETWEEN_FNS[case.id](&column, rows, low, high, ClosureMode::Selective)
                .unwrap();
        let materialized_between = generated::MATERIALIZED_BETWEEN_FNS[case.id](
            &column,
            rows,
            low,
            high,
            ClosureMode::Selective,
        )
        .unwrap();
        assert_eq!(fused_between.answer, between.answer);
        assert_eq!(materialized_between.answer, between.answer);
        assert_eq!(
            between.answer,
            expected(&column.truth, &Query::Between { rows, low, high },),
            "{}",
            case.name()
        );
        let filter =
            generated::FILTER_FNS[case.id](&column, low, high, ClosureMode::Selective).unwrap();
        let static_filter =
            generated::STATIC_FILTER_FNS[case.id](&column, low, high, ClosureMode::Selective)
                .unwrap();
        assert_eq!(static_filter.answer, filter.answer);
        assert_access_subset(
            &format!("case {} FILTER", case.id),
            static_filter.metrics,
            filter.metrics,
        );
        let mut untracked = ReadSession::new_untracked(&column.page, ClosureMode::Selective);
        let untracked_filter =
            generated::FILTER_SESSION_FNS[case.id](&column, low, high, &mut untracked).unwrap();
        assert_eq!(untracked_filter.answer, filter.answer);
        assert_eq!(untracked_filter.metrics.logical_bytes, 0);
        assert_eq!(untracked_filter.metrics.transferred_bytes, 0);
        assert_eq!(untracked.primitive_values_read(), 0);
        let scan_filter =
            generated::FILTER_SCAN_FNS[case.id](&column, low, high, ClosureMode::Selective)
                .unwrap();
        assert_eq!(scan_filter.answer, filter.answer);
        if column.page.invariants().non_decreasing {
            assert!(filter.decoded_rows < scan_filter.decoded_rows);
        }
        assert_eq!(
            filter.answer,
            expected(
                &column.truth,
                &Query::Filter {
                    predicate: Predicate::Between { low, high },
                },
            ),
            "{}",
            case.name()
        );
        let ranges = match &filter.answer {
            Answer::Ranges(ranges) => ranges,
            _ => unreachable!(),
        };
        let selected_sum =
            generated::SUM_RANGES_FNS[case.id](&column, ranges, ClosureMode::Selective).unwrap();
        let fused_selected_sum =
            generated::FUSED_SUM_RANGES_FNS[case.id](&column, ranges, ClosureMode::Selective)
                .unwrap();
        let expected_sum = ranges
            .iter()
            .flat_map(|range| &column.truth[range.start..range.end])
            .flatten()
            .map(|&value| i128::from(value))
            .sum();
        assert_eq!(selected_sum.answer, Answer::Sum(expected_sum));
        assert_eq!(fused_selected_sum.answer, selected_sum.answer);
        assert_eq!(fused_selected_sum.decoded_rows, column.truth.len());
    }
}

fn assert_access_subset(
    label: &str,
    baseline: witness::access_compiler::AccessMetrics,
    generated: witness::access_compiler::AccessMetrics,
) {
    assert!(
        generated.logical_bytes <= baseline.logical_bytes,
        "{label}: logical {} > {}",
        generated.logical_bytes,
        baseline.logical_bytes
    );
    assert!(
        generated.delivered_bytes <= baseline.delivered_bytes,
        "{label}: delivered {} > {}",
        generated.delivered_bytes,
        baseline.delivered_bytes
    );
    assert!(
        generated.transferred_bytes <= baseline.transferred_bytes,
        "{label}: transferred {} > {}",
        generated.transferred_bytes,
        baseline.transferred_bytes
    );
    assert!(
        generated.frames_decoded <= baseline.frames_decoded,
        "{label}: frames {} > {}",
        generated.frames_decoded,
        baseline.frames_decoded
    );
}

#[test]
fn generated_kernels_read_serialized_pages_at_bundle_offsets() {
    let case = &heldout_cases()[14];
    let column = encode(&case.recipe, input_for(case, ROWS)).unwrap();
    let offset = 4096_u64;
    let path = std::env::temp_dir().join(format!(
        "witness-access-session-{}-{}.acp",
        std::process::id(),
        primitive_rule_fingerprint()
    ));
    let mut writer = File::create(&path).unwrap();
    writer.write_all(&vec![0xa5; offset as usize]).unwrap();
    writer.write_all(column.page.bytes()).unwrap();
    writer.sync_all().unwrap();
    drop(writer);

    let rows = Span::new(ROWS / 3, ROWS * 2 / 3).unwrap();
    let expected = generated::SUM_FNS[case.id](&column, rows, ClosureMode::Selective).unwrap();
    let file = File::open(&path).unwrap();
    let mut file_session =
        ReadSession::from_file(&column.page, ClosureMode::Selective, &file, offset);
    let from_file = generated::SUM_SESSION_FNS[case.id](&column, rows, &mut file_session).unwrap();
    let bytes = fs::read(&path).unwrap();
    let mut byte_session = ReadSession::from_bytes(
        &column.page,
        ClosureMode::Selective,
        &bytes,
        offset as usize,
    )
    .unwrap();
    let from_bytes = generated::SUM_SESSION_FNS[case.id](&column, rows, &mut byte_session).unwrap();

    assert_eq!(from_file.answer, expected.answer);
    assert_eq!(from_bytes.answer, expected.answer);
    assert_eq!(from_file.metrics, expected.metrics);
    assert_eq!(from_bytes.metrics, expected.metrics);
    assert!(from_file.metrics.transfer_operations > 0);
    fs::remove_file(path).unwrap();
}

fn expected(values: &[Option<i64>], query: &Query) -> Answer {
    match *query {
        Query::Get { row } => Answer::Value(values[row]),
        Query::Sum { rows } => Answer::Sum(
            values[rows.start..rows.end]
                .iter()
                .flatten()
                .map(|&value| i128::from(value))
                .sum(),
        ),
        Query::Between { rows, low, high } => {
            let mut ranges: Vec<Span> = Vec::new();
            for (offset, value) in values[rows.start..rows.end].iter().enumerate() {
                if value.is_some_and(|value| low <= value && value <= high) {
                    let row = rows.start + offset;
                    if let Some(last) = ranges.last_mut()
                        && last.end == row
                    {
                        last.end = row + 1;
                    } else {
                        ranges.push(Span::new(row, row + 1).unwrap());
                    }
                }
            }
            Answer::Ranges(ranges)
        }
        Query::Filter {
            predicate: Predicate::Between { low, high },
        } => expected(
            values,
            &Query::Between {
                rows: Span::new(0, values.len()).unwrap(),
                low,
                high,
            },
        ),
        Query::Filter {
            predicate: Predicate::Equals { value: target },
        } => {
            let mut ranges: Vec<Span> = Vec::new();
            for (row, value) in values.iter().enumerate() {
                if *value == Some(target) {
                    ranges.push(Span::new(row, row + 1).unwrap());
                }
            }
            Answer::Ranges(ranges)
        }
        Query::Count {
            predicate: Predicate::Equals { value: target },
        } => Answer::Count(
            values
                .iter()
                .filter(|value| **value == Some(target))
                .count(),
        ),
        Query::Count {
            predicate: Predicate::Between { low, high },
        } => Answer::Count(
            values
                .iter()
                .filter(|value| value.is_some_and(|value| low <= value && value <= high))
                .count(),
        ),
    }
}
