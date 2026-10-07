use super::{FieldId, ReadSession, SerializedPage, Span};

pub fn load_metadata(page: &SerializedPage, session: &mut ReadSession<'_>) -> Result<(), String> {
    let metadata = &page.layout().fields[page.layout().metadata.0];
    session
        .read_range(metadata.id, Span::new(0, metadata.length)?)
        .map(|_| ())
}

pub fn locate_indexed(
    session: &mut ReadSession<'_>,
    index_field: FieldId,
    lengths_field: FieldId,
    runs: usize,
    interval: usize,
    target_row: usize,
) -> Result<(usize, usize, usize), String> {
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
    let mut row_start = session.read_u32(index_field, low * 2)? as usize;
    let mut run = session.read_u32(index_field, low * 2 + 1)? as usize;
    while run < runs {
        let row_end = row_start
            .checked_add(session.read_u32(lengths_field, run)? as usize)
            .ok_or("RLE row overflow")?;
        if target_row < row_end {
            return Ok((run, row_start, row_end));
        }
        row_start = row_end;
        run += 1;
    }
    Err("RLE index did not cover target row".into())
}

pub fn find_position(
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

pub fn lower_bound_position(
    session: &mut ReadSession<'_>,
    index_field: FieldId,
    positions_field: FieldId,
    count: usize,
    interval: usize,
    target: usize,
) -> Result<usize, String> {
    if count == 0 {
        return Ok(0);
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
    let mut exception = session.read_u32(index_field, low * 2 + 1)? as usize;
    while exception < count && (session.read_u32(positions_field, exception)? as usize) < target {
        exception += 1;
    }
    Ok(exception)
}

pub fn nullable_rank(
    session: &mut ReadSession<'_>,
    validity: FieldId,
    rank_index: FieldId,
    row: usize,
    rank_interval: usize,
) -> Result<Option<usize>, String> {
    let byte = session.read_u8(validity, row / 8)?;
    if byte >> (row % 8) & 1 == 0 {
        return Ok(None);
    }
    let block = row / rank_interval;
    let mut rank = session.read_u32(rank_index, block)? as usize;
    let block_start = block * rank_interval;
    for validity_byte in block_start / 8..row / 8 {
        rank += session.read_u8(validity, validity_byte)?.count_ones() as usize;
    }
    let prior_mask = (1_u16 << (row % 8)) - 1;
    rank += (u16::from(byte) & prior_mask).count_ones() as usize;
    Ok(Some(rank))
}

pub fn nullable_prefix_rank(
    session: &mut ReadSession<'_>,
    validity: FieldId,
    rank_index: FieldId,
    row: usize,
    rank_interval: usize,
) -> Result<usize, String> {
    let block = row / rank_interval;
    let mut rank = session.read_u32(rank_index, block)? as usize;
    let block_start = block * rank_interval;
    for validity_byte in block_start / 8..row / 8 {
        rank += session.read_u8(validity, validity_byte)?.count_ones() as usize;
    }
    if !row.is_multiple_of(8) {
        let byte = session.read_u8(validity, row / 8)?;
        rank += (u16::from(byte) & ((1_u16 << (row % 8)) - 1)).count_ones() as usize;
    }
    Ok(rank)
}

pub fn nullable_is_valid(
    session: &mut ReadSession<'_>,
    validity: FieldId,
    row: usize,
) -> Result<bool, String> {
    Ok(session.read_u8(validity, row / 8)? >> (row % 8) & 1 == 1)
}

pub fn push_range(ranges: &mut Vec<Span>, start: usize, end: usize) {
    if let Some(last) = ranges.last_mut()
        && last.end == start
    {
        last.end = end;
    } else {
        ranges.push(Span { start, end });
    }
}

/// Rows per batch in batch kernels: 16 KiB of decoded values, which stays
/// resident in L1 between the decode and the consuming loop.
pub const BATCH_ROWS: usize = 1_024;

/// Adds a batch of decoded values to `sum`. Any intermediate overflow fails,
/// so the outcome equals a chain of per-value `checked_add` calls.
pub fn sum_batch(sum: i128, values: &[i128]) -> Result<i128, String> {
    let mut total = sum;
    let mut overflow = false;
    for &value in values {
        let (next, carried) = total.overflowing_add(value);
        total = next;
        overflow |= carried;
    }
    if overflow {
        Err("batch SUM overflow".into())
    } else {
        Ok(total)
    }
}

/// Appends the rows of a batch whose values lie in `[low, high]`, producing
/// the same ranges as one `push_range` call per matching row.
pub fn push_batch_matches(
    ranges: &mut Vec<Span>,
    values: &[i128],
    first_row: usize,
    low: i128,
    high: i128,
) {
    let mut run_start = None;
    for (offset, &value) in values.iter().enumerate() {
        let hit = low <= value && value <= high;
        match (hit, run_start) {
            (true, None) => run_start = Some(offset),
            (false, Some(start)) => {
                push_range(ranges, first_row + start, first_row + offset);
                run_start = None;
            }
            _ => {}
        }
    }
    if let Some(start) = run_start {
        push_range(ranges, first_row + start, first_row + values.len());
    }
}

pub fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}
