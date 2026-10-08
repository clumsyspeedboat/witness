//! Three-layer access-closure compiler experiment.

mod certificates;
mod codegen;
mod compiler;
mod cost_model;
mod decoder;
mod encode;
pub mod floats;
mod format;
mod heldout;
mod ids;
mod interpreter;
mod invariants;
mod layout;
pub mod paged;
pub mod paged_file;
mod plan;
mod runtime;
mod schedule;
mod static_baseline;
pub mod strings;
mod support;
pub mod vector;

pub use certificates::{
    BlockBloom, BlockMinMax, CandidatePlan, SparseFence, intersect_candidates, refine_eq, refine_in,
};
pub use codegen::generate_rust_module;
pub use compiler::compile;
pub use cost_model::{CostFeatures, CostModel, CostObservation};
pub use decoder::{DecoderIr, DecoderNode, DeltaCoding};
pub use encode::{EncodedColumn, InputColumn, Recipe, encode};
pub use format::{CheckedInvariants, SerializedPage};
pub use heldout::{DataProfile, HeldoutCase, heldout_cases, input_for, primitive_rule_fingerprint};
pub use ids::{FieldId, FrameId, NodeId, PlanNodeId};
pub use interpreter::{
    Answer, Execution, execute_count_runs, execute_full_decode, execute_fused_decode,
    execute_interpreted,
};
pub use invariants::{
    AccessInvariant, Assumption, BoundSource, Evidence, FactScope, InvariantFact, InvariantSet,
    MappingInvariant, NullPlacement, Property, ValueInvariant, derive_invariants,
};
pub use layout::{
    AccessSet, ByteClosure, DependencyRule, FieldLayout, FieldLocation, FrameCodec, FrameLayout,
    LayoutIr, Span,
};
pub use plan::{
    Authorization, ClosureSpec, OutputGuarantee, PlanIr, PlanNode, PlanOp, Predicate, Query,
    RowDomain,
};
pub use runtime::{AccessMetrics, ClosureMode, ReadSession};
pub use schedule::{ReadSchedule, coalesce_ranges};
pub use static_baseline::{
    StaticBitPack, StaticDecoder, StaticDelta, StaticDictionary, StaticFor, StaticNullable,
    StaticPatch, StaticRle, static_between, static_dictionary_filter, static_get,
    static_monotone_filter, static_piecewise_monotone_filter, static_sum,
};
pub use support::{
    BATCH_ROWS, bound_interval, find_position, load_metadata, locate_indexed, lower_bound_position,
    nullable_is_valid, nullable_prefix_rank, nullable_rank, push_batch_matches, push_range,
    sum_batch, unzigzag,
};
