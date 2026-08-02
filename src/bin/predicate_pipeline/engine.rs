use witness::access_compiler::{
    AccessMetrics, Answer, ClosureMode, EncodedColumn, ReadSession, Span,
};

use crate::generated;

#[derive(Clone, Copy, Debug)]
pub enum ValuePlan {
    Selective,
    Fused,
}

#[derive(Clone, Copy, Debug)]
pub enum FilterPlan {
    Compiled,
    FullScan,
}

#[derive(Clone, Copy, Debug)]
pub struct PipelinePlans {
    pub value: ValuePlan,
    pub filter: FilterPlan,
}

impl ValuePlan {
    pub fn name(self) -> &'static str {
        match self {
            Self::Selective => "generated_selective",
            Self::Fused => "generated_fused",
        }
    }
}

#[derive(Clone, Debug)]
pub struct PipelineResult {
    pub sum: i128,
    pub ranges: Vec<Span>,
    pub selector_metrics: AccessMetrics,
    pub value_metrics: AccessMetrics,
    pub selector_decoded_rows: usize,
    pub value_decoded_rows: usize,
    pub selector_primitive_values_read: usize,
    pub value_primitive_values_read: usize,
}

pub fn known_selection(
    case: usize,
    column: &EncodedColumn,
    ranges: &[Span],
    plan: ValuePlan,
) -> Result<PipelineResult, String> {
    known_selection_mode(case, column, ranges, plan, true)
}

pub fn known_selection_untracked(
    case: usize,
    column: &EncodedColumn,
    ranges: &[Span],
    plan: ValuePlan,
) -> Result<PipelineResult, String> {
    known_selection_mode(case, column, ranges, plan, false)
}

fn known_selection_mode(
    case: usize,
    column: &EncodedColumn,
    ranges: &[Span],
    plan: ValuePlan,
    tracked: bool,
) -> Result<PipelineResult, String> {
    let mode = match plan {
        ValuePlan::Selective => ClosureMode::Selective,
        ValuePlan::Fused => ClosureMode::FullPage,
    };
    let mut session = session(&column.page, mode, tracked);
    let execution = match plan {
        ValuePlan::Selective => {
            generated::SUM_RANGES_SESSION_FNS[case](column, ranges, &mut session)?
        }
        ValuePlan::Fused => {
            generated::FUSED_SUM_RANGES_SESSION_FNS[case](column, ranges, &mut session)?
        }
    };
    Ok(PipelineResult {
        sum: answer_sum(execution.answer)?,
        ranges: ranges.to_vec(),
        selector_metrics: zero_metrics(),
        value_metrics: session.metrics(),
        selector_decoded_rows: 0,
        value_decoded_rows: execution.decoded_rows,
        selector_primitive_values_read: 0,
        value_primitive_values_read: session.primitive_values_read(),
    })
}

pub fn complete_query(
    selector_case: usize,
    selector: &EncodedColumn,
    value_case: usize,
    value: &EncodedColumn,
    low: i64,
    high: i64,
    plans: PipelinePlans,
) -> Result<PipelineResult, String> {
    complete_query_mode(
        selector_case,
        selector,
        value_case,
        value,
        low,
        high,
        plans,
        true,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn complete_query_untracked(
    selector_case: usize,
    selector: &EncodedColumn,
    value_case: usize,
    value: &EncodedColumn,
    low: i64,
    high: i64,
    plans: PipelinePlans,
) -> Result<PipelineResult, String> {
    complete_query_mode(
        selector_case,
        selector,
        value_case,
        value,
        low,
        high,
        plans,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
fn complete_query_mode(
    selector_case: usize,
    selector: &EncodedColumn,
    value_case: usize,
    value: &EncodedColumn,
    low: i64,
    high: i64,
    plans: PipelinePlans,
    tracked: bool,
) -> Result<PipelineResult, String> {
    if selector.truth.len() != value.truth.len() {
        return Err("pipeline columns have different row counts".into());
    }
    let selector_mode = match plans.value {
        ValuePlan::Selective => ClosureMode::Selective,
        ValuePlan::Fused => ClosureMode::FullPage,
    };
    let mut selector_session = session(&selector.page, selector_mode, tracked);
    let filter = match plans.filter {
        FilterPlan::Compiled => generated::FILTER_SESSION_FNS[selector_case](
            selector,
            low,
            high,
            &mut selector_session,
        )?,
        FilterPlan::FullScan => generated::FILTER_SCAN_SESSION_FNS[selector_case](
            selector,
            low,
            high,
            &mut selector_session,
        )?,
    };
    let ranges = match filter.answer {
        Answer::Ranges(ranges) => ranges,
        _ => return Err("generated filter did not return ranges".into()),
    };
    let mut result = known_selection_mode(value_case, value, &ranges, plans.value, tracked)?;
    result.selector_metrics = selector_session.metrics();
    result.selector_decoded_rows = filter.decoded_rows;
    result.selector_primitive_values_read = selector_session.primitive_values_read();
    Ok(result)
}

fn session<'a>(
    page: &'a witness::access_compiler::SerializedPage,
    mode: ClosureMode,
    tracked: bool,
) -> ReadSession<'a> {
    if tracked {
        ReadSession::new(page, mode)
    } else {
        ReadSession::new_untracked(page, mode)
    }
}

fn answer_sum(answer: Answer) -> Result<i128, String> {
    match answer {
        Answer::Sum(sum) => Ok(sum),
        _ => Err("generated aggregate did not return a sum".into()),
    }
}

fn zero_metrics() -> AccessMetrics {
    AccessMetrics {
        logical_bytes: 0,
        delivered_bytes: 0,
        transferred_bytes: 0,
        transfer_operations: 0,
        frames_decoded: 0,
    }
}
