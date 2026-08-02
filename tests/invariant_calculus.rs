use proptest::prelude::*;
use witness::access_compiler::{
    AccessInvariant, Answer, Authorization, CheckedInvariants, ClosureMode, DecoderIr, DecoderNode,
    DeltaCoding, Evidence, FactScope, InputColumn, MappingInvariant, NullPlacement, PlanOp,
    Predicate, Property, Query, Recipe, ValueInvariant, compile, derive_invariants, encode,
    execute_interpreted,
};

fn unsigned_delta() -> Recipe {
    Recipe::UnsignedDelta {
        restart_interval: 1_024,
        deltas: Box::new(Recipe::BitPack),
    }
}

#[test]
fn unsigned_delta_is_a_structural_monotonicity_proof() {
    let column = encode(
        &unsigned_delta(),
        InputColumn::dense((0..1_024).map(|row| 10_000 + row * 7).collect()),
    )
    .unwrap();
    let facts = derive_invariants(
        &column.decoder,
        column.page.layout(),
        CheckedInvariants::default(),
    )
    .unwrap();
    let certificate = facts
        .iter()
        .find(|fact| {
            fact.node == column.decoder.root()
                && fact.scope == FactScope::AllRows
                && fact.property == Property::Value(ValueInvariant::NonDecreasing)
        })
        .unwrap();
    assert_eq!(
        certificate.evidence,
        Evidence::Structural("unsigned_delta_segments")
    );
    assert!(matches!(
        column.decoder.node(column.decoder.root()).unwrap(),
        witness::access_compiler::DecoderNode::Delta {
            coding: DeltaCoding::Unsigned,
            ..
        }
    ));
}

#[test]
fn signed_delta_does_not_claim_structural_monotonicity() {
    let recipe = Recipe::Delta {
        restart_interval: 128,
        deltas: Box::new(Recipe::BitPack),
    };
    let column = encode(&recipe, InputColumn::dense(vec![10, 9, 11, 8, 20, 3, 30])).unwrap();
    let facts = derive_invariants(
        &column.decoder,
        column.page.layout(),
        CheckedInvariants::default(),
    )
    .unwrap();
    assert!(!facts.contains(
        column.decoder.root(),
        FactScope::AllRows,
        Property::Value(ValueInvariant::NonDecreasing),
    ));
}

#[test]
fn restart_delta_proves_only_piecewise_monotonicity() {
    let recipe = Recipe::UnsignedDelta {
        restart_interval: 128,
        deltas: Box::new(Recipe::BitPack),
    };
    let column = encode(
        &recipe,
        InputColumn::dense((0..1_024).map(i64::from).collect()),
    )
    .unwrap();
    let facts = derive_invariants(
        &column.decoder,
        column.page.layout(),
        CheckedInvariants::default(),
    )
    .unwrap();
    let root = column.decoder.root();
    assert!(facts.contains(
        root,
        FactScope::AllRows,
        Property::Value(ValueInvariant::PiecewiseNonDecreasing { max_rows: 128 }),
    ));
    assert!(!facts.contains(
        root,
        FactScope::AllRows,
        Property::Value(ValueInvariant::NonDecreasing),
    ));
}

#[test]
fn piecewise_fact_authorizes_only_per_block_search() {
    let recipe = Recipe::UnsignedDelta {
        restart_interval: 128,
        deltas: Box::new(Recipe::BitPack),
    };
    let values = (0..1_024).map(|row| i64::from(row % 128)).collect();
    let column = encode(&recipe, InputColumn::dense(values)).unwrap();
    let plan = compile(
        &column,
        Query::Filter {
            predicate: witness::access_compiler::Predicate::Between { low: 20, high: 40 },
        },
    )
    .unwrap();
    assert!(
        plan.nodes
            .iter()
            .any(|node| { matches!(node.op, PlanOp::SearchPiecewiseMonotone { max_rows: 128 }) })
    );
    assert!(
        !plan
            .nodes
            .iter()
            .any(|node| matches!(node.op, PlanOp::SearchMonotone { .. }))
    );
}

