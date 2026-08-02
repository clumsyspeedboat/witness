use super::{
    AccessInvariant, AccessSet, Authorization, ClosureSpec, DecoderNode, EncodedColumn, FactScope,
    FieldId, MappingInvariant, NullPlacement, OutputGuarantee, PlanIr, PlanNode, PlanNodeId,
    PlanOp, Predicate, Property, Query, RowDomain, Span, ValueInvariant, derive_invariants,
};

pub fn compile(column: &EncodedColumn, query: Query) -> Result<PlanIr, String> {
    let rows = query_rows(column.truth.len(), &query)?;
    let mut nodes = Vec::new();
    let metadata = column.page.layout().metadata;
    let metadata_length = column.page.layout().fields[metadata.0].length;
    let mut metadata_fields = AccessSet::default();
    metadata_fields.insert(metadata, Span::new(0, metadata_length)?);
    push_plan_node(
        &mut nodes,
        PlanOp::LoadMetadata { field: metadata },
        rows,
        metadata_fields.clone(),
        ClosureSpec::Exact(column.page.layout().closure(&metadata_fields)?),
        OutputGuarantee::PhysicalBytes,
        Authorization::Unconditional,
    );
    let invariants = derive_invariants(
        &column.decoder,
        column.page.layout(),
        column.page.invariants(),
    )?;
    let root = column.decoder.root();
    let monotone_nulls = invariants.root_monotone_nulls(root);
    let piecewise_rows = invariants.root_piecewise_monotone_rows(root);
    // Which scope carries the order fact decides what the search step cites:
    // a dense column proves it over all rows, a nullable one only over the
    // non-null rows, and the checker rejects a mismatch either way.
    let monotone_scope = if invariants.contains(
        root,
        FactScope::AllRows,
        Property::Value(ValueInvariant::NonDecreasing),
    ) {
        FactScope::AllRows
    } else {
        FactScope::NonNullRows
    };
    let mut count_runs: Option<(FieldId, Authorization)> = None;
    if matches!(&query, Query::Filter { .. })
        && matches!(
            monotone_nulls,
            Some(NullPlacement::NoNulls | NullPlacement::First | NullPlacement::Last)
        )
    {
        let possible_fields = column
            .page
            .layout()
            .fields
            .iter()
            .filter(|field| field.id != metadata)
            .map(|field| field.id)
            .collect();
        push_plan_node(
            &mut nodes,
            PlanOp::SearchMonotone {
                nulls: monotone_nulls.unwrap(),
            },
            rows,
            AccessSet::default(),
            ClosureSpec::RuntimeRefined {
                seed: AccessSet::default(),
                possible_fields,
                reason: "checked non-decreasing invariant enables exact boundary search".into(),
            },
            OutputGuarantee::ExactBitmap,
            Authorization::Fact {
                node: root,
                scope: monotone_scope,
                property: Property::Value(ValueInvariant::NonDecreasing),
            },
        );
    } else if matches!(&query, Query::Filter { .. })
        && let Some(max_rows) = piecewise_rows
    {
        let possible_fields = column
            .page
            .layout()
            .fields
            .iter()
            .filter(|field| field.id != metadata)
            .map(|field| field.id)
            .collect();
        push_plan_node(
            &mut nodes,
            PlanOp::SearchPiecewiseMonotone { max_rows },
            rows,
            AccessSet::default(),
            ClosureSpec::RuntimeRefined {
                seed: AccessSet::default(),
                possible_fields,
                reason: "structural piecewise non-decreasing invariant enables exact per-block boundary search".into(),
            },
            OutputGuarantee::ExactBitmap,
            Authorization::Fact {
                node: root,
                scope: FactScope::AllRows,
                property: Property::Value(ValueInvariant::PiecewiseNonDecreasing { max_rows }),
            },
        );
    } else if matches!(&query, Query::Filter { .. })
        && invariants.contains(
            root,
            FactScope::Mapping,
            Property::Mapping(MappingInvariant::OrderPreserving),
        )
        && let DecoderNode::Dictionary {
            ids,
            dictionary,
            entries,
            ..
        } = column.decoder.node(root)?
    {
        let mut fields = AccessSet::default();
        fields.insert(
            *dictionary,
            Span::new(0, column.page.layout().fields[dictionary.0].length)?,
        );
        add_access_node(
            column,
            &mut nodes,
            PlanOp::TranslateDictionaryRange {
                dictionary: *dictionary,
                entries: *entries,
            },
            rows,
            fields,
            refinement(
                true,
                &[],
                "predicate bounds select a runtime dictionary subset",
            ),
            Authorization::Fact {
                node: root,
                scope: FactScope::Mapping,
                property: Property::Mapping(MappingInvariant::OrderPreserving),
            },
        )?;
        compile_node(column, *ids, rows, &mut nodes, false)?;
    } else if matches!(
        &query,
        Query::Count {
            predicate: Predicate::Equals { .. }
        }
    ) && let DecoderNode::Rle {
        values,
        run_lengths,
        runs,
        index_interval,
        ..
    } = column.decoder.node(root)?
    {
        let run_lengths_length = column.page.layout().fields[run_lengths.0].length;
        let mut fields = AccessSet::default();
        fields.insert(*run_lengths, Span::new(0, run_lengths_length)?);
        add_access_node(
            column,
            &mut nodes,
            PlanOp::ReadRange {
                field: *run_lengths,
                bytes: Span::new(0, run_lengths_length)?,
            },
            rows,
            fields,
            refinement(
                false,
                &[],
                "run-length index authorizes an exact count from matching runs \
                 without decoding any repeated row",
            ),
            Authorization::Unconditional,
        )?;
        compile_node(column, *values, Span::new(0, *runs)?, &mut nodes, true)?;
        count_runs = Some((
            *run_lengths,
            Authorization::Fact {
                node: root,
                scope: FactScope::Physical,
                property: Property::Access(AccessInvariant::RunIndex {
                    max_runs: *index_interval,
                }),
            },
        ));
    } else {
        compile_node(column, root, rows, &mut nodes, false)?;
    }
    let (final_op, final_authorization) = match (&query, count_runs) {
        (Query::Count { .. }, Some((run_lengths, licence))) => {
            (PlanOp::CountRuns { run_lengths }, licence)
        }
        (Query::Count { .. }, None) => (PlanOp::CountExact, Authorization::Unconditional),
        (Query::Get { .. }, _) => (PlanOp::MaterializeRows, Authorization::Unconditional),
        (Query::Sum { .. }, _) => (PlanOp::AggregateEncoded, Authorization::Unconditional),
        (Query::Between { .. } | Query::Filter { .. }, _) => {
            (PlanOp::RefineCandidates, Authorization::Unconditional)
        }
    };
    let output = match query {
        Query::Get { .. } | Query::Sum { .. } | Query::Count { .. } => OutputGuarantee::ExactScalar,
        Query::Between { .. } | Query::Filter { .. } => OutputGuarantee::ExactBitmap,
    };
    push_plan_node(
        &mut nodes,
        final_op,
        rows,
        AccessSet::default(),
        ClosureSpec::Exact(column.page.layout().closure(&AccessSet::default())?),
        output,
        final_authorization,
    );
    let plan = PlanIr {
        query,
        nodes,
        output,
        fallback_reason: None,
    };
    plan.validate()?;
    // Fail closed: no plan leaves the compiler until every step that skips
    // scan work has been re-checked against the facts this column proves.
    plan.check_authorization(&invariants)?;
    Ok(plan)
}

