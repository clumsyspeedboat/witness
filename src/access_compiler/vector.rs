//! Vectorized execution over the decoder IR.
//!
//! Generated kernels specialize one column's composition when the artifact is
//! built. A paged column holds many pages, each encoded with its own
//! composition, so it needs an executor that reads the decoder IR at run
//! time. This one dispatches per chunk rather than per value: every node turns
//! its child's chunks into its own, so a 1,024-value batch, a run, or a stretch
//! of nulls costs one call per tree node.
//!
//! Reads go through the same `ReadSession` primitives as the generated kernels,
//! so byte accounting means the same thing on both paths.

use super::{
    BATCH_ROWS, DecoderIr, DecoderNode, DeltaCoding, FieldId, NodeId, NullPlacement, ReadSession,
    Span, find_position, locate_indexed, lower_bound_position, nullable_prefix_rank, nullable_rank,
    push_batch_matches, push_range, sum_batch, unzigzag,
};

/// Consecutive decoded rows handed from a node to its parent.
pub enum Chunk<'b> {
    /// Non-null values, one per row.
    Values(&'b mut [i128]),
    /// `len` rows that all hold `value`.
    Run { value: i128, len: usize },
    /// `len` null rows.
    Nulls { len: usize },
}

impl Chunk<'_> {
    pub fn len(&self) -> usize {
        match self {
            Self::Values(values) => values.len(),
            Self::Run { len, .. } | Self::Nulls { len } => *len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Receives every chunk together with the row it starts at.
pub type Sink<'s, 'p> =
    dyn FnMut(&mut ReadSession<'p>, usize, Chunk<'_>) -> Result<(), String> + 's;

/// Streams rows `rows` of the decoded column to `sink` in row order.
pub fn scan<'p>(
    decoder: &DecoderIr,
    session: &mut ReadSession<'p>,
    rows: Span,
    sink: &mut Sink<'_, 'p>,
) -> Result<(), String> {
    if rows.end > decoder.len()? {
        return Err("vector scan exceeds the column".into());
    }
    scan_node(decoder, decoder.root(), session, rows.start, rows.end, sink)
}

/// The value at `row`, or `None` for a null.
pub fn get(
    decoder: &DecoderIr,
    session: &mut ReadSession<'_>,
    row: usize,
) -> Result<Option<i128>, String> {
    if row >= decoder.len()? {
        return Err("vector GET outside the column".into());
    }
    get_node(decoder, decoder.root(), session, row)
}

/// Exact SUM of the non-null values in `rows`.
pub fn sum(decoder: &DecoderIr, session: &mut ReadSession<'_>, rows: Span) -> Result<i128, String> {
    sum_ranges(decoder, session, &[rows])
}

/// Exact SUM of the non-null values in ordered, disjoint `ranges`.
pub fn sum_ranges(
    decoder: &DecoderIr,
    session: &mut ReadSession<'_>,
    ranges: &[Span],
) -> Result<i128, String> {
    validate_ranges(decoder, ranges)?;
    let mut total = 0_i128;
    for range in ranges {
        scan(decoder, session, *range, &mut |_, _, chunk| {
            total = add_chunk(total, &chunk)?;
            Ok(())
        })?;
    }
    Ok(total)
}

/// Rows of `rows` whose value lies in `[low, high]`, by decoding every row.
pub fn between(
    decoder: &DecoderIr,
    session: &mut ReadSession<'_>,
    rows: Span,
    low: i64,
    high: i64,
) -> Result<Vec<Span>, String> {
    if low > high {
        return Err("vector BETWEEN bounds are invalid".into());
    }
    let (low, high) = (i128::from(low), i128::from(high));
    let mut ranges = Vec::new();
    scan(decoder, session, rows, &mut |_, row, chunk| {
        match chunk {
            Chunk::Values(values) => push_batch_matches(&mut ranges, values, row, low, high),
            Chunk::Run { value, len } if low <= value && value <= high => {
                push_range(&mut ranges, row, row + len);
            }
            Chunk::Run { .. } | Chunk::Nulls { .. } => {}
        }
        Ok(())
    })?;
    Ok(ranges)
}

/// Rows of `rows` that hold a value rather than a null. A nullable root
/// answers from its validity bits alone; any other root has no nulls.
pub fn non_null(
    decoder: &DecoderIr,
    session: &mut ReadSession<'_>,
    rows: Span,
) -> Result<Vec<Span>, String> {
    if rows.end > decoder.len()? {
        return Err("vector scan exceeds the column".into());
    }
    let mut ranges = Vec::new();
    if rows.is_empty() {
        return Ok(ranges);
    }
    match *decoder.node(decoder.root())? {
        DecoderNode::Nullable { validity, .. } => {
            let first_byte = rows.start / 8;
            let bits =
                session.read_range(validity, Span::new(first_byte, (rows.end - 1) / 8 + 1)?)?;
            for row in rows.start..rows.end {
                if bits[row / 8 - first_byte] >> (row % 8) & 1 == 1 {
                    push_range(&mut ranges, row, row + 1);
                }
            }
            return Ok(ranges);
        }
        DecoderNode::Rle { .. } | DecoderNode::Patch { .. } => {}
        _ => return Ok(vec![rows]),
    }
    scan(decoder, session, rows, &mut |_, row, chunk| {
        if !matches!(chunk, Chunk::Nulls { .. }) {
            push_range(&mut ranges, row, row + chunk.len());
        }
        Ok(())
    })?;
    Ok(ranges)
}

/// Rows of `rows` whose value lies in `[low, high]`, found by two boundary
/// searches. Sound only when the rows are non-decreasing with nulls placed
/// as `nulls` says; a caller must hold the fact that proves it.
pub fn search_monotone(
    decoder: &DecoderIr,
    session: &mut ReadSession<'_>,
    rows: Span,
    low: i64,
    high: i64,
    nulls: NullPlacement,
) -> Result<Vec<Span>, String> {
    if low > high || nulls == NullPlacement::Arbitrary {
        return Err("vector monotone search contract is invalid".into());
    }
    let start = boundary(decoder, session, rows, i128::from(low), false, nulls)?;
    let end = boundary(
        decoder,
        session,
        Span::new(start, rows.end)?,
        i128::from(high),
        true,
        nulls,
    )?;
    Ok(if start < end {
        vec![Span::new(start, end)?]
    } else {
        Vec::new()
    })
}

/// `search_monotone` within each block of `max_rows` rows: the shape a
/// piecewise order fact licenses.
pub fn search_piecewise(
    decoder: &DecoderIr,
    session: &mut ReadSession<'_>,
    rows: Span,
    low: i64,
    high: i64,
    max_rows: usize,
) -> Result<Vec<Span>, String> {
    if max_rows == 0 {
        return Err("piecewise blocks must be non-empty".into());
    }
    let mut ranges = Vec::new();
    let mut block_start = rows.start / max_rows * max_rows;
    while block_start < rows.end {
        let block = Span::new(
            block_start.max(rows.start),
            (block_start + max_rows).min(rows.end),
        )?;
        for span in search_monotone(decoder, session, block, low, high, NullPlacement::NoNulls)? {
            push_range(&mut ranges, span.start, span.end);
        }
        block_start += max_rows;
    }
    Ok(ranges)
}

/// Rows whose value lies in `[low, high]` for a column whose root is a sorted
/// dictionary: the value range becomes a code range by two searches of the
/// table, and only the codes are scanned.
pub fn dictionary_range(
    decoder: &DecoderIr,
    session: &mut ReadSession<'_>,
    rows: Span,
    low: i64,
    high: i64,
) -> Result<Vec<Span>, String> {
    let &DecoderNode::Dictionary {
        ids,
        dictionary,
        entries,
        sorted_unique: true,
    } = decoder.node(decoder.root())?
    else {
        return Err("dictionary range needs a sorted dictionary root".into());
    };
    if low > high {
        return Err("vector dictionary range bounds are invalid".into());
    }
    if rows.end > decoder.len()? {
        return Err("vector scan exceeds the column".into());
    }
    let low_id = table_bound(session, dictionary, entries, low, false)?;
    let high_id = table_bound(session, dictionary, entries, high, true)?;
    let mut ranges = Vec::new();
    if low_id < high_id {
        let (low_id, last_id) = (low_id as i128, high_id as i128 - 1);
        scan_node(
            decoder,
            ids,
            session,
            rows.start,
            rows.end,
            &mut |_, row, chunk| {
                match chunk {
                    Chunk::Values(codes) => {
                        if codes.iter().any(|&id| !(0..entries as i128).contains(&id)) {
                            return Err("dictionary id is outside table".into());
                        }
                        push_batch_matches(&mut ranges, codes, row, low_id, last_id);
                    }
                    Chunk::Run { value, len } => {
                        if !(0..entries as i128).contains(&value) {
                            return Err("dictionary id is outside table".into());
                        }
                        if low_id <= value && value <= last_id {
                            push_range(&mut ranges, row, row + len);
                        }
                    }
                    Chunk::Nulls { .. } => {}
                }
                Ok(())
            },
        )?;
    }
    Ok(ranges)
}

fn scan_node<'p>(
    decoder: &DecoderIr,
    node: NodeId,
    session: &mut ReadSession<'p>,
    start: usize,
    end: usize,
    sink: &mut Sink<'_, 'p>,
) -> Result<(), String> {
    if start >= end {
        return Ok(());
    }
    match *decoder.node(node)? {
        DecoderNode::BitUnpack {
            stream,
            width,
            len,
            miniblock_rows,
            miniblock_bytes,
        } => {
            if end > len {
                return Err("bit-unpack scan exceeds its stream".into());
            }
            let rows = Span::new(start, end)?;
            session.touch_bits(stream, rows, width, miniblock_rows, miniblock_bytes)?;
            let mut buffer = session.take_batch_buffer();
            let mut row = start;
            while row < end {
                let chunk_end = (row + BATCH_ROWS).min(end);
                session.unpack_bits_into(
                    stream,
                    Span::new(row, chunk_end)?,
                    width,
                    miniblock_rows,
                    miniblock_bytes,
                    &mut buffer,
                )?;
                sink(session, row, Chunk::Values(&mut buffer))?;
                row = chunk_end;
            }
            session.restore_batch_buffer(buffer);
            Ok(())
        }
        DecoderNode::For { base, values } => {
            let base = i128::from(session.read_i64(base, 0)?);
            scan_node(
                decoder,
                values,
                session,
                start,
                end,
                &mut |session, row, chunk| {
                    let chunk = match chunk {
                        Chunk::Values(values) => {
                            let mut overflow = false;
                            for value in values.iter_mut() {
                                let (next, carried) = value.overflowing_add(base);
                                *value = next;
                                overflow |= carried;
                            }
                            if overflow {
                                return Err("FOR span overflow".into());
                            }
                            Chunk::Values(values)
                        }
                        Chunk::Run { value, len } => Chunk::Run {
                            value: value.checked_add(base).ok_or("FOR run overflow")?,
                            len,
                        },
                        nulls => nulls,
                    };
                    sink(session, row, chunk)
                },
            )
        }
        DecoderNode::Delta {
            deltas,
            restarts,
            restart_interval,
            len,
            coding,
        } => {
            if end > len {
                return Err("delta scan exceeds its column".into());
            }
            let mut block_start = start / restart_interval * restart_interval;
            let mut expanded = Vec::new();
            while block_start < end {
                let block_end = (block_start + restart_interval).min(end);
                let emit_start = start.max(block_start);
                let mut value =
                    i128::from(session.read_i64(restarts, block_start / restart_interval)?);
                if block_start >= emit_start {
                    sink(session, block_start, Chunk::Values(&mut [value]))?;
                }
                if block_start + 1 < block_end {
                    scan_node(
                        decoder,
                        deltas,
                        session,
                        block_start + 1,
                        block_end,
                        &mut |session, row, chunk| {
                            let codes: &mut [i128] = match chunk {
                                Chunk::Values(codes) => codes,
                                Chunk::Run { value: code, len } => {
                                    expanded.clear();
                                    expanded.resize(len, code);
                                    &mut expanded
                                }
                                Chunk::Nulls { .. } => return Err("delta code is null".into()),
                            };
                            let mut overflow = false;
                            for slot in codes.iter_mut() {
                                let delta = delta_value(*slot, coding)?;
                                let (next, carried) = value.overflowing_add(delta);
                                value = next;
                                overflow |= carried;
                                *slot = value;
                            }
                            if overflow {
                                return Err("delta span overflow".into());
                            }
                            let skip = emit_start.saturating_sub(row).min(codes.len());
                            if skip < codes.len() {
                                sink(session, row + skip, Chunk::Values(&mut codes[skip..]))?;
                            }
                            Ok(())
                        },
                    )?;
                }
                block_start += restart_interval;
            }
            Ok(())
        }
        DecoderNode::Dictionary {
            ids,
            dictionary,
            entries,
            ..
        } => scan_node(
            decoder,
            ids,
            session,
            start,
            end,
            &mut |session, row, chunk| {
                let chunk = match chunk {
                    Chunk::Values(ids) => {
                        session.gather_i64_into(dictionary, entries, ids)?;
                        Chunk::Values(ids)
                    }
                    Chunk::Run { value: id, len } => Chunk::Run {
                        value: table_entry(session, dictionary, entries, id)?,
                        len,
                    },
                    nulls => nulls,
                };
                sink(session, row, chunk)
            },
        ),
        DecoderNode::Rle {
            values,
            run_lengths,
            run_index,
            len,
            runs,
            index_interval,
        } => {
            if end > len {
                return Err("run-length scan exceeds its column".into());
            }
            let (mut run, mut run_start, _) =
                locate_indexed(session, run_index, run_lengths, runs, index_interval, start)?;
            while run < runs && run_start < end {
                let run_end = run_start
                    .checked_add(session.read_u32(run_lengths, run)? as usize)
                    .ok_or("run-length row overflow")?;
                let span_start = start.max(run_start);
                let span_end = end.min(run_end);
                if span_start < span_end {
                    let len = span_end - span_start;
                    let chunk = match get_node(decoder, values, session, run)? {
                        Some(value) => Chunk::Run { value, len },
                        None => Chunk::Nulls { len },
                    };
                    sink(session, span_start, chunk)?;
                }
                run_start = run_end;
                run += 1;
            }
            if run_start < end {
                return Err("run lengths end before the column does".into());
            }
            Ok(())
        }
        DecoderNode::Patch {
            values,
            positions,
            position_index,
            exceptions,
            count,
            index_interval,
        } => {
            let mut exception = lower_bound_position(
                session,
                position_index,
                positions,
                count,
                index_interval,
                start,
            )?;
            let mut patches: Vec<(usize, i128)> = Vec::new();
            while exception < count {
                let row = session.read_u32(positions, exception)? as usize;
                if row >= end {
                    break;
                }
                if patches.last().is_some_and(|&(previous, _)| row <= previous) {
                    return Err("patch positions are not ordered".into());
                }
                patches.push((row, i128::from(session.read_i64(exceptions, exception)?)));
                exception += 1;
            }
            let mut next = 0;
            scan_node(
                decoder,
                values,
                session,
                start,
                end,
                &mut |session, row, chunk| {
                    let chunk_end = row + chunk.len();
                    match chunk {
                        Chunk::Values(values) => {
                            while next < patches.len() && patches[next].0 < chunk_end {
                                let (patched, value) = patches[next];
                                values[patched - row] = value;
                                next += 1;
                            }
                            sink(session, row, Chunk::Values(values))
                        }
                        Chunk::Run { value, .. } => {
                            let mut cursor = row;
                            while next < patches.len() && patches[next].0 < chunk_end {
                                let (patched, exception) = patches[next];
                                if cursor < patched {
                                    sink(
                                        session,
                                        cursor,
                                        Chunk::Run {
                                            value,
                                            len: patched - cursor,
                                        },
                                    )?;
                                }
                                sink(session, patched, Chunk::Values(&mut [exception]))?;
                                cursor = patched + 1;
                                next += 1;
                            }
                            if cursor < chunk_end {
                                sink(
                                    session,
                                    cursor,
                                    Chunk::Run {
                                        value,
                                        len: chunk_end - cursor,
                                    },
                                )?;
                            }
                            Ok(())
                        }
                        nulls => sink(session, row, nulls),
                    }
                },
            )
        }
        DecoderNode::Nullable {
            validity,
            rank_index,
            values,
            logical_len,
            rank_interval,
            ..
        } => {
            if end > logical_len {
                return Err("nullable scan exceeds its column".into());
            }
            let mut compact =
                nullable_prefix_rank(session, validity, rank_index, start, rank_interval)?;
            let first_byte = start / 8;
            let bits = session
                .read_range(validity, Span::new(first_byte, (end - 1) / 8 + 1)?)?
                .to_vec();
            let valid = |row: usize| bits[row / 8 - first_byte] >> (row % 8) & 1 == 1;
            let mut logical = start;
            while logical < end {
                let state = valid(logical);
                let run_start = logical;
                logical += 1;
                while logical < end && valid(logical) == state {
                    logical += 1;
                }
                if state {
                    let child_start = compact;
                    compact += logical - run_start;
                    scan_node(
                        decoder,
                        values,
                        session,
                        child_start,
                        compact,
                        &mut |session, row, chunk| {
                            sink(session, run_start + row - child_start, chunk)
                        },
                    )?;
                } else {
                    sink(
                        session,
                        run_start,
                        Chunk::Nulls {
                            len: logical - run_start,
                        },
                    )?;
                }
            }
            Ok(())
        }
    }
}

