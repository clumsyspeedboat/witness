use witness::access_compiler::{
    Answer, ClosureMode, DecoderNode, EncodedColumn, FieldLocation, InputColumn, NullPlacement,
    Query, ReadSession, Recipe, SerializedPage, Span, encode, execute_interpreted, heldout_cases,
    input_for, vector,
};

const ROWS: usize = 16_384;

fn next(state: &mut u64) -> usize {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    (*state >> 33) as usize
}

fn session(column: &EncodedColumn) -> ReadSession<'_> {
    ReadSession::new(&column.page, ClosureMode::Selective)
}

fn interpreted(column: &EncodedColumn, query: Query) -> Answer {
    execute_interpreted(column, &query, ClosureMode::Selective)
        .unwrap()
        .answer
}

fn truth_sum(column: &EncodedColumn, ranges: &[Span]) -> i128 {
    ranges
        .iter()
        .flat_map(|range| &column.truth[range.start..range.end])
        .flatten()
        .map(|&value| i128::from(value))
        .sum()
}

/// Random GET, SUM, BETWEEN, and selected SUM against the interpreter.
fn check_against_interpreter(column: &EncodedColumn, label: &str, seed: u64) {
    let rows_total = column.truth.len();
    let mut present = column.truth.iter().flatten().copied().collect::<Vec<_>>();
    present.sort_unstable();
    let mut state = seed;
    for _ in 0..12 {
        let row = next(&mut state) % rows_total;
        let value = vector::get(&column.decoder, &mut session(column), row).unwrap();
        assert_eq!(
            Answer::Value(value.map(|value| i64::try_from(value).unwrap())),
            interpreted(column, Query::Get { row }),
            "{label} GET {row}"
        );
        let (a, b) = (next(&mut state) % rows_total, next(&mut state) % rows_total);
        let rows = Span::new(a.min(b), a.max(b) + 1).unwrap();
        let sum = vector::sum(&column.decoder, &mut session(column), rows).unwrap();
        assert_eq!(
            Answer::Sum(sum),
            interpreted(column, Query::Sum { rows }),
            "{label} SUM {rows:?}"
        );
        if present.is_empty() {
            continue;
        }
        let (x, y) = (
            present[next(&mut state) % present.len()],
            present[next(&mut state) % present.len()],
        );
        let (low, high) = (x.min(y), x.max(y));
        let ranges =
            vector::between(&column.decoder, &mut session(column), rows, low, high).unwrap();
        assert_eq!(
            Answer::Ranges(ranges.clone()),
            interpreted(column, Query::Between { rows, low, high }),
            "{label} BETWEEN {low}..={high} over {rows:?}"
        );
        let selected = vector::sum_ranges(&column.decoder, &mut session(column), &ranges).unwrap();
        assert_eq!(selected, truth_sum(column, &ranges), "{label} selected SUM");
    }
}

#[test]
fn vector_execution_matches_the_interpreter_on_every_held_out_composition() {
    for case in heldout_cases() {
        let column = encode(&case.recipe, input_for(&case, ROWS)).unwrap();
        check_against_interpreter(&column, &case.name(), case.id as u64 + 1);
    }
}

#[test]
fn vector_execution_reads_chunked_frames() {
    // A field belongs to at most one frame, so framed held-out recipes stay out.
    for case in heldout_cases()
        .into_iter()
        .filter(|case| !case.recipe.name().contains("Frame"))
    {
        let recipe = Recipe::ChunkedFrame {
            chunk_bytes: 1_024,
            values: Box::new(case.recipe.clone()),
        };
        let column = encode(&recipe, input_for(&case, ROWS)).unwrap();
        check_against_interpreter(&column, &recipe.name(), case.id as u64 + 101);
    }
}

#[test]
fn selections_agree_with_spans() {
    for case in heldout_cases() {
        let column = encode(&case.recipe, input_for(&case, ROWS)).unwrap();
        let mut present = column.truth.iter().flatten().copied().collect::<Vec<_>>();
        present.sort_unstable();
        let mut state = case.id as u64 + 7;
        for _ in 0..6 {
            let (a, b) = (next(&mut state) % ROWS, next(&mut state) % ROWS);
            let rows = Span::new(a.min(b), a.max(b) + 1).unwrap();
            let (x, y) = (
                present[next(&mut state) % present.len()],
                present[next(&mut state) % present.len()],
            );
            let (low, high) = (x.min(y), x.max(y));
            let ranges =
                vector::between(&column.decoder, &mut session(&column), rows, low, high).unwrap();
            let selection =
                vector::between_selection(&column.decoder, &mut session(&column), rows, low, high)
                    .unwrap();
            assert_eq!(selection.ranges(), ranges, "{}", case.name());
            assert_eq!(selection.run_count(), ranges.len(), "{}", case.name());
            assert_eq!(
                selection.count(),
                ranges.iter().map(|range| range.len()).sum::<usize>()
            );
            let summed =
                vector::sum_selection(&column.decoder, &mut session(&column), &selection).unwrap();
            assert_eq!(summed, truth_sum(&column, &ranges), "{}", case.name());
        }
    }
}

