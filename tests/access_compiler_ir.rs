use witness::access_compiler::{
    AccessSet, Authorization, ByteClosure, ClosureSpec, DecoderIr, DecoderNode, DependencyRule,
    FieldId, FieldLayout, FieldLocation, FrameCodec, FrameId, FrameLayout, LayoutIr, NodeId,
    OutputGuarantee, PlanIr, PlanNode, PlanNodeId, PlanOp, Predicate, Query, RowDomain, Span,
    compile, encode, heldout_cases, input_for,
};

#[test]
fn decoder_ir_requires_a_topological_validated_dag() {
    let bitpack = DecoderNode::BitUnpack {
        stream: FieldId(0),
        width: 3,
        len: 3,
        miniblock_rows: 128,
        miniblock_bytes: 48,
    };
    let valid = DecoderIr::new(
        vec![
            bitpack.clone(),
            DecoderNode::For {
                base: FieldId(1),
                values: NodeId(0),
            },
        ],
        NodeId(1),
    )
    .unwrap();
    assert_eq!(valid.len().unwrap(), 3);

    assert!(
        DecoderIr::new(
            vec![
                DecoderNode::For {
                    base: FieldId(1),
                    values: NodeId(1),
                },
                bitpack,
            ],
            NodeId(0),
        )
        .is_err()
    );
}

#[test]
fn full_domain_filter_owns_predicate_discovery() {
    let case = &heldout_cases()[4];
    let column = encode(&case.recipe, input_for(case, 4_096)).unwrap();
    let plan = compile(
        &column,
        Query::Filter {
            predicate: Predicate::Between { low: 0, high: 10 },
        },
    )
    .unwrap();
    assert_eq!(plan.output, OutputGuarantee::ExactBitmap);
    assert!(plan.nodes.iter().all(|node| node.rows.0.end == 4_096));
    assert_eq!(plan.nodes.last().unwrap().op, PlanOp::RefineCandidates);
    assert!(
        !plan
            .nodes
            .iter()
            .any(|node| matches!(node.op, PlanOp::SearchMonotone { .. }))
    );
}

#[test]
fn checked_monotonicity_authorizes_runtime_refined_boundary_search() {
    let case = &heldout_cases()[3];
    let column = encode(&case.recipe, input_for(case, 4_096)).unwrap();
    assert!(column.page.invariants().non_decreasing);
    let plan = compile(
        &column,
        Query::Filter {
            predicate: Predicate::Between { low: 0, high: 10 },
        },
    )
    .unwrap();
    let search = plan
        .nodes
        .iter()
        .find(|node| matches!(node.op, PlanOp::SearchMonotone { .. }))
        .unwrap();
    assert!(matches!(
        search.byte_closure,
        ClosureSpec::RuntimeRefined { .. }
    ));
}

#[test]
fn persisted_invariant_is_descriptor_authenticated() {
    let case = &heldout_cases()[3];
    let column = encode(&case.recipe, input_for(case, 4_096)).unwrap();
    assert!(column.page.invariants().non_decreasing);
    let reparsed =
        witness::access_compiler::SerializedPage::parse(column.page.bytes().to_vec()).unwrap();
    assert!(reparsed.invariants().non_decreasing);

    let mut modified = column.page.bytes().to_vec();
    modified[32] ^= 1;
    assert!(witness::access_compiler::SerializedPage::parse(modified).is_err());
}

#[test]
fn legacy_descriptor_without_certificate_defaults_to_scan() {
    let mut bytes = vec![0_u8; 128];
    bytes[..8].copy_from_slice(b"ACPAGE01");
    bytes[8..12].copy_from_slice(&2_u32.to_le_bytes());
    bytes[12..16].copy_from_slice(&1_u32.to_le_bytes());
    bytes[24..32].copy_from_slice(&128_u64.to_le_bytes());
    bytes[48..56].copy_from_slice(&128_u64.to_le_bytes());
    bytes[64..68].copy_from_slice(&64_u32.to_le_bytes());
    bytes[68..72].copy_from_slice(&64_u32.to_le_bytes());
    let page = witness::access_compiler::SerializedPage::parse(bytes).unwrap();
    assert!(!page.invariants().non_decreasing);
}