fn compile_node(
    column: &EncodedColumn,
    node_id: super::NodeId,
    rows: Span,
    plan: &mut Vec<PlanNode>,
    inherited_dynamic: bool,
) -> Result<(), String> {
    match column.decoder.node(node_id)? {
        DecoderNode::BitUnpack {
            stream,
            width,
            miniblock_rows,
            miniblock_bytes,
            ..
        } => {
            let mut fields = AccessSet::default();
            if *width > 0 && !rows.is_empty() {
                for block in rows.start / miniblock_rows..(rows.end - 1) / miniblock_rows + 1 {
                    let block_row = block * miniblock_rows;
                    let first = rows.start.max(block_row) - block_row;
                    let end = rows.end.min(block_row + miniblock_rows) - block_row;
                    let bit_start = first * *width as usize;
                    let bit_end = end * *width as usize;
                    fields.insert(
                        *stream,
                        Span::new(
                            block * miniblock_bytes + bit_start / 8,
                            block * miniblock_bytes + bit_end.div_ceil(8),
                        )?,
                    );
                }
            }
            add_access_node(
                column,
                plan,
                PlanOp::ReadRange {
                    field: *stream,
                    bytes: fields
                        .spans(*stream)
                        .first()
                        .copied()
                        .unwrap_or(Span::new(0, 0)?),
                },
                rows,
                fields,
                refinement(
                    inherited_dynamic,
                    &[],
                    "bit range depends on runtime-refined parent rows",
                ),
                Authorization::Unconditional,
            )?;
        }
        DecoderNode::For { base, values } => {
            let mut fields = AccessSet::default();
            fields.insert(*base, Span::new(0, 8)?);
            add_access_node(
                column,
                plan,
                PlanOp::ReadRange {
                    field: *base,
                    bytes: Span::new(0, 8)?,
                },
                rows,
                fields,
                refinement(inherited_dynamic, &[], "FOR child rows are runtime-refined"),
                Authorization::Unconditional,
            )?;
            compile_node(column, *values, rows, plan, inherited_dynamic)?;
        }
        DecoderNode::Delta {
            deltas,
            restarts,
            restart_interval,
            ..
        } => {
            let first_restart = rows.start / restart_interval;
            let last_restart = if rows.is_empty() {
                first_restart
            } else {
                (rows.end - 1) / restart_interval
            };
            let mut fields = AccessSet::default();
            fields.insert(
                *restarts,
                Span::new(first_restart * 8, (last_restart + 1) * 8)?,
            );
            add_access_node(
                column,
                plan,
                PlanOp::SeekRestart {
                    field: *restarts,
                    restart: first_restart,
                },
                rows,
                fields,
                refinement(
                    inherited_dynamic,
                    &[],
                    "delta restart range is runtime-refined",
                ),
                Authorization::Unconditional,
            )?;
            let expanded = if rows.is_empty() {
                rows
            } else {
                Span::new(first_restart * restart_interval, rows.end)?
            };
            compile_node(column, *deltas, expanded, plan, inherited_dynamic)?;
        }
        DecoderNode::Rle {
            values,
            run_lengths,
            run_index,
            runs,
            ..
        } => {
            let mut fields = AccessSet::default();
            let index_length = column.page.layout().fields[run_index.0].length;
            fields.insert(*run_index, Span::new(0, index_length)?);
            add_access_node(
                column,
                plan,
                PlanOp::RefineCandidates,
                rows,
                fields,
                refinement(
                    true,
                    &[*run_lengths],
                    "RLE index determines intersecting run lengths and value rows",
                ),
                Authorization::Unconditional,
            )?;
            compile_node(column, *values, Span::new(0, *runs)?, plan, true)?;
        }
        DecoderNode::Dictionary {
            ids,
            dictionary,
            entries: _,
            ..
        } => {
            compile_node(column, *ids, rows, plan, inherited_dynamic)?;
            let fields = AccessSet::default();
            add_access_node(
                column,
                plan,
                PlanOp::RefineCandidates,
                rows,
                fields,
                refinement(
                    true,
                    &[*dictionary],
                    "decoded ids determine the referenced dictionary subset",
                ),
                Authorization::Unconditional,
            )?;
        }
        DecoderNode::Patch {
            values,
            positions,
            position_index,
            exceptions,
            ..
        } => {
            let mut fields = AccessSet::default();
            fields.insert(
                *position_index,
                Span::new(0, column.page.layout().fields[position_index.0].length)?,
            );
            add_access_node(
                column,
                plan,
                PlanOp::RefineCandidates,
                rows,
                fields,
                refinement(
                    true,
                    &[*positions, *exceptions],
                    "patch index determines intersecting positions and exceptions",
                ),
                Authorization::Unconditional,
            )?;
            compile_node(column, *values, rows, plan, inherited_dynamic)?;
        }
        DecoderNode::Nullable {
            validity,
            rank_index,
            values,
            rank_interval,
            ..
        } => {
            let mut fields = AccessSet::default();
            fields.insert(*validity, Span::new(rows.start / 8, rows.end.div_ceil(8))?);
            let first_rank = rows.start / rank_interval;
            let rank_end = rows.end.div_ceil(*rank_interval) + 1;
            fields.insert(*rank_index, Span::new(first_rank * 4, rank_end * 4)?);
            add_access_node(
                column,
                plan,
                PlanOp::RefineCandidates,
                rows,
                fields,
                refinement(true, &[], "validity ranks determine compact child rows"),
                Authorization::Unconditional,
            )?;
            let child_len = column.decoder.node(*values)?.len(column.decoder.nodes())?;
            compile_node(column, *values, Span::new(0, child_len)?, plan, true)?;
        }
    }
    Ok(())
}