#[test]
fn sorted_dictionary_exposes_mapping_not_row_order() {
    let column = encode(
        &Recipe::Dictionary(Box::new(Recipe::BitPack)),
        InputColumn::dense(vec![30, 10, 20, 10, 30]),
    )
    .unwrap();
    let facts = derive_invariants(
        &column.decoder,
        column.page.layout(),
        CheckedInvariants::default(),
    )
    .unwrap();
    let root = column.decoder.root();
    assert!(facts.contains(
        root,
        FactScope::Mapping,
        Property::Mapping(MappingInvariant::OrderPreserving),
    ));
    assert!(!facts.contains(
        root,
        FactScope::AllRows,
        Property::Value(ValueInvariant::NonDecreasing),
    ));
    let plan = compile(
        &column,
        Query::Filter {
            predicate: witness::access_compiler::Predicate::Between { low: 15, high: 25 },
        },
    )
    .unwrap();
    assert!(
        plan.nodes
            .iter()
            .any(|node| matches!(node.op, PlanOp::TranslateDictionaryRange { entries: 3, .. }))
    );
}

#[test]
fn generic_dictionary_without_sorted_contract_exposes_no_order_mapping() {
    let mut column = encode(
        &Recipe::Dictionary(Box::new(Recipe::BitPack)),
        InputColumn::dense(vec![30, 10, 20, 10, 30]),
    )
    .unwrap();
    let root = column.decoder.root();
    let mut nodes = column.decoder.nodes().to_vec();
    let DecoderNode::Dictionary { sorted_unique, .. } = &mut nodes[root.0] else {
        panic!("expected dictionary root")
    };
    *sorted_unique = false;
    column.decoder = DecoderIr::new(nodes, root).unwrap();
    let facts = derive_invariants(
        &column.decoder,
        column.page.layout(),
        CheckedInvariants::default(),
    )
    .unwrap();
    assert!(!facts.contains(
        root,
        FactScope::Mapping,
        Property::Mapping(MappingInvariant::OrderPreserving),
    ));
    let plan = compile(
        &column,
        Query::Filter {
            predicate: witness::access_compiler::Predicate::Between { low: 15, high: 25 },
        },
    )
    .unwrap();
    assert!(
        !plan
            .nodes
            .iter()
            .any(|node| matches!(node.op, PlanOp::TranslateDictionaryRange { .. }))
    );
}

#[test]
fn null_placement_controls_monotone_search_authorization() {
    for (values, expected) in [
        (
            vec![None, None, Some(10), Some(20), Some(30)],
            Some(NullPlacement::First),
        ),
        (
            vec![Some(10), Some(20), Some(30), None, None],
            Some(NullPlacement::Last),
        ),
        (vec![Some(10), None, Some(20), None, Some(30)], None),
    ] {
        let recipe = Recipe::Nullable {
            rank_interval: 8,
            values: Box::new(unsigned_delta()),
        };
        let column = encode(&recipe, InputColumn::nullable(values)).unwrap();
        let plan = compile(
            &column,
            Query::Filter {
                predicate: witness::access_compiler::Predicate::Between { low: 15, high: 25 },
            },
        )
        .unwrap();
        let actual = plan.nodes.iter().find_map(|node| match node.op {
            PlanOp::SearchMonotone { nulls } => Some(nulls),
            _ => None,
        });
        assert_eq!(actual, expected);
    }
}

#[test]
fn unsigned_delta_roundtrips_extreme_i64_span() {
    let values = vec![i64::MIN, -1, 0, i64::MAX];
    let column = encode(&unsigned_delta(), InputColumn::dense(values.clone())).unwrap();
    for (row, expected) in values.into_iter().enumerate() {
        let execution =
            execute_interpreted(&column, &Query::Get { row }, ClosureMode::Selective).unwrap();
        assert_eq!(
            execution.answer,
            witness::access_compiler::Answer::Value(Some(expected))
        );
    }
}

proptest! {
    #[test]
    fn derived_root_value_facts_are_sound(
        values in prop::collection::vec(-10_000_i64..10_000, 1..512),
        null_stride in 0_usize..11,
    ) {
        let optional = values
            .iter()
            .enumerate()
            .map(|(row, &value)| {
                if null_stride > 1 && row % null_stride == 0 { None } else { Some(value) }
            })
            .collect::<Vec<_>>();
        if optional.iter().all(Option::is_none) {
            return Ok(());
        }
        let recipe = Recipe::Nullable {
            rank_interval: 16,
            values: Box::new(Recipe::For(Box::new(Recipe::BitPack))),
        };
        let Ok(column) = encode(&recipe, InputColumn::nullable(optional.clone())) else {
            return Ok(());
        };
        let facts = derive_invariants(
            &column.decoder,
            column.page.layout(),
            column.page.invariants(),
        ).unwrap();
        let root = column.decoder.root();
        if facts.contains(
            root,
            FactScope::AllRows,
            Property::Value(ValueInvariant::NonDecreasing),
        ) {
            prop_assert!(optional.iter().all(Option::is_some));
            prop_assert!(optional.windows(2).all(|pair| pair[0] <= pair[1]));
        }
        if facts.contains(
            root,
            FactScope::NonNullRows,
            Property::Value(ValueInvariant::NonDecreasing),
        ) {
            let dense = optional.iter().flatten().copied().collect::<Vec<_>>();
            prop_assert!(dense.windows(2).all(|pair| pair[0] <= pair[1]));
        }
    }
}

