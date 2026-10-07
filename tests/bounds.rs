use witness::access_compiler::{
    Answer, BoundSource, ClosureMode, EncodedColumn, FactScope, InputColumn, InvariantSet,
    Property, Query, ReadSession, Recipe, Span, ValueInvariant, bound_interval, derive_invariants,
    encode, execute_interpreted, heldout_cases, input_for,
};

fn facts(column: &EncodedColumn) -> InvariantSet {
    derive_invariants(
        &column.decoder,
        column.page.layout(),
        column.page.invariants(),
    )
    .unwrap()
}

/// Every bound the root proves, with its scope, evaluated from the page.
fn root_bounds(column: &EncodedColumn) -> Vec<(FactScope, BoundSource, (i128, i128))> {
    let root = column.decoder.root();
    facts(column)
        .iter()
        .filter(|fact| fact.node == root)
        .filter_map(|fact| match fact.property {
            Property::Value(ValueInvariant::Bounded(source)) => Some((fact.scope, source)),
            _ => None,
        })
        .map(|(scope, source)| {
            let mut session = ReadSession::new(&column.page, ClosureMode::Selective);
            (scope, source, bound_interval(&mut session, source).unwrap())
        })
        .collect()
}

fn for_bp() -> Recipe {
    Recipe::For(Box::new(Recipe::BitPack))
}

fn values(count: usize, seed: u64) -> Vec<Option<i64>> {
    let mut state = seed;
    (0..count)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            Some(1_000 + (state >> 40) as i64 % 5_000)
        })
        .collect()
}

#[test]
fn frame_of_reference_bounds_values_by_base_and_width() {
    let input = values(10_000, 7);
    let column = encode(&for_bp(), InputColumn::nullable(input.clone())).unwrap();
    let bounds = root_bounds(&column);
    assert_eq!(bounds.len(), 1);
    let (scope, source, (low, high)) = bounds[0];
    assert_eq!(scope, FactScope::AllRows);
    assert!(matches!(source, BoundSource::OffsetFromBase { .. }));
    let min = input.iter().flatten().min().copied().unwrap();
    let max = input.iter().flatten().max().copied().unwrap();
    assert_eq!(low, i128::from(min), "the base is the exact minimum");
    assert!(i128::from(max) <= high && high < i128::from(min) + 2 * i128::from(max - min + 1));
}

#[test]
fn sorted_dictionary_ends_are_exact_bounds() {
    let input = values(10_000, 11);
    let column = encode(
        &Recipe::Dictionary(Box::new(Recipe::BitPack)),
        InputColumn::nullable(input.clone()),
    )
    .unwrap();
    let bounds = root_bounds(&column);
    assert_eq!(bounds.len(), 1);
    let (_, source, interval) = bounds[0];
    assert!(matches!(source, BoundSource::TableEnds { .. }));
    let min = i128::from(input.iter().flatten().min().copied().unwrap());
    let max = i128::from(input.iter().flatten().max().copied().unwrap());
    assert_eq!(interval, (min, max));
}

#[test]
fn run_length_and_nullable_parents_keep_child_bounds() {
    let runs = values(200, 3)
        .into_iter()
        .flat_map(|value| std::iter::repeat_n(value, 50))
        .collect::<Vec<_>>();
    let rle = encode(
        &Recipe::Rle {
            index_interval: 32,
            values: Box::new(for_bp()),
        },
        InputColumn::nullable(runs),
    )
    .unwrap();
    assert!(
        root_bounds(&rle)
            .iter()
            .any(|(scope, _, _)| *scope == FactScope::AllRows)
    );

    let mut sparse = values(10_000, 5);
    for value in sparse.iter_mut().step_by(3) {
        *value = None;
    }
    let nullable = encode(
        &Recipe::Nullable {
            rank_interval: 256,
            values: Box::new(for_bp()),
        },
        InputColumn::nullable(sparse),
    )
    .unwrap();
    let bounds = root_bounds(&nullable);
    assert!(!bounds.is_empty());
    assert!(
        bounds
            .iter()
            .all(|(scope, _, _)| *scope == FactScope::NonNullRows)
    );
}

#[test]
fn patches_and_deltas_prove_no_bound() {
    let input = values(10_000, 13);
    let patched = encode(
        &Recipe::Patch {
            index_interval: 32,
            values: Box::new(for_bp()),
        },
        InputColumn::nullable(input.clone()).with_patch_rows([17, 4_000]),
    )
    .unwrap();
    assert!(
        root_bounds(&patched).is_empty(),
        "one exception can escape any bound"
    );
    let delta = encode(
        &Recipe::Delta {
            restart_interval: 256,
            deltas: Box::new(Recipe::BitPack),
        },
        InputColumn::nullable(input),
    )
    .unwrap();
    assert!(root_bounds(&delta).is_empty());
}

#[test]
fn an_empty_nullable_proves_all_null() {
    let column = encode(
        &Recipe::Nullable {
            rank_interval: 256,
            values: Box::new(Recipe::BitPack),
        },
        InputColumn::nullable(vec![None; 4_096]),
    )
    .unwrap();
    let root = column.decoder.root();
    assert!(facts(&column).contains(
        root,
        FactScope::AllRows,
        Property::Value(ValueInvariant::AllNull)
    ));
    let rows = Span::new(0, 4_096).unwrap();
    for (query, answer) in [
        (Query::Get { row: 4_000 }, Answer::Value(None)),
        (Query::Sum { rows }, Answer::Sum(0)),
        (
            Query::Between {
                rows,
                low: i64::MIN,
                high: i64::MAX,
            },
            Answer::Ranges(Vec::new()),
        ),
    ] {
        let execution = execute_interpreted(&column, &query, ClosureMode::Selective).unwrap();
        assert_eq!(execution.answer, answer, "{query:?}");
    }
    let dense = encode(&for_bp(), InputColumn::nullable(values(64, 1))).unwrap();
    assert!(!facts(&dense).contains(
        dense.decoder.root(),
        FactScope::AllRows,
        Property::Value(ValueInvariant::AllNull)
    ));
}

#[test]
fn every_derived_bound_contains_every_value_on_held_out_compositions() {
    let mut checked = 0;
    for case in heldout_cases() {
        let column = encode(&case.recipe, input_for(&case, 16_384)).unwrap();
        for (_, _, (low, high)) in root_bounds(&column) {
            assert!(
                column
                    .truth
                    .iter()
                    .flatten()
                    .all(|&value| low <= i128::from(value) && i128::from(value) <= high),
                "{}",
                case.name()
            );
            checked += 1;
        }
    }
    assert!(checked >= 5, "held-out compositions must exercise bounds");
}

#[test]
fn reading_bound_evidence_is_accounted() {
    let column = encode(&for_bp(), InputColumn::nullable(values(4_096, 9))).unwrap();
    let (_, source, _) = root_bounds(&column)[0];
    let mut session = ReadSession::new(&column.page, ClosureMode::Selective);
    bound_interval(&mut session, source).unwrap();
    let metrics = session.metrics();
    assert_eq!(metrics.logical_bytes, 8, "a FOR bound reads only its base");
    assert!(metrics.delivered_bytes > 0);
}
