use std::marker::PhantomData;

use super::{
    Answer, ClosureMode, EncodedColumn, Execution, FieldId, NullPlacement, ReadSession, Span,
    find_position, load_metadata, locate_indexed, lower_bound_position, nullable_is_valid,
    nullable_prefix_rank, nullable_rank, push_range, unzigzag,
};

pub trait StaticDecoder {
    fn get(session: &mut ReadSession<'_>, row: usize) -> Result<Option<i128>, String>;

    fn visit<F>(
        session: &mut ReadSession<'_>,
        start: usize,
        end: usize,
        visitor: &mut F,
    ) -> Result<(), String>
    where
        F: FnMut(&mut ReadSession<'_>, Option<i128>, usize, usize) -> Result<(), String>,
    {
        for row in start..end {
            let value = Self::get(session, row)?;
            visitor(session, value, row, row + 1)?;
        }
        Ok(())
    }
}

pub struct StaticBitPack<
    const FIELD: usize,
    const WIDTH: u8,
    const MINIBLOCK_ROWS: usize,
    const MINIBLOCK_BYTES: usize,
>;

impl<const FIELD: usize, const WIDTH: u8, const MINIBLOCK_ROWS: usize, const MINIBLOCK_BYTES: usize>
    StaticDecoder for StaticBitPack<FIELD, WIDTH, MINIBLOCK_ROWS, MINIBLOCK_BYTES>
{
    #[inline(always)]
    fn get(session: &mut ReadSession<'_>, row: usize) -> Result<Option<i128>, String> {
        Ok(Some(i128::from(session.read_bits(
            FieldId(FIELD),
            row,
            WIDTH,
            MINIBLOCK_ROWS,
            MINIBLOCK_BYTES,
        )?)))
    }
}

pub struct StaticFor<const BASE: usize, Child>(PhantomData<Child>);

impl<const BASE: usize, Child: StaticDecoder> StaticDecoder for StaticFor<BASE, Child> {
    #[inline(always)]
    fn get(session: &mut ReadSession<'_>, row: usize) -> Result<Option<i128>, String> {
        let base = i128::from(session.read_i64(FieldId(BASE), 0)?);
        Ok(Child::get(session, row)?
            .map(|value| value.checked_add(base).ok_or("static FOR overflow"))
            .transpose()?)
    }

    #[inline(always)]
    fn visit<F>(
        session: &mut ReadSession<'_>,
        start: usize,
        end: usize,
        visitor: &mut F,
    ) -> Result<(), String>
    where
        F: FnMut(&mut ReadSession<'_>, Option<i128>, usize, usize) -> Result<(), String>,
    {
        let base = i128::from(session.read_i64(FieldId(BASE), 0)?);
        Child::visit(
            session,
            start,
            end,
            &mut |session, value, span_start, span_end| {
                let value = value
                    .map(|value| value.checked_add(base).ok_or("static FOR span overflow"))
                    .transpose()?;
                visitor(session, value, span_start, span_end)
            },
        )
    }
}

pub struct StaticDelta<
    const RESTARTS: usize,
    const RESTART_INTERVAL: usize,
    const UNSIGNED: bool,
    Child,
>(PhantomData<Child>);

impl<
    const RESTARTS: usize,
    const RESTART_INTERVAL: usize,
    const UNSIGNED: bool,
    Child: StaticDecoder,
> StaticDecoder for StaticDelta<RESTARTS, RESTART_INTERVAL, UNSIGNED, Child>
{
    #[inline(always)]
    fn get(session: &mut ReadSession<'_>, row: usize) -> Result<Option<i128>, String> {
        let block = row / RESTART_INTERVAL;
        let block_start = block * RESTART_INTERVAL;
        let mut value = i128::from(session.read_i64(FieldId(RESTARTS), block)?);
        for position in block_start + 1..=row {
            let code = Child::get(session, position)?.ok_or("static delta code is null")?;
            let code = u64::try_from(code).map_err(|_| "static delta code does not fit u64")?;
            let delta = if UNSIGNED {
                i128::from(code)
            } else {
                i128::from(unzigzag(code))
            };
            value = value.checked_add(delta).ok_or("static delta overflow")?;
        }
        Ok(Some(value))
    }

    #[inline(always)]
    fn visit<F>(
        session: &mut ReadSession<'_>,
        start: usize,
        end: usize,
        visitor: &mut F,
    ) -> Result<(), String>
    where
        F: FnMut(&mut ReadSession<'_>, Option<i128>, usize, usize) -> Result<(), String>,
    {
        let mut block_start = start / RESTART_INTERVAL * RESTART_INTERVAL;
        while block_start < end {
            let block_end = (block_start + RESTART_INTERVAL).min(end);
            let emit_start = start.max(block_start);
            let mut value =
                i128::from(session.read_i64(FieldId(RESTARTS), block_start / RESTART_INTERVAL)?);
            for row in block_start..block_end {
                if row > block_start {
                    let code = Child::get(session, row)?.ok_or("static delta code is null")?;
                    let code =
                        u64::try_from(code).map_err(|_| "static delta code does not fit u64")?;
                    let delta = if UNSIGNED {
                        i128::from(code)
                    } else {
                        i128::from(unzigzag(code))
                    };
                    value = value
                        .checked_add(delta)
                        .ok_or("static delta span overflow")?;
                }
                if row >= emit_start {
                    visitor(session, Some(value), row, row + 1)?;
                }
            }
            block_start += RESTART_INTERVAL;
        }
        Ok(())
    }
}

pub struct StaticRle<
    const LENGTHS: usize,
    const INDEX: usize,
    const RUNS: usize,
    const INDEX_INTERVAL: usize,
    Child,
>(PhantomData<Child>);

impl<
    const LENGTHS: usize,
    const INDEX: usize,
    const RUNS: usize,
    const INDEX_INTERVAL: usize,
    Child: StaticDecoder,
> StaticDecoder for StaticRle<LENGTHS, INDEX, RUNS, INDEX_INTERVAL, Child>
{
    #[inline(always)]
    fn get(session: &mut ReadSession<'_>, row: usize) -> Result<Option<i128>, String> {
        let (run, _, _) = locate_indexed(
            session,
            FieldId(INDEX),
            FieldId(LENGTHS),
            RUNS,
            INDEX_INTERVAL,
            row,
        )?;
        Child::get(session, run)
    }

    #[inline(always)]
    fn visit<F>(
        session: &mut ReadSession<'_>,
        start: usize,
        end: usize,
        visitor: &mut F,
    ) -> Result<(), String>
    where
        F: FnMut(&mut ReadSession<'_>, Option<i128>, usize, usize) -> Result<(), String>,
    {
        if start == end {
            return Ok(());
        }
        let (mut run, mut run_start, _) = locate_indexed(
            session,
            FieldId(INDEX),
            FieldId(LENGTHS),
            RUNS,
            INDEX_INTERVAL,
            start,
        )?;
        while run < RUNS && run_start < end {
            let run_end = run_start
                .checked_add(session.read_u32(FieldId(LENGTHS), run)? as usize)
                .ok_or("static RLE row overflow")?;
            let span_start = start.max(run_start);
            let span_end = end.min(run_end);
            if span_start < span_end {
                let value = Child::get(session, run)?;
                visitor(session, value, span_start, span_end)?;
            }
            run_start = run_end;
            run += 1;
        }
        Ok(())
    }
}

pub struct StaticDictionary<const FIELD: usize, const ENTRIES: usize, Child>(PhantomData<Child>);

impl<const FIELD: usize, const ENTRIES: usize, Child: StaticDecoder> StaticDecoder
    for StaticDictionary<FIELD, ENTRIES, Child>
{
    #[inline(always)]
    fn get(session: &mut ReadSession<'_>, row: usize) -> Result<Option<i128>, String> {
        let id = usize::try_from(Child::get(session, row)?.ok_or("static dictionary id is null")?)
            .map_err(|_| "static dictionary id does not fit usize")?;
        if id >= ENTRIES {
            return Err("static dictionary id outside table".into());
        }
        Ok(Some(i128::from(session.read_i64(FieldId(FIELD), id)?)))
    }

    #[inline(always)]
    fn visit<F>(
        session: &mut ReadSession<'_>,
        start: usize,
        end: usize,
        visitor: &mut F,
    ) -> Result<(), String>
    where
        F: FnMut(&mut ReadSession<'_>, Option<i128>, usize, usize) -> Result<(), String>,
    {
        Child::visit(
            session,
            start,
            end,
            &mut |session, id, span_start, span_end| {
                let id = usize::try_from(id.ok_or("static dictionary id is null")?)
                    .map_err(|_| "static dictionary id does not fit usize")?;
                if id >= ENTRIES {
                    return Err("static dictionary id outside table".into());
                }
                let value = Some(i128::from(session.read_i64(FieldId(FIELD), id)?));
                visitor(session, value, span_start, span_end)
            },
        )
    }
}

pub struct StaticPatch<
    const POSITIONS: usize,
    const INDEX: usize,
    const EXCEPTIONS: usize,
    const COUNT: usize,
    const INDEX_INTERVAL: usize,
    Child,
>(PhantomData<Child>);

impl<
    const POSITIONS: usize,
    const INDEX: usize,
    const EXCEPTIONS: usize,
    const COUNT: usize,
    const INDEX_INTERVAL: usize,
    Child: StaticDecoder,
> StaticDecoder for StaticPatch<POSITIONS, INDEX, EXCEPTIONS, COUNT, INDEX_INTERVAL, Child>
{
    #[inline(always)]
    fn get(session: &mut ReadSession<'_>, row: usize) -> Result<Option<i128>, String> {
        match find_position(
            session,
            FieldId(INDEX),
            FieldId(POSITIONS),
            COUNT,
            INDEX_INTERVAL,
            row,
        )? {
            Some(exception) => Ok(Some(i128::from(
                session.read_i64(FieldId(EXCEPTIONS), exception)?,
            ))),
            None => Child::get(session, row),
        }
    }

    #[inline(always)]
    fn visit<F>(
        session: &mut ReadSession<'_>,
        start: usize,
        end: usize,
        visitor: &mut F,
    ) -> Result<(), String>
    where
        F: FnMut(&mut ReadSession<'_>, Option<i128>, usize, usize) -> Result<(), String>,
    {
        let mut exception = lower_bound_position(
            session,
            FieldId(INDEX),
            FieldId(POSITIONS),
            COUNT,
            INDEX_INTERVAL,
            start,
        )?;
        let mut patches = Vec::new();
        while exception < COUNT {
            let row = session.read_u32(FieldId(POSITIONS), exception)? as usize;
            if row >= end {
                break;
            }
            patches.push((
                row,
                i128::from(session.read_i64(FieldId(EXCEPTIONS), exception)?),
            ));
            exception += 1;
        }
        let mut patch = 0;
        Child::visit(
            session,
            start,
            end,
            &mut |session, value, span_start, span_end| {
                let mut cursor = span_start;
                while patch < patches.len() && patches[patch].0 < span_end {
                    let (row, exception) = patches[patch];
                    if row < cursor {
                        return Err("static patch positions are not ordered".into());
                    }
                    if cursor < row {
                        visitor(session, value, cursor, row)?;
                    }
                    visitor(session, Some(exception), row, row + 1)?;
                    cursor = row + 1;
                    patch += 1;
                }
                if cursor < span_end {
                    visitor(session, value, cursor, span_end)?;
                }
                Ok(())
            },
        )
    }
}

pub struct StaticNullable<
    const VALIDITY: usize,
    const RANK_INDEX: usize,
    const RANK_INTERVAL: usize,
    Child,
>(PhantomData<Child>);

impl<
    const VALIDITY: usize,
    const RANK_INDEX: usize,
    const RANK_INTERVAL: usize,
    Child: StaticDecoder,
> StaticDecoder for StaticNullable<VALIDITY, RANK_INDEX, RANK_INTERVAL, Child>
{
    #[inline(always)]
    fn get(session: &mut ReadSession<'_>, row: usize) -> Result<Option<i128>, String> {
        match nullable_rank(
            session,
            FieldId(VALIDITY),
            FieldId(RANK_INDEX),
            row,
            RANK_INTERVAL,
        )? {
            Some(compact_row) => Child::get(session, compact_row),
            None => Ok(None),
        }
    }

    #[inline(always)]
    fn visit<F>(
        session: &mut ReadSession<'_>,
        start: usize,
        end: usize,
        visitor: &mut F,
    ) -> Result<(), String>
    where
        F: FnMut(&mut ReadSession<'_>, Option<i128>, usize, usize) -> Result<(), String>,
    {
        let mut logical = start;
        let mut compact = nullable_prefix_rank(
            session,
            FieldId(VALIDITY),
            FieldId(RANK_INDEX),
            start,
            RANK_INTERVAL,
        )?;
        while logical < end {
            let valid = nullable_is_valid(session, FieldId(VALIDITY), logical)?;
            let run_start = logical;
            logical += 1;
            while logical < end && nullable_is_valid(session, FieldId(VALIDITY), logical)? == valid
            {
                logical += 1;
            }
            if valid {
                let child_start = compact;
                compact += logical - run_start;
                Child::visit(
                    session,
                    child_start,
                    compact,
                    &mut |session, value, span_start, span_end| {
                        let mapped_start = run_start + span_start - child_start;
                        visitor(
                            session,
                            value,
                            mapped_start,
                            mapped_start + span_end - span_start,
                        )
                    },
                )?;
            } else {
                visitor(session, None, run_start, logical)?;
            }
        }
        Ok(())
    }
}

pub fn static_get<D: StaticDecoder>(
    column: &EncodedColumn,
    row: usize,
    mode: ClosureMode,
) -> Result<Execution, String> {
    if row >= column.truth.len() {
        return Err("static GET outside column".into());
    }
    let mut session = ReadSession::new(&column.page, mode);
    load_metadata(&column.page, &mut session)?;
    let value = D::get(&mut session, row)?
        .map(|value| i64::try_from(value).map_err(|_| "static GET value does not fit i64"))
        .transpose()?;
    Ok(Execution {
        answer: Answer::Value(value),
        metrics: session.metrics(),
        decoded_rows: 1,
    })
}

pub fn static_sum<D: StaticDecoder>(
    column: &EncodedColumn,
    rows: Span,
    mode: ClosureMode,
) -> Result<Execution, String> {
    validate_rows(column, rows)?;
    let mut session = ReadSession::new(&column.page, mode);
    load_metadata(&column.page, &mut session)?;
    let mut sum = 0_i128;
    let mut decoded_rows = 0;
    D::visit(
        &mut session,
        rows.start,
        rows.end,
        &mut |_session, value, start, end| {
            decoded_rows += end - start;
            if let Some(value) = value {
                sum = sum
                    .checked_add(
                        value
                            .checked_mul((end - start) as i128)
                            .ok_or("static SUM product overflow")?,
                    )
                    .ok_or("static SUM overflow")?;
            }
            Ok(())
        },
    )?;
    Ok(Execution {
        answer: Answer::Sum(sum),
        metrics: session.metrics(),
        decoded_rows,
    })
}

pub fn static_between<D: StaticDecoder>(
    column: &EncodedColumn,
    rows: Span,
    low: i64,
    high: i64,
    mode: ClosureMode,
) -> Result<Execution, String> {
    validate_rows(column, rows)?;
    if low > high {
        return Err("static BETWEEN bounds invalid".into());
    }
    let mut session = ReadSession::new(&column.page, mode);
    load_metadata(&column.page, &mut session)?;
    let mut ranges = Vec::new();
    let mut decoded_rows = 0;
    D::visit(
        &mut session,
        rows.start,
        rows.end,
        &mut |_session, value, start, end| {
            decoded_rows += end - start;
            if value.is_some_and(|value| i128::from(low) <= value && value <= i128::from(high)) {
                push_range(&mut ranges, start, end);
            }
            Ok(())
        },
    )?;
    Ok(Execution {
        answer: Answer::Ranges(ranges),
        metrics: session.metrics(),
        decoded_rows,
    })
}

pub fn static_monotone_filter<D: StaticDecoder>(
    column: &EncodedColumn,
    low: i64,
    high: i64,
    nulls: NullPlacement,
    mode: ClosureMode,
) -> Result<Execution, String> {
    if low > high || nulls == NullPlacement::Arbitrary {
        return Err("static monotone FILTER contract is invalid".into());
    }
    let mut session = ReadSession::new(&column.page, mode);
    load_metadata(&column.page, &mut session)?;
    let mut decoded_rows = 0;
    let mut left = 0;
    let mut right = column.truth.len();
    while left < right {
        let mid = left + (right - left) / 2;
        let value = D::get(&mut session, mid)?;
        decoded_rows += 1;
        if monotone_before(value, i128::from(low), false, nulls)? {
            left = mid + 1;
        } else {
            right = mid;
        }
    }
    let start = left;
    right = column.truth.len();
    while left < right {
        let mid = left + (right - left) / 2;
        let value = D::get(&mut session, mid)?;
        decoded_rows += 1;
        if monotone_before(value, i128::from(high), true, nulls)? {
            left = mid + 1;
        } else {
            right = mid;
        }
    }
    let ranges = if start < left {
        vec![Span::new(start, left)?]
    } else {
        Vec::new()
    };
    Ok(Execution {
        answer: Answer::Ranges(ranges),
        metrics: session.metrics(),
        decoded_rows,
    })
}

pub fn static_piecewise_monotone_filter<D: StaticDecoder>(
    column: &EncodedColumn,
    low: i64,
    high: i64,
    max_rows: usize,
    mode: ClosureMode,
) -> Result<Execution, String> {
    if low > high || max_rows == 0 {
        return Err("static piecewise FILTER arguments are invalid".into());
    }
    let mut session = ReadSession::new(&column.page, mode);
    load_metadata(&column.page, &mut session)?;
    let mut ranges = Vec::new();
    let mut decoded_rows = 0;
    let mut block_start = 0;
    while block_start < column.truth.len() {
        let block_end = (block_start + max_rows).min(column.truth.len());
        let start = piecewise_boundary::<D>(
            &mut session,
            block_start,
            block_end,
            low,
            false,
            &mut decoded_rows,
        )?;
        let end = piecewise_boundary::<D>(
            &mut session,
            start,
            block_end,
            high,
            true,
            &mut decoded_rows,
        )?;
        if start < end {
            push_range(&mut ranges, start, end);
        }
        block_start = block_end;
    }
    Ok(Execution {
        answer: Answer::Ranges(ranges),
        metrics: session.metrics(),
        decoded_rows,
    })
}

fn piecewise_boundary<D: StaticDecoder>(
    session: &mut ReadSession<'_>,
    mut left: usize,
    mut right: usize,
    bound: i64,
    inclusive: bool,
    decoded_rows: &mut usize,
) -> Result<usize, String> {
    while left < right {
        let mid = left + (right - left) / 2;
        let value = D::get(session, mid)?.ok_or("piecewise monotone block contained null")?;
        *decoded_rows += 1;
        if value < i128::from(bound) || inclusive && value == i128::from(bound) {
            left = mid + 1;
        } else {
            right = mid;
        }
    }
    Ok(left)
}

pub fn static_dictionary_filter<
    const DICTIONARY: usize,
    const ENTRIES: usize,
    Ids: StaticDecoder,
>(
    column: &EncodedColumn,
    low: i64,
    high: i64,
    mode: ClosureMode,
) -> Result<Execution, String> {
    if low > high {
        return Err("static dictionary FILTER bounds invalid".into());
    }
    let mut session = ReadSession::new(&column.page, mode);
    load_metadata(&column.page, &mut session)?;
    let mut left = 0;
    let mut right = ENTRIES;
    while left < right {
        let mid = left + (right - left) / 2;
        if session.read_i64(FieldId(DICTIONARY), mid)? < low {
            left = mid + 1;
        } else {
            right = mid;
        }
    }
    let low_id = left;
    right = ENTRIES;
    while left < right {
        let mid = left + (right - left) / 2;
        if session.read_i64(FieldId(DICTIONARY), mid)? <= high {
            left = mid + 1;
        } else {
            right = mid;
        }
    }
    let high_id = left;
    let mut ranges = Vec::new();
    let mut decoded_rows = 0;
    if low_id < high_id {
        Ids::visit(
            &mut session,
            0,
            column.truth.len(),
            &mut |_session, id, start, end| {
                decoded_rows += end - start;
                let id = usize::try_from(id.ok_or("static dictionary id is null")?)
                    .map_err(|_| "static dictionary id does not fit usize")?;
                if id >= ENTRIES {
                    return Err("static dictionary id outside table".into());
                }
                if low_id <= id && id < high_id {
                    push_range(&mut ranges, start, end);
                }
                Ok(())
            },
        )?;
    }
    Ok(Execution {
        answer: Answer::Ranges(ranges),
        metrics: session.metrics(),
        decoded_rows,
    })
}

fn monotone_before(
    value: Option<i128>,
    bound: i128,
    inclusive: bool,
    nulls: NullPlacement,
) -> Result<bool, String> {
    match (nulls, value) {
        (NullPlacement::NoNulls, None) => Err("dense monotone invariant encountered null".into()),
        (NullPlacement::First, None) => Ok(true),
        (NullPlacement::Last, None) => Ok(false),
        (NullPlacement::Arbitrary, _) => Err("arbitrary nulls are not monotone-searchable".into()),
        (_, Some(value)) => Ok(if inclusive {
            value <= bound
        } else {
            value < bound
        }),
    }
}

fn validate_rows(column: &EncodedColumn, rows: Span) -> Result<(), String> {
    if rows.start > rows.end || rows.end > column.truth.len() {
        Err("static query range invalid".into())
    } else {
        Ok(())
    }
}