#[test]
fn licensed_searches_agree_with_a_scan() {
    let sorted = (0..ROWS as i64)
        .map(|row| Some(1_000 + row / 3))
        .collect::<Vec<_>>();
    let sawtooth = (0..ROWS as i64)
        .map(|row| Some(row % 256 * 7 + row / 4_096))
        .collect::<Vec<_>>();
    let mut nulls_first = sorted.clone();
    nulls_first[..100].fill(None);
    let monotone = encode(
        &Recipe::UnsignedDelta {
            restart_interval: ROWS,
            deltas: Box::new(Recipe::BitPack),
        },
        InputColumn::nullable(sorted.clone()),
    )
    .unwrap();
    let piecewise = encode(
        &Recipe::UnsignedDelta {
            restart_interval: 256,
            deltas: Box::new(Recipe::BitPack),
        },
        InputColumn::nullable(sawtooth),
    )
    .unwrap();
    let nullable = encode(
        &Recipe::Nullable {
            rank_interval: 256,
            values: Box::new(Recipe::For(Box::new(Recipe::BitPack))),
        },
        InputColumn::nullable(nulls_first),
    )
    .unwrap();
    let dictionary = encode(
        &Recipe::Dictionary(Box::new(Recipe::Rle {
            index_interval: 32,
            values: Box::new(Recipe::BitPack),
        })),
        InputColumn::nullable(sorted),
    )
    .unwrap();
    let all = Span::new(0, ROWS).unwrap();
    for (low, high) in [
        (1_100, 2_000),
        (0, 999),
        (5_000, 9_000),
        (1_000, 1_000),
        (3_000, 50_000),
    ] {
        let scan = |column: &EncodedColumn| {
            vector::between(&column.decoder, &mut session(column), all, low, high).unwrap()
        };
        let searched = vector::search_monotone(
            &monotone.decoder,
            &mut session(&monotone),
            all,
            low,
            high,
            NullPlacement::NoNulls,
        )
        .unwrap();
        assert_eq!(searched, scan(&monotone), "monotone {low}..={high}");
        let piecewise_found = vector::search_piecewise(
            &piecewise.decoder,
            &mut session(&piecewise),
            all,
            low % 1_800,
            high % 1_800 + 1_800,
            256,
        )
        .unwrap();
        assert_eq!(
            piecewise_found,
            vector::between(
                &piecewise.decoder,
                &mut session(&piecewise),
                all,
                low % 1_800,
                high % 1_800 + 1_800
            )
            .unwrap(),
            "piecewise {low}..={high}"
        );
        let first = vector::search_monotone(
            &nullable.decoder,
            &mut session(&nullable),
            all,
            low,
            high,
            NullPlacement::First,
        )
        .unwrap();
        assert_eq!(first, scan(&nullable), "nulls-first {low}..={high}");
        let translated = vector::dictionary_range(
            &dictionary.decoder,
            &mut session(&dictionary),
            all,
            low,
            high,
        )
        .unwrap();
        assert_eq!(translated, scan(&dictionary), "dictionary {low}..={high}");
    }
}

#[test]
fn non_null_rows_match_the_truth() {
    for case in heldout_cases().into_iter().filter(|case| case.nullable) {
        let column = encode(&case.recipe, input_for(&case, ROWS)).unwrap();
        let all = Span::new(0, ROWS).unwrap();
        let ranges = vector::non_null(&column.decoder, &mut session(&column), all).unwrap();
        let mut expected = Vec::new();
        for (row, value) in column.truth.iter().enumerate() {
            if value.is_some() {
                witness::access_compiler::push_range(&mut expected, row, row + 1);
            }
        }
        assert_eq!(ranges, expected, "{}", case.name());
    }
}

#[test]
fn unordered_patch_positions_fail_closed() {
    let case = heldout_cases()
        .into_iter()
        .find(|case| matches!(case.recipe, Recipe::Patch { .. }))
        .unwrap();
    let column = encode(&case.recipe, input_for(&case, ROWS)).unwrap();
    let positions = column
        .page
        .layout()
        .fields
        .iter()
        .find(|field| field.name == "patch.positions")
        .unwrap();
    let FieldLocation::Direct { offset } = positions.location else {
        panic!("positions must be direct")
    };
    let mut bytes = column.page.bytes().to_vec();
    let (first, second) = (offset, offset + 4);
    let swapped = [&bytes[second..second + 4], &bytes[first..first + 4]].concat();
    bytes[first..first + 8].copy_from_slice(&swapped);
    let corrupt = EncodedColumn {
        recipe: column.recipe.clone(),
        decoder: column.decoder.clone(),
        page: SerializedPage::parse(bytes).unwrap(),
        truth: column.truth.clone(),
    };
    let all = Span::new(0, ROWS).unwrap();
    assert!(vector::sum(&corrupt.decoder, &mut session(&corrupt), all).is_err());
    assert!(matches!(
        corrupt.decoder.node(corrupt.decoder.root()).unwrap(),
        DecoderNode::Patch { .. }
    ));
}