fn get_node(
    decoder: &DecoderIr,
    node: NodeId,
    session: &mut ReadSession<'_>,
    row: usize,
) -> Result<Option<i128>, String> {
    match *decoder.node(node)? {
        DecoderNode::BitUnpack {
            stream,
            width,
            len,
            miniblock_rows,
            miniblock_bytes,
        } => {
            if row >= len {
                return Err("bit-unpack GET exceeds its stream".into());
            }
            Ok(Some(i128::from(session.read_bits(
                stream,
                row,
                width,
                miniblock_rows,
                miniblock_bytes,
            )?)))
        }
        DecoderNode::For { base, values } => {
            let base = i128::from(session.read_i64(base, 0)?);
            get_node(decoder, values, session, row)?
                .map(|value| value.checked_add(base).ok_or_else(|| "FOR overflow".into()))
                .transpose()
        }
        DecoderNode::Delta {
            deltas,
            restarts,
            restart_interval,
            coding,
            ..
        } => {
            let block_start = row / restart_interval * restart_interval;
            let mut value = i128::from(session.read_i64(restarts, row / restart_interval)?);
            if block_start < row {
                scan_node(
                    decoder,
                    deltas,
                    session,
                    block_start + 1,
                    row + 1,
                    &mut |_, _, chunk| {
                        match chunk {
                            Chunk::Values(codes) => {
                                for &code in codes.iter() {
                                    value = value
                                        .checked_add(delta_value(code, coding)?)
                                        .ok_or("delta overflow")?;
                                }
                            }
                            Chunk::Run { value: code, len } => {
                                value = delta_value(code, coding)?
                                    .checked_mul(len as i128)
                                    .and_then(|step| value.checked_add(step))
                                    .ok_or("delta overflow")?;
                            }
                            Chunk::Nulls { .. } => return Err("delta code is null".into()),
                        }
                        Ok(())
                    },
                )?;
            }
            Ok(Some(value))
        }
        DecoderNode::Dictionary {
            ids,
            dictionary,
            entries,
            ..
        } => {
            let id = get_node(decoder, ids, session, row)?.ok_or("dictionary id is null")?;
            Ok(Some(table_entry(session, dictionary, entries, id)?))
        }
        DecoderNode::Rle {
            values,
            run_lengths,
            run_index,
            runs,
            index_interval,
            ..
        } => {
            let (run, _, _) =
                locate_indexed(session, run_index, run_lengths, runs, index_interval, row)?;
            get_node(decoder, values, session, run)
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
            position_index,
            positions,
            count,
            index_interval,
            row,
        )? {
            Some(exception) => Ok(Some(i128::from(session.read_i64(exceptions, exception)?))),
            None => get_node(decoder, values, session, row),
        },
        DecoderNode::Nullable {
            validity,
            rank_index,
            values,
            rank_interval,
            ..
        } => match nullable_rank(session, validity, rank_index, row, rank_interval)? {
            Some(compact) => get_node(decoder, values, session, compact),
            None => Ok(None),
        },
    }
}

