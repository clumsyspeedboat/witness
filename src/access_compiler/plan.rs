use super::{
    AccessInvariant, AccessSet, ByteClosure, FactScope, FieldId, InvariantSet, MappingInvariant,
    NodeId, NullPlacement, PlanNodeId, Property, Span, ValueInvariant,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Predicate {
    Between { low: i64, high: i64 },
    Equals { value: i64 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Query {
    Get {
        row: usize,
    },
    Sum {
        rows: Span,
    },
    Between {
        rows: Span,
        low: i64,
        high: i64,
    },
    Filter {
        predicate: Predicate,
    },
    /// Exact row count matching `predicate`. Restricted to
    /// `Predicate::Equals`: a between-predicate count has no dedicated
    /// authorized plan and is rejected rather than silently falling back.
    Count {
        predicate: Predicate,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RowDomain(pub Span);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputGuarantee {
    PhysicalBytes,
    ExactScalar,
    ExactBitmap,
    CandidateBitmap,
    MaterializedValues,
    FallbackRequired,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlanOp {
    ReadRange {
        field: FieldId,
        bytes: Span,
    },
    ReadBlock {
        field: FieldId,
        block: usize,
    },
    LoadMetadata {
        field: FieldId,
    },
    DecodeBlock {
        field: FieldId,
        block: usize,
    },
    SeekRestart {
        field: FieldId,
        restart: usize,
    },
    SearchMonotone {
        nulls: NullPlacement,
    },
    SearchPiecewiseMonotone {
        max_rows: usize,
    },
    ProbeBloom {
        blocks: usize,
        hashes: u8,
    },
    ProbeMinMax {
        blocks: usize,
    },
    ProbeSparseFence {
        entries: usize,
    },
    TranslateDictionaryRange {
        dictionary: FieldId,
        entries: usize,
    },
    /// Exact count from run lengths of matching runs, without decoding any
    /// repeated row. Authorized only when the root is run-length encoded.
    CountRuns {
        run_lengths: FieldId,
    },
    /// Exact count by decoding and comparing every value; the fallback when
    /// no run-length structure authorizes `CountRuns`.
    CountExact,
    RefineCandidates,
    AggregateEncoded,
    MaterializeRows,
    FusedDecodeQuery,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClosureSpec {
    Exact(ByteClosure),
    RuntimeRefined {
        seed: AccessSet,
        possible_fields: Vec<FieldId>,
        reason: String,
    },
}

/// The licence a plan step carries. Steps that only move or decode bytes are
/// `Unconditional`: they are always legal, so they cite nothing. Every step
/// that *skips* work a scan would have done must instead name the derived
/// fact that permits it, and [`PlanIr::check_authorization`] re-verifies that
/// the named fact is really present in the invariant set the column produced.
///
/// Keeping the licence in the IR is what makes authorization checkable rather
/// than merely conventional: a plan that claims an unearned fast path is
/// rejected by the checker, not caught by review.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Authorization {
    Unconditional,
    Fact {
        node: NodeId,
        scope: FactScope,
        property: Property,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanNode {
    pub id: PlanNodeId,
    pub op: PlanOp,
    pub rows: RowDomain,
    pub required_fields: AccessSet,
    pub byte_closure: ClosureSpec,
    pub guarantee: OutputGuarantee,
    pub authorization: Authorization,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanIr {
    pub query: Query,
    pub nodes: Vec<PlanNode>,
    pub output: OutputGuarantee,
    pub fallback_reason: Option<String>,
}

impl PlanOp {
    /// True for steps that answer a query while examining less than a scan
    /// would. These are exactly the steps whose correctness rests on a
    /// derived fact rather than on decoding every row, so the checker demands
    /// a licence for them and refuses one for anything else.
    pub fn skips_scan_work(&self) -> bool {
        matches!(
            self,
            Self::SearchMonotone { .. }
                | Self::SearchPiecewiseMonotone { .. }
                | Self::TranslateDictionaryRange { .. }
                | Self::CountRuns { .. }
                | Self::ProbeSparseFence { .. }
        )
    }

    /// Whether a cited property actually entails this step. Presence of the
    /// fact is not enough: a monotone certificate must not license a run
    /// count, and a run index must not license a boundary search.
    fn licensed_by(&self, scope: FactScope, property: Property) -> bool {
        match self {
            Self::SearchMonotone { .. } | Self::ProbeSparseFence { .. } => {
                matches!(scope, FactScope::AllRows | FactScope::NonNullRows)
                    && property == Property::Value(ValueInvariant::NonDecreasing)
            }
            Self::SearchPiecewiseMonotone { max_rows } => {
                scope == FactScope::AllRows
                    && property
                        == Property::Value(ValueInvariant::PiecewiseNonDecreasing {
                            max_rows: *max_rows,
                        })
            }
            Self::TranslateDictionaryRange { .. } => {
                scope == FactScope::Mapping
                    && property == Property::Mapping(MappingInvariant::OrderPreserving)
            }
            Self::CountRuns { .. } => {
                scope == FactScope::Physical
                    && matches!(property, Property::Access(AccessInvariant::RunIndex { .. }))
            }
            _ => false,
        }
    }
}

impl PlanIr {
    /// Re-derive the authorization contract against the facts the column
    /// actually proves. `compile` runs this on every plan it returns, so a
    /// coding mistake that grants an unearned fast path fails closed here
    /// instead of silently returning wrong rows. Three ways to fail:
    /// skipping work with no licence, citing a fact the column does not
    /// prove, and citing a fact that does not entail the step.
    pub fn check_authorization(&self, invariants: &InvariantSet) -> Result<(), String> {
        for node in &self.nodes {
            let id = node.id.0;
            match &node.authorization {
                Authorization::Unconditional if node.op.skips_scan_work() => {
                    return Err(format!(
                        "plan node {id} ({:?}) skips scan work without citing a fact",
                        node.op
                    ));
                }
                Authorization::Unconditional => {}
                Authorization::Fact { .. } if !node.op.skips_scan_work() => {
                    return Err(format!(
                        "plan node {id} ({:?}) cites a licence it does not need",
                        node.op
                    ));
                }
                Authorization::Fact {
                    node: fact_node,
                    scope,
                    property,
                } => {
                    if !invariants.contains(*fact_node, *scope, *property) {
                        return Err(format!(
                            "plan node {id} cites {property:?} at {scope:?}, which this column does not prove"
                        ));
                    }
                    if !node.op.licensed_by(*scope, *property) {
                        return Err(format!(
                            "plan node {id} cites {property:?}, which does not license {:?}",
                            node.op
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.nodes.is_empty() {
            return Err("plan contains no operations".into());
        }
        for (index, node) in self.nodes.iter().enumerate() {
            if node.id.0 != index {
                return Err(format!("plan node {index} has a non-canonical id"));
            }
        }
        if self.output == OutputGuarantee::FallbackRequired && self.fallback_reason.is_none() {
            return Err("fallback plan has no reason".into());
        }
        Ok(())
    }
}
