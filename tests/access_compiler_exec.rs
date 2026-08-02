use proptest::prelude::*;
use witness::access_compiler::{
    Answer, ClosureMode, EncodedColumn, FieldLocation, Predicate, Query, SerializedPage, Span,
    compile, encode, execute_full_decode, execute_fused_decode, execute_interpreted, heldout_cases,
    input_for, primitive_rule_fingerprint,
};

const ROWS: usize = 4_096;

#[test]
fn heldout_compositions_roundtrip_all_query_shapes() {
    let cases = heldout_cases();
    assert_eq!(cases.len(), 19);
    assert_ne!(primitive_rule_fingerprint(), 0);
    for case in cases {
        let column = encode(&case.recipe, input_for(&case, ROWS)).unwrap();
        let parsed = SerializedPage::parse(column.page.bytes().to_vec()).unwrap();
        assert_eq!(
            parsed.layout().dependencies,
            column.page.layout().dependencies,
            "{}",
            case.name()
        );
        for query in queries(&column.truth) {
            compile(&column, query.clone()).unwrap().validate().unwrap();
            let expected = expected(&column.truth, &query);
            for execution in [
                execute_interpreted(&column, &query, ClosureMode::Selective).unwrap(),
                execute_full_decode(&column, &query, ClosureMode::Selective).unwrap(),
                execute_fused_decode(&column, &query, ClosureMode::Selective).unwrap(),
            ] {
                assert_eq!(execution.answer, expected, "{} {query:?}", case.name());
                assert_eq!(
                    execution.metrics.delivered_bytes,
                    execution.metrics.transferred_bytes
                );
                assert!(execution.metrics.logical_bytes > 0);
            }
        }
    }
}

#[test]
fn closure_disabled_delivers_the_whole_serialized_page() {
    for case in heldout_cases() {
        let column = encode(&case.recipe, input_for(&case, ROWS)).unwrap();
        let query = Query::Get { row: ROWS / 2 };
        let selective = execute_interpreted(&column, &query, ClosureMode::Selective).unwrap();
        let full = execute_interpreted(&column, &query, ClosureMode::FullPage).unwrap();
        assert_eq!(selective.answer, full.answer);
        assert_eq!(full.metrics.delivered_bytes, column.page.bytes().len());
        assert!(selective.metrics.delivered_bytes <= full.metrics.delivered_bytes);
    }
}

#[test]
fn malformed_dependency_and_dictionary_data_are_rejected() {
    let case = &heldout_cases()[4];
    let column = encode(&case.recipe, input_for(case, ROWS)).unwrap();
    let mut invalid_dependency = column.page.bytes().to_vec();
    let fields = u32::from_le_bytes(invalid_dependency[12..16].try_into().unwrap()) as usize;
    let frames = u32::from_le_bytes(invalid_dependency[16..20].try_into().unwrap()) as usize;
    let dependency_at = 48 + fields * 48 + frames * 32;
    invalid_dependency[dependency_at + 4..dependency_at + 8]
        .copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(SerializedPage::parse(invalid_dependency).is_err());

    let dictionary = column
        .page
        .layout()
        .fields
        .iter()
        .find(|field| field.name == "dictionary.values")
        .unwrap();
    let FieldLocation::Direct { .. } = dictionary.location else {
        panic!("dictionary corruption test requires a direct field");
    };
    let mut invalid_ids = column.page.bytes().to_vec();
    let length_at = 48 + dictionary.id.0 * 48 + 16;
    invalid_ids[length_at..length_at + 8].copy_from_slice(&8_u64.to_le_bytes());
    rewrite_descriptor_checksum(&mut invalid_ids, column.page.layout().fields[0].length);
    let malformed = EncodedColumn {
        recipe: column.recipe,
        decoder: column.decoder,
        page: SerializedPage::parse(invalid_ids).unwrap(),
        truth: column.truth,
    };
    assert!(
        execute_interpreted(&malformed, &Query::Get { row: 2 }, ClosureMode::Selective).is_err()
    );
}

fn rewrite_descriptor_checksum(bytes: &mut [u8], header_length: usize) {
    bytes[40..48].fill(0);
    let checksum = bytes[..header_length]
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x1000_0000_01b3)
        });
    bytes[40..48].copy_from_slice(&checksum.to_le_bytes());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn malformed_serialized_pages_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..8192)) {
        let _ = SerializedPage::parse(bytes);
    }
}

fn queries(values: &[Option<i64>]) -> Vec<Query> {
    let n = values.len();
    let mut present = values.iter().flatten().copied().collect::<Vec<_>>();
    present.sort_unstable();
    vec![
        Query::Get { row: n / 2 },
        Query::Get { row: n / 2 + 7 },
        Query::Sum {
            rows: Span::new(n / 3, n / 3 + n / 100).unwrap(),
        },
        Query::Between {
            rows: Span::new(n / 5, n * 4 / 5).unwrap(),
            low: present[present.len() / 4],
            high: present[present.len() * 3 / 4],
        },
        Query::Filter {
            predicate: Predicate::Between {
                low: present[present.len() / 4],
                high: present[present.len() * 3 / 4],
            },
        },
    ]
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