#[test]
fn run_length_structure_authorizes_counting_without_decoding_rows() {
    let values = vec![7, 7, 7, 7, 7, 9, 9, 9, 7, 7, 7, 7, 7, 7, 7, 9];
    let column = encode(
        &Recipe::Rle {
            index_interval: 4,
            values: Box::new(Recipe::BitPack),
        },
        InputColumn::dense(values.clone()),
    )
    .unwrap();
    let plan = compile(
        &column,
        Query::Count {
            predicate: Predicate::Equals { value: 7 },
        },
    )
    .unwrap();
    let run_lengths_field = match plan.nodes.last().map(|node| &node.op) {
        Some(PlanOp::CountRuns { run_lengths }) => *run_lengths,
        other => panic!("run-length structure should authorize CountRuns, got {other:?}"),
    };
    // The plan must depend on the run-length index, not on every row: a
    // 16-row column has far more rows than runs, so a sound closure over
    // just run lengths and run values is strictly smaller than the row
    // stream it replaces.
    assert!(
        plan.nodes
            .iter()
            .any(|node| matches!(&node.op, PlanOp::ReadRange { field, .. } if *field == run_lengths_field)),
        "CountRuns must declare a dependency on its run-length field"
    );

    let expected = values.iter().filter(|&&value| value == 7).count();
    let execution = execute_interpreted(
        &column,
        &Query::Count {
            predicate: Predicate::Equals { value: 7 },
        },
        ClosureMode::Selective,
    )
    .unwrap();
    assert_eq!(execution.answer, Answer::Count(expected));
}

#[test]
fn count_equal_falls_back_to_exact_scan_without_run_structure() {
    let column = encode(
        &Recipe::BitPack,
        InputColumn::dense(vec![1, 2, 3, 2, 1, 2, 3]),
    )
    .unwrap();
    let plan = compile(
        &column,
        Query::Count {
            predicate: Predicate::Equals { value: 2 },
        },
    )
    .unwrap();
    assert!(matches!(
        plan.nodes.last().map(|node| &node.op),
        Some(PlanOp::CountExact)
    ));
    let execution = execute_interpreted(
        &column,
        &Query::Count {
            predicate: Predicate::Equals { value: 2 },
        },
        ClosureMode::Selective,
    )
    .unwrap();
    assert_eq!(execution.answer, Answer::Count(3));
}

#[test]
fn count_between_has_no_authorized_plan_and_is_rejected() {
    let column = encode(&Recipe::BitPack, InputColumn::dense(vec![1, 2, 3])).unwrap();
    assert!(
        compile(
            &column,
            Query::Count {
                predicate: Predicate::Between { low: 0, high: 10 },
            },
        )
        .is_err()
    );
}

proptest! {
    #[test]
    fn count_equal_matches_brute_force_over_arbitrary_runs(
        runs in prop::collection::vec((0_i64..40, 1_usize..12), 1..40),
        target in 0_i64..40,
    ) {
        let mut values = Vec::new();
        for (value, length) in runs {
            values.extend(std::iter::repeat_n(value, length));
        }
        let column = encode(
            &Recipe::Rle {
                index_interval: 8,
                values: Box::new(Recipe::BitPack),
            },
            InputColumn::dense(values.clone()),
        ).unwrap();
        let expected = values.iter().filter(|&&value| value == target).count();
        let execution = execute_interpreted(
            &column,
            &Query::Count { predicate: Predicate::Equals { value: target } },
            ClosureMode::Selective,
        ).unwrap();
        prop_assert_eq!(execution.answer, Answer::Count(expected));
    }
}