#[test]
fn physical_closure_reaches_a_fixed_point_and_rounds_delivery() {
    let fields = vec![
        direct(0, "metadata", 0, 64),
        direct(1, "data", 64, 256),
        direct(2, "index", 320, 64),
        direct(3, "restarts", 384, 64),
    ];
    let layout = LayoutIr {
        metadata: FieldId(0),
        fields,
        frames: Vec::new(),
        dependencies: vec![
            DependencyRule::DependentField {
                source: FieldId(1),
                prerequisite: FieldId(0),
            },
            DependencyRule::IndexedStream {
                data: FieldId(1),
                index: FieldId(2),
                data_block_bytes: 64,
                index_entry_bytes: 4,
            },
            DependencyRule::Restart {
                data: FieldId(1),
                restarts: FieldId(3),
                data_bytes_per_restart: 64,
                bytes_per_restart: 8,
            },
        ],
        file_length: 448,
    };
    let mut seed = AccessSet::default();
    seed.insert(FieldId(1), Span::new(70, 75).unwrap());
    let closure = layout.closure(&seed).unwrap();
    assert_eq!(
        closure.fields.spans(FieldId(1)),
        &[Span { start: 64, end: 75 }]
    );
    assert_eq!(
        closure.fields.spans(FieldId(2)),
        &[Span { start: 4, end: 8 }]
    );
    assert_eq!(
        closure.fields.spans(FieldId(3)),
        &[Span { start: 8, end: 16 }]
    );
    assert_eq!(closure.logical_bytes, 87);
    assert_eq!(closure.delivered_bytes, 256);
}

#[test]
fn any_access_inside_a_frame_delivers_the_complete_compressed_frame() {
    let layout = LayoutIr {
        metadata: FieldId(0),
        fields: vec![
            direct(0, "metadata", 0, 64),
            FieldLayout {
                id: FieldId(1),
                name: "framed.data".into(),
                length: 100,
                alignment: 1,
                read_granularity: 1,
                location: FieldLocation::Framed {
                    frame: FrameId(0),
                    decoded_offset: 0,
                },
            },
        ],
        frames: vec![FrameLayout {
            id: FrameId(0),
            codec: FrameCodec::Zstd,
            offset: 64,
            compressed_length: 30,
            decoded_length: 100,
            alignment: 64,
        }],
        dependencies: Vec::new(),
        file_length: 94,
    };
    let mut seed = AccessSet::default();
    seed.insert(FieldId(1), Span::new(5, 10).unwrap());
    let closure = layout.closure(&seed).unwrap();
    assert_eq!(closure.logical_bytes, 5);
    assert_eq!(closure.delivered_bytes, 30);
    assert_eq!(closure.delivered, vec![Span { start: 64, end: 94 }]);
}

#[test]
fn plan_nodes_carry_domains_fields_closures_and_guarantees() {
    let closure = ByteClosure {
        fields: AccessSet::default(),
        delivered: vec![Span { start: 0, end: 64 }],
        logical_bytes: 8,
        delivered_bytes: 64,
    };
    let plan = PlanIr {
        query: Query::Get { row: 7 },
        nodes: vec![PlanNode {
            id: PlanNodeId(0),
            op: PlanOp::LoadMetadata { field: FieldId(0) },
            rows: RowDomain(Span { start: 7, end: 8 }),
            required_fields: AccessSet::default(),
            byte_closure: ClosureSpec::Exact(closure),
            guarantee: OutputGuarantee::ExactScalar,
            authorization: Authorization::Unconditional,
        }],
        output: OutputGuarantee::ExactScalar,
        fallback_reason: None,
    };
    plan.validate().unwrap();
}

#[test]
fn runtime_refinement_declares_every_patch_field_it_may_open() {
    let case = &heldout_cases()[1];
    let column = encode(&case.recipe, input_for(case, 4_096)).unwrap();
    let plan = compile(&column, Query::Get { row: 2_000 }).unwrap();
    let possible = plan
        .nodes
        .iter()
        .find_map(|node| match &node.byte_closure {
            ClosureSpec::RuntimeRefined {
                possible_fields,
                reason,
                ..
            } if reason.starts_with("patch index") => Some(possible_fields),
            _ => None,
        })
        .unwrap();
    let names = possible
        .iter()
        .map(|field| column.page.layout().fields[field.0].name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        ["patch.positions", "patch.index", "patch.exceptions"]
    );
}

fn direct(id: usize, name: &str, offset: usize, length: usize) -> FieldLayout {
    FieldLayout {
        id: FieldId(id),
        name: name.into(),
        length,
        alignment: 64,
        read_granularity: 64,
        location: FieldLocation::Direct { offset },
    }
}
