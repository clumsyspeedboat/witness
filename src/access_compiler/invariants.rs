use std::collections::BTreeSet;

use super::{CheckedInvariants, DecoderIr, DecoderNode, DeltaCoding, LayoutIr, NodeId};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum NullPlacement {
    NoNulls,
    First,
    Last,
    Arbitrary,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum FactScope {
    AllRows,
    NonNullRows,
    Mapping,
    Physical,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Evidence {
    Structural(&'static str),
    CheckedDescriptor(&'static str),
    Probabilistic(&'static str),
    Layout(&'static str),
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Assumption {
    CheckedArithmetic,
    PositiveRunLengths,
    SortedUniqueDictionary,
    TrustedDescriptor,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ValueInvariant {
    NonDecreasing,
    PiecewiseNonDecreasing { max_rows: usize },
    NullPlacement(NullPlacement),
    UnsignedRange { width: u8 },
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum MappingInvariant {
    OrderPreserving,
    Injective,
    AffineShift,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AccessInvariant {
    Miniblock { rows: usize },
    RestartBound { rows: usize },
    RunIndex { max_runs: usize },
    PatchIndex { max_exceptions: usize },
    ValidityRank { max_rows: usize },
    EnclosingFrame { compressed_bytes: usize },
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Property {
    Value(ValueInvariant),
    Mapping(MappingInvariant),
    Access(AccessInvariant),
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct InvariantFact {
    pub node: NodeId,
    pub scope: FactScope,
    pub property: Property,
    pub evidence: Evidence,
    pub assumptions: BTreeSet<Assumption>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InvariantSet {
    facts: BTreeSet<InvariantFact>,
}

impl InvariantSet {
    pub fn iter(&self) -> impl Iterator<Item = &InvariantFact> {
        self.facts.iter()
    }

    pub fn contains(&self, node: NodeId, scope: FactScope, property: Property) -> bool {
        self.facts
            .iter()
            .any(|fact| fact.node == node && fact.scope == scope && fact.property == property)
    }

    pub fn root_monotone_nulls(&self, root: NodeId) -> Option<NullPlacement> {
        if self.contains(
            root,
            FactScope::AllRows,
            Property::Value(ValueInvariant::NonDecreasing),
        ) {
            return Some(NullPlacement::NoNulls);
        }
        if !self.contains(
            root,
            FactScope::NonNullRows,
            Property::Value(ValueInvariant::NonDecreasing),
        ) {
            return None;
        }
        self.facts.iter().find_map(|fact| match fact.property {
            Property::Value(ValueInvariant::NullPlacement(placement))
                if fact.node == root && fact.scope == FactScope::AllRows =>
            {
                Some(placement)
            }
            _ => None,
        })
    }

    pub fn root_piecewise_monotone_rows(&self, root: NodeId) -> Option<usize> {
        self.facts.iter().find_map(|fact| match fact.property {
            Property::Value(ValueInvariant::PiecewiseNonDecreasing { max_rows })
                if fact.node == root && fact.scope == FactScope::AllRows =>
            {
                Some(max_rows)
            }
            _ => None,
        })
    }

    fn insert(
        &mut self,
        node: NodeId,
        scope: FactScope,
        property: Property,
        evidence: Evidence,
        assumptions: impl IntoIterator<Item = Assumption>,
    ) {
        self.facts.insert(InvariantFact {
            node,
            scope,
            property,
            evidence,
            assumptions: assumptions.into_iter().collect(),
        });
    }

    fn has_value(&self, node: NodeId, scope: FactScope, property: ValueInvariant) -> bool {
        self.contains(node, scope, Property::Value(property))
    }
}

pub fn derive_invariants(
    decoder: &DecoderIr,
    layout: &LayoutIr,
    checked: CheckedInvariants,
) -> Result<InvariantSet, String> {
    decoder.len()?;
    layout.validate()?;
    let mut output = InvariantSet::default();
    for (index, node) in decoder.nodes().iter().enumerate() {
        derive_node(NodeId(index), node, &mut output);
    }
    let root = decoder.root();
    output.insert(
        root,
        FactScope::AllRows,
        Property::Value(ValueInvariant::NullPlacement(checked.null_placement)),
        Evidence::CheckedDescriptor("null_placement"),
        [Assumption::TrustedDescriptor],
    );
    if checked.non_decreasing {
        output.insert(
            root,
            FactScope::AllRows,
            Property::Value(ValueInvariant::NonDecreasing),
            Evidence::CheckedDescriptor("non_decreasing"),
            [Assumption::TrustedDescriptor],
        );
    } else if checked.non_decreasing_non_null {
        output.insert(
            root,
            FactScope::NonNullRows,
            Property::Value(ValueInvariant::NonDecreasing),
            Evidence::CheckedDescriptor("non_decreasing_non_null"),
            [Assumption::TrustedDescriptor],
        );
    }
    if let Some(frame) = layout.frames.first() {
        output.insert(
            root,
            FactScope::Physical,
            Property::Access(AccessInvariant::EnclosingFrame {
                compressed_bytes: frame.compressed_length,
            }),
            Evidence::Layout("enclosing_frame"),
            [],
        );
    }
    Ok(output)
}

fn derive_node(node_id: NodeId, node: &DecoderNode, output: &mut InvariantSet) {
    match node {
        DecoderNode::BitUnpack {
            width,
            miniblock_rows,
            ..
        } => {
            output.insert(
                node_id,
                FactScope::AllRows,
                Property::Value(ValueInvariant::UnsignedRange { width: *width }),
                Evidence::Structural("bit_unpack_unsigned_domain"),
                [],
            );
            output.insert(
                node_id,
                FactScope::Physical,
                Property::Access(AccessInvariant::Miniblock {
                    rows: *miniblock_rows,
                }),
                Evidence::Structural("bit_unpack_miniblock"),
                [],
            );
        }
        DecoderNode::For { values, .. } => {
            output.insert(
                node_id,
                FactScope::Mapping,
                Property::Mapping(MappingInvariant::OrderPreserving),
                Evidence::Structural("for_checked_affine_shift"),
                [Assumption::CheckedArithmetic],
            );
            output.insert(
                node_id,
                FactScope::Mapping,
                Property::Mapping(MappingInvariant::Injective),
                Evidence::Structural("for_checked_affine_shift"),
                [Assumption::CheckedArithmetic],
            );
            output.insert(
                node_id,
                FactScope::Mapping,
                Property::Mapping(MappingInvariant::AffineShift),
                Evidence::Structural("for_checked_affine_shift"),
                [Assumption::CheckedArithmetic],
            );
            transfer_monotone(*values, node_id, FactScope::AllRows, output);
        }
        DecoderNode::Delta {
            deltas,
            coding,
            restart_interval,
            len,
            ..
        } => {
            output.insert(
                node_id,
                FactScope::Physical,
                Property::Access(AccessInvariant::RestartBound {
                    rows: *restart_interval,
                }),
                Evidence::Structural("delta_restart"),
                [],
            );
            if *coding == DeltaCoding::Unsigned && unsigned_width(output, *deltas).is_some() {
                let property = if *restart_interval >= *len {
                    ValueInvariant::NonDecreasing
                } else {
                    ValueInvariant::PiecewiseNonDecreasing {
                        max_rows: *restart_interval,
                    }
                };
                output.insert(
                    node_id,
                    FactScope::AllRows,
                    Property::Value(property),
                    Evidence::Structural("unsigned_delta_segments"),
                    [Assumption::CheckedArithmetic],
                );
            }
        }
        DecoderNode::Dictionary {
            ids, sorted_unique, ..
        } => {
            if *sorted_unique {
                output.insert(
                    node_id,
                    FactScope::Mapping,
                    Property::Mapping(MappingInvariant::OrderPreserving),
                    Evidence::Structural("sorted_unique_dictionary"),
                    [Assumption::SortedUniqueDictionary],
                );
                output.insert(
                    node_id,
                    FactScope::Mapping,
                    Property::Mapping(MappingInvariant::Injective),
                    Evidence::Structural("sorted_unique_dictionary"),
                    [Assumption::SortedUniqueDictionary],
                );
                transfer_monotone(*ids, node_id, FactScope::AllRows, output);
            }
        }
        DecoderNode::Rle {
            values,
            index_interval,
            ..
        } => {
            transfer_monotone(*values, node_id, FactScope::AllRows, output);
            output.insert(
                node_id,
                FactScope::Physical,
                Property::Access(AccessInvariant::RunIndex {
                    max_runs: *index_interval,
                }),
                Evidence::Structural("rle_run_index"),
                [Assumption::PositiveRunLengths],
            );
        }
        DecoderNode::Patch { index_interval, .. } => output.insert(
            node_id,
            FactScope::Physical,
            Property::Access(AccessInvariant::PatchIndex {
                max_exceptions: *index_interval,
            }),
            Evidence::Structural("patch_position_index"),
            [],
        ),
        DecoderNode::Nullable {
            values,
            rank_interval,
            ..
        } => {
            transfer_monotone(*values, node_id, FactScope::NonNullRows, output);
            output.insert(
                node_id,
                FactScope::Physical,
                Property::Access(AccessInvariant::ValidityRank {
                    max_rows: *rank_interval,
                }),
                Evidence::Structural("nullable_rank_index"),
                [],
            );
        }
    }
}

fn transfer_monotone(
    child: NodeId,
    parent: NodeId,
    parent_scope: FactScope,
    output: &mut InvariantSet,
) {
    if output.has_value(child, FactScope::AllRows, ValueInvariant::NonDecreasing) {
        output.insert(
            parent,
            parent_scope,
            Property::Value(ValueInvariant::NonDecreasing),
            Evidence::Structural("order_preserving_composition"),
            [Assumption::CheckedArithmetic],
        );
    }
}

fn unsigned_width(output: &InvariantSet, node: NodeId) -> Option<u8> {
    output.iter().find_map(|fact| match fact.property {
        Property::Value(ValueInvariant::UnsignedRange { width })
            if fact.node == node && fact.scope == FactScope::AllRows =>
        {
            Some(width)
        }
        _ => None,
    })
}