/// First row in `rows` whose value is not before `bound`: not below it, or
/// not at or below it when `inclusive`.
fn boundary(
    decoder: &DecoderIr,
    session: &mut ReadSession<'_>,
    rows: Span,
    bound: i128,
    inclusive: bool,
    nulls: NullPlacement,
) -> Result<usize, String> {
    let (mut left, mut right) = (rows.start, rows.end);
    while left < right {
        let mid = left + (right - left) / 2;
        let before = match (nulls, get(decoder, session, mid)?) {
            (NullPlacement::NoNulls, None) => {
                return Err("dense monotone invariant encountered null".into());
            }
            (NullPlacement::First, None) => true,
            (NullPlacement::Last, None) => false,
            (NullPlacement::Arbitrary, _) => {
                return Err("arbitrary nulls are not monotone-searchable".into());
            }
            (_, Some(value)) => {
                if inclusive {
                    value <= bound
                } else {
                    value < bound
                }
            }
        };
        if before {
            left = mid + 1;
        } else {
            right = mid;
        }
    }
    Ok(left)
}

/// Number of table entries below `bound` (or at or below it when
/// `inclusive`), by binary search of a sorted table.
fn table_bound(
    session: &mut ReadSession<'_>,
    table: FieldId,
    entries: usize,
    bound: i64,
    inclusive: bool,
) -> Result<usize, String> {
    let (mut left, mut right) = (0, entries);
    while left < right {
        let mid = left + (right - left) / 2;
        let entry = session.read_i64(table, mid)?;
        if entry < bound || (inclusive && entry == bound) {
            left = mid + 1;
        } else {
            right = mid;
        }
    }
    Ok(left)
}