/// The authorization contract is machine-checked, not merely a convention the
/// compiler happens to follow. These tests tamper with a compiled plan the way
/// a bad rule change would and confirm the checker rejects it.
fn facts_for(
    column: &witness::access_compiler::EncodedColumn,
) -> witness::access_compiler::InvariantSet {
    derive_invariants(
        &column.decoder,
        column.page.layout(),
        column.page.invariants(),
    )
    .unwrap()
}

#[test]
fn every_compiled_plan_passes_its_own_authorization_check() {
    for recipe in [
        unsigned_delta(),
        Recipe::BitPack,
        Recipe::Dictionary(Box::new(Recipe::BitPack)),
        Recipe::Rle {
            index_interval: 64,
            values: Box::new(Recipe::BitPack),
        },
    ] {
        let column = encode(
            &recipe,
            InputColumn::dense((0..2_048).map(|row| (row / 8) as i64).collect()),
        )
        .unwrap();
        let facts = facts_for(&column);
        for query in [
            Query::Filter {
                predicate: Predicate::Between { low: 5, high: 50 },
            },
            Query::Count {
                predicate: Predicate::Equals { value: 5 },
            },
        ] {
            let plan = compile(&column, query).unwrap();
            plan.check_authorization(&facts)
                .expect("compiler emitted a plan that fails its own authorization check");
        }
    }
}

#[test]
fn a_boundary_search_claiming_order_on_an_unordered_column_is_rejected() {
    // Descending values: no order fact of any kind is derivable.
    let unordered = encode(
        &Recipe::BitPack,
        InputColumn::dense((0..1_024).map(|row| 4_096 - row).collect()),
    )
    .unwrap();
    let ordered = encode(
        &unsigned_delta(),
        InputColumn::dense((0..1_024).map(|row| 10_000 + row * 7).collect()),
    )
    .unwrap();
    let licensed = compile(
        &ordered,
        Query::Filter {
            predicate: Predicate::Between {
                low: 10_000,
                high: 11_000,
            },
        },
    )
    .unwrap();
    assert!(licensed.nodes.iter().any(
        |node| node.op.skips_scan_work() && node.authorization != Authorization::Unconditional
    ));
    // Same plan, evaluated against a column that proves nothing: fail closed.
    let error = licensed
        .check_authorization(&facts_for(&unordered))
        .unwrap_err();
    assert!(
        error.contains("does not prove"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_fast_path_that_cites_nothing_is_rejected() {
    let column = encode(
        &unsigned_delta(),
        InputColumn::dense((0..1_024).map(|row| 10_000 + row * 7).collect()),
    )
    .unwrap();
    let facts = facts_for(&column);
    let mut plan = compile(
        &column,
        Query::Filter {
            predicate: Predicate::Between {
                low: 10_000,
                high: 11_000,
            },
        },
    )
    .unwrap();
    for node in &mut plan.nodes {
        if node.op.skips_scan_work() {
            node.authorization = Authorization::Unconditional;
        }
    }
    let error = plan.check_authorization(&facts).unwrap_err();
    assert!(
        error.contains("without citing a fact"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_run_index_does_not_license_a_boundary_search() {
    // A real fact, present on the column, but the wrong one for the step:
    // presence alone must not be enough.
    let column = encode(
        &Recipe::Rle {
            index_interval: 64,
            values: Box::new(Recipe::BitPack),
        },
        InputColumn::dense((0..2_048).map(|row| (row / 8) as i64).collect()),
    )
    .unwrap();
    let facts = facts_for(&column);
    let run_index = facts
        .iter()
        .find(|fact| {
            matches!(
                fact.property,
                Property::Access(AccessInvariant::RunIndex { .. })
            )
        })
        .expect("run-length column must prove a run index");
    let mut plan = compile(
        &column,
        Query::Filter {
            predicate: Predicate::Between { low: 5, high: 50 },
        },
    )
    .unwrap();
    plan.nodes.push(witness::access_compiler::PlanNode {
        id: witness::access_compiler::PlanNodeId(plan.nodes.len()),
        op: PlanOp::SearchMonotone {
            nulls: NullPlacement::NoNulls,
        },
        rows: plan.nodes[0].rows,
        required_fields: Default::default(),
        byte_closure: plan.nodes[0].byte_closure.clone(),
        guarantee: witness::access_compiler::OutputGuarantee::ExactBitmap,
        authorization: Authorization::Fact {
            node: run_index.node,
            scope: run_index.scope,
            property: run_index.property,
        },
    });
    let error = plan.check_authorization(&facts).unwrap_err();
    assert!(
        error.contains("does not license"),
        "unexpected error: {error}"
    );
}