fn add_access_node(
    column: &EncodedColumn,
    plan: &mut Vec<PlanNode>,
    op: PlanOp,
    rows: Span,
    fields: AccessSet,
    refinement: Refinement<'_>,
    authorization: Authorization,
) -> Result<(), String> {
    let closure = if refinement.enabled {
        let mut possible_fields = fields
            .iter()
            .map(|(field, _)| field)
            .chain(refinement.possible_fields.iter().copied())
            .collect::<Vec<_>>();
        possible_fields.sort_unstable();
        possible_fields.dedup();
        ClosureSpec::RuntimeRefined {
            seed: fields.clone(),
            possible_fields,
            reason: refinement.reason.into(),
        }
    } else {
        ClosureSpec::Exact(column.page.layout().closure(&fields)?)
    };
    push_plan_node(
        plan,
        op,
        rows,
        fields,
        closure,
        OutputGuarantee::PhysicalBytes,
        authorization,
    );
    Ok(())
}

struct Refinement<'a> {
    enabled: bool,
    possible_fields: &'a [super::FieldId],
    reason: &'a str,
}

fn refinement<'a>(
    enabled: bool,
    possible_fields: &'a [super::FieldId],
    reason: &'a str,
) -> Refinement<'a> {
    Refinement {
        enabled,
        possible_fields,
        reason,
    }
}