/// Every code width, packed bare and under a frame-of-reference base, from
/// unaligned first rows: sums and selections match the truth, so batch
/// unpacking agrees with the stored bits wherever a span starts.
#[test]
fn packed_kernels_agree_with_the_truth_at_every_width() {
    for width in 1..=63_u32 {
        for base in [0_i64, -1_000_003] {
            let mut state = u64::from(width) * 31 + base.unsigned_abs();
            let largest = (1_i64 << width) - 1;
            let mut values = (0..3_000)
                .map(|_| base + (next(&mut state) as i64 & largest))
                .collect::<Vec<_>>();
            values[17] = base + largest;
            values[18] = base;
            let recipe = if base == 0 {
                Recipe::BitPack
            } else {
                Recipe::For(Box::new(Recipe::BitPack))
            };
            let column = encode(&recipe, InputColumn::dense(values.clone())).unwrap();
            for _ in 0..8 {
                let (a, b) = (next(&mut state) % 3_000, next(&mut state) % 3_000);
                let rows = Span::new(a.min(b), a.max(b) + 1).unwrap();
                let sum = vector::sum(&column.decoder, &mut session(&column), rows).unwrap();
                assert_eq!(
                    sum,
                    truth_sum(&column, &[rows]),
                    "width {width} base {base}"
                );
                let (x, y) = (
                    values[next(&mut state) % 3_000],
                    values[next(&mut state) % 3_000],
                );
                let (low, high) = (x.min(y), x.max(y));
                let selection = vector::between_selection(
                    &column.decoder,
                    &mut session(&column),
                    rows,
                    low,
                    high,
                )
                .unwrap();
                for (row, &value) in values.iter().enumerate().take(rows.end).skip(rows.start) {
                    assert_eq!(
                        selection.contains(row),
                        low <= value && value <= high,
                        "width {width} base {base} row {row}"
                    );
                }
                let selected =
                    vector::sum_selection(&column.decoder, &mut session(&column), &selection)
                        .unwrap();
                assert_eq!(
                    selected,
                    truth_sum(&column, &selection.ranges()),
                    "width {width} base {base}"
                );
            }
        }
    }
}

/// The fused SUM over frame-of-reference and sorted-dictionary selectors
/// equals the truth, and declines pages that are not plain codes.
#[test]
fn fused_sums_agree_with_the_truth() {
    let mut state = 99_u64;
    let rows = 5_000;
    let selector = (0..rows)
        .map(|_| (next(&mut state) % 300) as i64 - 120)
        .collect::<Vec<_>>();
    let value = (0..rows)
        .map(|_| (next(&mut state) % 70_000) as i64 - 9_000)
        .collect::<Vec<_>>();
    let bitpack = || Box::new(Recipe::BitPack);
    let value_column = encode(&Recipe::For(bitpack()), InputColumn::dense(value.clone())).unwrap();
    for (recipe, compare) in [
        (Recipe::For(bitpack()), vector::CodeCompare::Offset),
        (
            Recipe::Dictionary(bitpack()),
            vector::CodeCompare::SortedDictionary,
        ),
    ] {
        let column = encode(&recipe, InputColumn::dense(selector.clone())).unwrap();
        for (low, high) in [
            (-120, 179),
            (-5, 5),
            (0, 0),
            (200, 400),
            (-500, -121),
            (37, 120),
        ] {
            for span in [Span::new(0, rows).unwrap(), Span::new(333, 4_097).unwrap()] {
                let (mut sum, mut matched) = (0_i128, 0_usize);
                for row in span.start..span.end {
                    if low <= selector[row] && selector[row] <= high {
                        sum += i128::from(value[row]);
                        matched += 1;
                    }
                }
                let fused = vector::fused_sum_between(
                    &column.decoder,
                    &mut session(&column),
                    &value_column.decoder,
                    &mut session(&value_column),
                    span,
                    (low, high),
                    compare,
                )
                .unwrap();
                assert_eq!(fused, Some((sum, matched)), "{low}..={high} {compare:?}");
            }
        }
    }
    let delta = encode(
        &Recipe::Delta {
            restart_interval: 256,
            deltas: bitpack(),
        },
        InputColumn::dense(selector.clone()),
    )
    .unwrap();
    let declined = vector::fused_sum_between(
        &delta.decoder,
        &mut session(&delta),
        &value_column.decoder,
        &mut session(&value_column),
        Span::new(0, rows).unwrap(),
        (0, 10),
        vector::CodeCompare::Offset,
    )
    .unwrap();
    assert_eq!(declined, None);
}