fn table_entry(
    session: &mut ReadSession<'_>,
    table: FieldId,
    entries: usize,
    id: i128,
) -> Result<i128, String> {
    let id = usize::try_from(id).map_err(|_| "dictionary id does not fit usize")?;
    if id >= entries {
        return Err("dictionary id is outside table".into());
    }
    Ok(i128::from(session.read_i64(table, id)?))
}

fn delta_value(code: i128, coding: DeltaCoding) -> Result<i128, String> {
    let code = u64::try_from(code).map_err(|_| "delta code does not fit u64")?;
    Ok(match coding {
        DeltaCoding::ZigZag => i128::from(unzigzag(code)),
        DeltaCoding::Unsigned => i128::from(code),
    })
}

fn add_chunk(total: i128, chunk: &Chunk<'_>) -> Result<i128, String> {
    match chunk {
        Chunk::Values(values) => sum_batch(total, values),
        Chunk::Run { value, len } => value
            .checked_mul(*len as i128)
            .and_then(|run| total.checked_add(run))
            .ok_or_else(|| "vector SUM overflow".into()),
        Chunk::Nulls { .. } => Ok(total),
    }
}

fn validate_ranges(decoder: &DecoderIr, ranges: &[Span]) -> Result<(), String> {
    let len = decoder.len()?;
    let mut previous = 0;
    for range in ranges {
        if range.start < previous || range.end > len {
            return Err("vector selection ranges are invalid".into());
        }
        previous = range.end;
    }
    Ok(())
}