fn push_plan_node(
    plan: &mut Vec<PlanNode>,
    op: PlanOp,
    rows: Span,
    required_fields: AccessSet,
    byte_closure: ClosureSpec,
    guarantee: OutputGuarantee,
    authorization: Authorization,
) {
    plan.push(PlanNode {
        id: PlanNodeId(plan.len()),
        op,
        rows: RowDomain(rows),
        required_fields,
        byte_closure,
        guarantee,
        authorization,
    });
}

fn query_rows(len: usize, query: &Query) -> Result<Span, String> {
    if let Query::Count {
        predicate: Predicate::Between { .. },
    } = query
    {
        return Err(
            "Count has no authorized plan for Predicate::Between; use Filter and count the bitmap"
                .into(),
        );
    }
    let rows = match *query {
        Query::Get { row } if row < len => Span::new(row, row + 1)?,
        Query::Get { .. } => return Err("GET row is outside column".into()),
        Query::Sum { rows } | Query::Between { rows, .. }
            if rows.start <= rows.end && rows.end <= len =>
        {
            rows
        }
        Query::Filter { .. } | Query::Count { .. } => Span::new(0, len)?,
        _ => return Err("query row range is invalid".into()),
    };
    let invalid_bounds = match query {
        Query::Between { low, high, .. } => low > high,
        Query::Filter {
            predicate: Predicate::Between { low, high },
        } => low > high,
        _ => false,
    };
    if invalid_bounds {
        return Err("BETWEEN bounds are invalid".into());
    }
    Ok(rows)
}