/// Pages with scattered and clustered nulls, under frame-of-reference and
/// sorted-dictionary codes: selections, sums, and fused sums over them equal
/// the truth from any first row.
#[test]
fn nullable_code_kernels_agree_with_the_truth() {
    let mut state = 4_242_u64;
    let rows = 6_000;
    let mut column = |gap: usize, range: usize, offset: i64| {
        (0..rows)
            .map(|row| {
                let value = (next(&mut state) % range) as i64 + offset;
                let null = if gap == 0 {
                    (1_500..2_700).contains(&row)
                } else {
                    next(&mut state).is_multiple_of(gap)
                };
                (!null).then_some(value)
            })
            .collect::<Vec<_>>()
    };
    let selectors = [
        column(3, 400, -50),
        column(0, 400, 7),
        column(29, 90, 1_000),
    ];
    let values = [column(5, 50_000, -20_000), column(0, 1 << 30, 0)];
    let bitpack = || Box::new(Recipe::BitPack);
    let nullable = |values: Recipe| Recipe::Nullable {
        rank_interval: 512,
        values: Box::new(values),
    };
    let encoded = |recipe: &Recipe, values: &[Option<i64>]| {
        encode(
            recipe,
            InputColumn {
                values: values.to_vec(),
                patch_rows: Default::default(),
            },
        )
        .unwrap()
    };
    let truth = |selector: &[Option<i64>], value: &[Option<i64>], span: Span, low, high| {
        let (mut sum, mut matched) = (0_i128, 0_usize);
        for row in span.start..span.end {
            if selector[row].is_some_and(|selected| low <= selected && selected <= high) {
                matched += 1;
                sum += value[row].map_or(0, i128::from);
            }
        }
        (sum, matched)
    };
    let spans = [
        Span::new(0, rows).unwrap(),
        Span::new(1_499, 2_701).unwrap(),
        Span::new(777, 5_123).unwrap(),
        Span::new(2_000, 2_001).unwrap(),
    ];
    let value_recipes = [
        nullable(Recipe::For(bitpack())),
        nullable(Recipe::Dictionary(bitpack())),
    ];
    for (value, value_recipe) in values
        .iter()
        .flat_map(|value| value_recipes.iter().map(move |recipe| (value, recipe)))
    {
        let value_column = encoded(value_recipe, value);
        for span in spans {
            let sum =
                vector::sum(&value_column.decoder, &mut session(&value_column), span).unwrap();
            let expected = value[span.start..span.end]
                .iter()
                .flatten()
                .map(|&value| i128::from(value))
                .sum::<i128>();
            assert_eq!(sum, expected, "nullable range sum {span:?}");
        }
        for selector in &selectors {
            for (recipe, compare) in [
                (
                    nullable(Recipe::For(bitpack())),
                    vector::CodeCompare::Offset,
                ),
                (
                    nullable(Recipe::Dictionary(bitpack())),
                    vector::CodeCompare::SortedDictionary,
                ),
            ] {
                let selector_column = encoded(&recipe, selector);
                for (low, high) in [
                    (-50, 349),
                    (0, 40),
                    (1_010, 1_011),
                    (5_000, 6_000),
                    (100, 100),
                ] {
                    for span in spans {
                        let expected = truth(selector, value, span, low, high);
                        let fused = vector::fused_sum_between(
                            &selector_column.decoder,
                            &mut session(&selector_column),
                            &value_column.decoder,
                            &mut session(&value_column),
                            span,
                            (low, high),
                            compare,
                        )
                        .unwrap();
                        assert_eq!(
                            fused,
                            Some(expected),
                            "{} {low}..={high} {span:?}",
                            recipe.name()
                        );
                        // A translation licence needs a dictionary root, so
                        // nullable dictionary pages reach only the fused pass.
                        if compare == vector::CodeCompare::SortedDictionary {
                            continue;
                        }
                        let selection = vector::between_selection(
                            &selector_column.decoder,
                            &mut session(&selector_column),
                            span,
                            low,
                            high,
                        )
                        .unwrap();
                        assert_eq!(selection.count(), expected.1, "{} {span:?}", recipe.name());
                        let summed = vector::sum_selection(
                            &value_column.decoder,
                            &mut session(&value_column),
                            &selection,
                        )
                        .unwrap();
                        assert_eq!(summed, expected.0, "{} {span:?}", recipe.name());
                    }
                }
            }
        }
    }
}
