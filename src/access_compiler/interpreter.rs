use super::{
    AccessMetrics, ClosureMode, DecoderNode, DeltaCoding, EncodedColumn, FieldId, NodeId,
    Predicate, Query, ReadSession, Span,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Answer {
    Value(Option<i64>),
    Sum(i128),
    Ranges(Vec<Span>),
    Count(usize),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Execution {
    pub answer: Answer,
    pub metrics: AccessMetrics,
    pub decoded_rows: usize,
}

pub fn execute_interpreted(
    column: &EncodedColumn,
    query: &Query,
    mode: ClosureMode,
) -> Result<Execution, String> {
    validate_query(column.truth.len(), query)?;
    let mut session = ReadSession::new(&column.page, mode);
    load_metadata(&column.page, &mut session)?;
    let mut decoded_rows = 0;
    let answer = execute_rows(column, query, &mut session, &mut decoded_rows, false)?;
    Ok(Execution {
        answer,
        metrics: session.metrics(),
        decoded_rows,
    })
}

pub fn execute_full_decode(
    column: &EncodedColumn,
    query: &Query,
    mode: ClosureMode,
) -> Result<Execution, String> {
    validate_query(column.truth.len(), query)?;
    let mut session = ReadSession::new(&column.page, mode);
    load_metadata(&column.page, &mut session)?;
    let mut values = Vec::with_capacity(column.truth.len());
    for row in 0..column.truth.len() {
        values.push(get(column, column.decoder.root(), row, &mut session)?);
    }
    let answer = query_materialized(&values, query)?;
    Ok(Execution {
        answer,
        metrics: session.metrics(),
        decoded_rows: column.truth.len(),
    })
}

pub fn execute_fused_decode(
    column: &EncodedColumn,
    query: &Query,
    mode: ClosureMode,
) -> Result<Execution, String> {
    validate_query(column.truth.len(), query)?;
    let mut session = ReadSession::new(&column.page, mode);
    load_metadata(&column.page, &mut session)?;
    let mut decoded_rows = 0;
    let answer = execute_rows(column, query, &mut session, &mut decoded_rows, true)?;
    Ok(Execution {
        answer,
        metrics: session.metrics(),
        decoded_rows,
    })
}

/// Executes the `PlanOp::CountRuns` mechanism directly: decodes each run's
/// value once and adds its length on a match, touching `runs` decoded values
/// rather than `rows` regardless of match count. Requires an `Rle` root;
/// callers should confirm `compile` authorized `CountRuns` before calling
/// this rather than falling back to [`count_matching`] silently.
pub fn execute_count_runs(
    column: &EncodedColumn,
    target: i64,
    mode: ClosureMode,
) -> Result<Execution, String> {
    let mut session = ReadSession::new(&column.page, mode);
    load_metadata(&column.page, &mut session)?;
    let (values, run_lengths, runs) = match column.decoder.node(column.decoder.root())? {
        DecoderNode::Rle {
            values,
            run_lengths,
            runs,
            ..
        } => (*values, *run_lengths, *runs),
        _ => return Err("execute_count_runs requires an Rle root".into()),
    };
    let mut count = 0_usize;
    let mut decoded_runs = 0_usize;
    for run in 0..runs {
        let value = get(column, values, run, &mut session)?;
        decoded_runs += 1;
        if value == Some(target) {
            count += session.read_u32(run_lengths, run)? as usize;
        }
    }
    Ok(Execution {
        answer: Answer::Count(count),
        metrics: session.metrics(),
        decoded_rows: decoded_runs,
    })
}

fn execute_rows(
    column: &EncodedColumn,
    query: &Query,
    session: &mut ReadSession<'_>,
    decoded_rows: &mut usize,
    full_stream: bool,
) -> Result<Answer, String> {
    match *query {
        Query::Get { row } => {
            if full_stream {
                let mut answer = None;
                for current in 0..column.truth.len() {
                    let value = get(column, column.decoder.root(), current, session)?;
                    if current == row {
                        answer = Some(value);
                    }
                    *decoded_rows += 1;
                }
                Ok(Answer::Value(answer.ok_or("fused GET missed its row")?))
            } else {
                *decoded_rows = 1;
                Ok(Answer::Value(get(
                    column,
                    column.decoder.root(),
                    row,
                    session,
                )?))
            }
        }
        Query::Sum { rows } => {
            let domain = if full_stream {
                Span::new(0, column.truth.len())?
            } else {
                rows
            };
            let mut sum = 0_i128;
            for row in domain.start..domain.end {
                let value = get(column, column.decoder.root(), row, session)?;
                if rows.start <= row
                    && row < rows.end
                    && let Some(value) = value
                {
                    sum = sum.checked_add(i128::from(value)).ok_or("SUM overflow")?;
                }
                *decoded_rows += 1;
            }
            Ok(Answer::Sum(sum))
        }
        Query::Between { rows, low, high } => {
            let domain = if full_stream {
                Span::new(0, column.truth.len())?
            } else {
                rows
            };
            let mut ranges = Vec::new();
            for row in domain.start..domain.end {
                let value = get(column, column.decoder.root(), row, session)?;
                if rows.start <= row
                    && row < rows.end
                    && value.is_some_and(|value| low <= value && value <= high)
                {
                    push_range(&mut ranges, row, row + 1);
                }
                *decoded_rows += 1;
            }
            Ok(Answer::Ranges(ranges))
        }
        Query::Filter {
            predicate: Predicate::Between { low, high },
        } => filter_between(
            column,
            Span::new(0, column.truth.len())?,
            low,
            high,
            session,
            decoded_rows,
        ),
        Query::Filter {
            predicate: Predicate::Equals { value },
        } => filter_equals(
            column,
            Span::new(0, column.truth.len())?,
            value,
            session,
            decoded_rows,
        ),
        Query::Count {
            predicate: Predicate::Equals { value },
        } => count_matching(
            column,
            Span::new(0, column.truth.len())?,
            move |candidate| candidate == value,
            session,
            decoded_rows,
        ),
        Query::Count {
            predicate: Predicate::Between { low, high },
        } => count_matching(
            column,
            Span::new(0, column.truth.len())?,
            move |candidate| low <= candidate && candidate <= high,
            session,
            decoded_rows,
        ),
    }
}

fn get(
    column: &EncodedColumn,
    node_id: NodeId,
    row: usize,
    session: &mut ReadSession<'_>,
) -> Result<Option<i64>, String> {
    let node = column.decoder.node(node_id)?;
    if row >= node.len(column.decoder.nodes())? {
        return Err(format!("row {row} is outside decoder node {}", node_id.0));
    }
    let value = match node {
        DecoderNode::BitUnpack {
            stream,
            width,
            miniblock_rows,
            miniblock_bytes,
            ..
        } => Some(
            i64::try_from(session.read_bits(
                *stream,
                row,
                *width,
                *miniblock_rows,
                *miniblock_bytes,
            )?)
            .map_err(|_| "BitUnpack value does not fit i64")?,
        ),
        DecoderNode::For { base, values } => {
            let base = session.read_i64(*base, 0)?;
            get(column, *values, row, session)?
                .map(|value| value.checked_add(base).ok_or("FOR overflow"))
                .transpose()?
        }
        DecoderNode::Delta {
            deltas,
            restarts,
            restart_interval,
            coding,
            ..
        } => {
            let block = row / restart_interval;
            let block_start = block * restart_interval;
            let mut value = i128::from(session.read_i64(*restarts, block)?);
            for position in block_start + 1..=row {
                let code = u64::try_from(
                    get(column, *deltas, position, session)?
                        .ok_or("delta code unexpectedly null")?,
                )
                .map_err(|_| "delta code does not fit u64")?;
                let delta = match coding {
                    DeltaCoding::ZigZag => i128::from(unzigzag(code)),
                    DeltaCoding::Unsigned => i128::from(code),
                };
                value = value
                    .checked_add(delta)
                    .ok_or("delta reconstruction overflow")?;
            }
            Some(i64::try_from(value).map_err(|_| "delta result does not fit i64")?)
        }
        DecoderNode::Rle {
            values,
            run_lengths,
            run_index,
            runs,
            index_interval,
            ..
        } => {
            let run = locate_indexed(
                session,
                *run_index,
                *run_lengths,
                *runs,
                *index_interval,
                row,
            )?;
            get(column, *values, run, session)?
        }
        DecoderNode::Dictionary {
            ids,
            dictionary,
            entries,
            ..
        } => {
            let id =
                get(column, *ids, row, session)?.ok_or("dictionary id unexpectedly null")? as usize;
            if id >= *entries {
                return Err("dictionary id is outside the dictionary".into());
            }
            Some(session.read_i64(*dictionary, id)?)
        }
        DecoderNode::Patch {
            values,
            positions,
            position_index,
            exceptions,
            count,
            index_interval,
        } => match find_position(
            session,
            *position_index,
            *positions,
            *count,
            *index_interval,
            row,
        )? {
            Some(exception) => Some(session.read_i64(*exceptions, exception)?),
            None => get(column, *values, row, session)?,
        },
        DecoderNode::Nullable {
            validity,
            rank_index,
            values,
            rank_interval,
            ..
        } => {
            let byte = session.read_u8(*validity, row / 8)?;
            if byte >> (row % 8) & 1 == 0 {
                None
            } else {
                let block = row / rank_interval;
                let mut rank = session.read_u32(*rank_index, block)? as usize;
                let block_start = block * rank_interval;
                for validity_byte in block_start / 8..row / 8 {
                    rank += session.read_u8(*validity, validity_byte)?.count_ones() as usize;
                }
                let prior_mask = (1_u16 << (row % 8)) - 1;
                rank += (u16::from(byte) & prior_mask).count_ones() as usize;
                get(column, *values, rank, session)?
            }
        }
    };
    Ok(value)
}

fn locate_indexed(
    session: &mut ReadSession<'_>,
    index_field: FieldId,
    lengths_field: FieldId,
    runs: usize,
    interval: usize,
    target_row: usize,
) -> Result<usize, String> {
    let entries = runs.div_ceil(interval) + 1;
    let mut low = 0;
    let mut high = entries;
    while low + 1 < high {
        let mid = low + (high - low) / 2;
        if session.read_u32(index_field, mid * 2)? as usize <= target_row {
            low = mid;
        } else {
            high = mid;
        }
    }
    let mut row = session.read_u32(index_field, low * 2)? as usize;
    let mut run = session.read_u32(index_field, low * 2 + 1)? as usize;
    while run < runs {
        row = row
            .checked_add(session.read_u32(lengths_field, run)? as usize)
            .ok_or("RLE row overflow")?;
        if target_row < row {
            return Ok(run);
        }
        run += 1;
    }
    Err("RLE index did not cover target row".into())
}

fn find_position(
    session: &mut ReadSession<'_>,
    index_field: FieldId,
    positions_field: FieldId,
    count: usize,
    interval: usize,
    target: usize,
) -> Result<Option<usize>, String> {
    if count == 0 {
        return Ok(None);
    }
    let entries = count.div_ceil(interval) + 1;
    let mut low = 0;
    let mut high = entries;
    while low + 1 < high {
        let mid = low + (high - low) / 2;
        if session.read_u32(index_field, mid * 2)? as usize <= target {
            low = mid;
        } else {
            high = mid;
        }
    }
    let start = session.read_u32(index_field, low * 2 + 1)? as usize;
    for exception in start..(start + interval).min(count) {
        match (session.read_u32(positions_field, exception)? as usize).cmp(&target) {
            std::cmp::Ordering::Less => {}
            std::cmp::Ordering::Equal => return Ok(Some(exception)),
            std::cmp::Ordering::Greater => break,
        }
    }
    Ok(None)
}

fn query_materialized(values: &[Option<i64>], query: &Query) -> Result<Answer, String> {
    match *query {
        Query::Get { row } => Ok(Answer::Value(values[row])),
        Query::Sum { rows } => Ok(Answer::Sum(
            values[rows.start..rows.end]
                .iter()
                .flatten()
                .map(|&value| i128::from(value))
                .sum(),
        )),
        Query::Between { rows, low, high } => {
            let mut ranges = Vec::new();
            for (offset, value) in values[rows.start..rows.end].iter().enumerate() {
                if value.is_some_and(|value| low <= value && value <= high) {
                    push_range(&mut ranges, rows.start + offset, rows.start + offset + 1);
                }
            }
            Ok(Answer::Ranges(ranges))
        }
        Query::Filter {
            predicate: Predicate::Between { low, high },
        } => {
            let mut ranges = Vec::new();
            for (row, value) in values.iter().enumerate() {
                if value.is_some_and(|value| low <= value && value <= high) {
                    push_range(&mut ranges, row, row + 1);
                }
            }
            Ok(Answer::Ranges(ranges))
        }
        Query::Filter {
            predicate: Predicate::Equals { value: target },
        } => {
            let mut ranges = Vec::new();
            for (row, value) in values.iter().enumerate() {
                if *value == Some(target) {
                    push_range(&mut ranges, row, row + 1);
                }
            }
            Ok(Answer::Ranges(ranges))
        }
        Query::Count {
            predicate: Predicate::Equals { value: target },
        } => Ok(Answer::Count(
            values
                .iter()
                .filter(|value| **value == Some(target))
                .count(),
        )),
        Query::Count {
            predicate: Predicate::Between { low, high },
        } => Ok(Answer::Count(
            values
                .iter()
                .filter(|value| value.is_some_and(|value| low <= value && value <= high))
                .count(),
        )),
    }
}

fn validate_query(len: usize, query: &Query) -> Result<(), String> {
    match query {
        Query::Get { row } if *row >= len => Err("GET row is outside column".into()),
        Query::Sum { rows } | Query::Between { rows, .. }
            if rows.start > rows.end || rows.end > len =>
        {
            Err("query row range is invalid".into())
        }
        Query::Between { low, high, .. } if low > high => Err("BETWEEN bounds are invalid".into()),
        Query::Filter {
            predicate: Predicate::Between { low, high },
        } if low > high => Err("BETWEEN bounds are invalid".into()),
        _ => Ok(()),
    }
}

fn filter_between(
    column: &EncodedColumn,
    rows: Span,
    low: i64,
    high: i64,
    session: &mut ReadSession<'_>,
    decoded_rows: &mut usize,
) -> Result<Answer, String> {
    let mut ranges = Vec::new();
    for row in rows.start..rows.end {
        let value = get(column, column.decoder.root(), row, session)?;
        if value.is_some_and(|value| low <= value && value <= high) {
            push_range(&mut ranges, row, row + 1);
        }
        *decoded_rows += 1;
    }
    Ok(Answer::Ranges(ranges))
}

fn filter_equals(
    column: &EncodedColumn,
    rows: Span,
    target: i64,
    session: &mut ReadSession<'_>,
    decoded_rows: &mut usize,
) -> Result<Answer, String> {
    let mut ranges = Vec::new();
    for row in rows.start..rows.end {
        let value = get(column, column.decoder.root(), row, session)?;
        if value == Some(target) {
            push_range(&mut ranges, row, row + 1);
        }
        *decoded_rows += 1;
    }
    Ok(Answer::Ranges(ranges))
}

/// Ground-truth reference count: decode every row and test it directly.
/// Deliberately unoptimized so it is a trustworthy check for any authorized
/// fast path (e.g. `PlanOp::CountRuns`).
fn count_matching(
    column: &EncodedColumn,
    rows: Span,
    matches: impl Fn(i64) -> bool,
    session: &mut ReadSession<'_>,
    decoded_rows: &mut usize,
) -> Result<Answer, String> {
    let mut count = 0;
    for row in rows.start..rows.end {
        let value = get(column, column.decoder.root(), row, session)?;
        if value.is_some_and(&matches) {
            count += 1;
        }
        *decoded_rows += 1;
    }
    Ok(Answer::Count(count))
}

fn load_metadata(
    page: &super::SerializedPage,
    session: &mut ReadSession<'_>,
) -> Result<(), String> {
    let metadata = &page.layout().fields[page.layout().metadata.0];
    session
        .read_range(metadata.id, Span::new(0, metadata.length)?)
        .map(|_| ())
}

fn push_range(ranges: &mut Vec<Span>, start: usize, end: usize) {
    if let Some(last) = ranges.last_mut()
        && last.end == start
    {
        last.end = end;
    } else {
        ranges.push(Span { start, end });
    }
}

fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}
