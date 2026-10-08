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
    validate_rows(decoder, rows)?;
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

/// A set of rows within `rows`, one bit per row: bit `r` stands for row
/// `rows.start + r`. A bitmap costs the same however scattered the rows are,
/// where a list of spans costs one entry per run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Selection {
    pub rows: Span,
    pub words: Vec<u64>,
}

impl Selection {
    pub fn empty(rows: Span) -> Self {
        Self {
            rows,
            words: vec![0; rows.len().div_ceil(64)],
        }
    }

    /// Every row of `rows`, set a word at a time.
    pub fn full(rows: Span) -> Self {
        let mut words = vec![!0_u64; rows.len().div_ceil(64)];
        if let Some(last) = words.last_mut()
            && !rows.len().is_multiple_of(64)
        {
            *last = (1 << (rows.len() % 64)) - 1;
        }
        Self { rows, words }
    }

    /// Selects every row in `start..end`, which must lie within `rows`.
    pub fn set_range(&mut self, start: usize, end: usize) {
        for row in start..end {
            let bit = row - self.rows.start;
            self.words[bit / 64] |= 1 << (bit % 64);
        }
    }

    pub fn contains(&self, row: usize) -> bool {
        let bit = row - self.rows.start;
        self.words[bit / 64] >> (bit % 64) & 1 == 1
    }

    pub fn count(&self) -> usize {
        self.words
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum()
    }

    /// Selected rows in `start..end`, a word at a time.
    pub fn count_range(&self, start: usize, end: usize) -> usize {
        if start >= end {
            return 0;
        }
        let (first, last) = (start - self.rows.start, end - self.rows.start);
        let (first_word, last_word) = (first / 64, (last - 1) / 64);
        let mut count = 0;
        for index in first_word..=last_word {
            let mut word = self.words[index];
            if index == first_word {
                word &= !0 << (first % 64);
            }
            let high = last - index * 64;
            if index == last_word && high < 64 {
                word &= (1 << high) - 1;
            }
            count += word.count_ones() as usize;
        }
        count
    }

    /// The selected rows as ordered, disjoint spans.
    pub fn ranges(&self) -> Vec<Span> {
        let mut ranges = Vec::new();
        for (index, &word) in self.words.iter().enumerate() {
            let mut bits = word;
            while bits != 0 {
                let low = bits.trailing_zeros() as usize;
                let run = (!(bits >> low)).trailing_zeros() as usize;
                let start = self.rows.start + index * 64 + low;
                push_range(&mut ranges, start, start + run);
                bits = if low + run >= 64 {
                    0
                } else {
                    bits & (!0 << (low + run))
                };
            }
        }
        ranges
    }

    /// How many disjoint spans `ranges` would return, without building them:
    /// one per selected bit whose predecessor is not selected.
    pub fn run_count(&self) -> usize {
        let mut carry = 0;
        let mut runs = 0;
        for &word in &self.words {
            runs += (word & !(word << 1 | carry)).count_ones() as usize;
            carry = word >> 63;
        }
        runs
    }

    /// The span from the first selected row through the last, if any.
    pub fn hull(&self) -> Option<Span> {
        let first = self.words.iter().position(|word| *word != 0)?;
        let last = self.words.iter().rposition(|word| *word != 0)?;
        let start = first * 64 + self.words[first].trailing_zeros() as usize;
        let end = last * 64 + 64 - self.words[last].leading_zeros() as usize;
        Some(Span {
            start: self.rows.start + start,
            end: self.rows.start + end,
        })
    }
}

/// Rows of `rows` whose value lies in `[low, high]`, as a selection.
pub fn between_selection(
    decoder: &DecoderIr,
    session: &mut ReadSession<'_>,
    rows: Span,
    low: i64,
    high: i64,
) -> Result<Selection, String> {
    if low > high {
        return Err("vector BETWEEN bounds are invalid".into());
    }
    validate_rows(decoder, rows)?;
    let (low, high) = (i128::from(low), i128::from(high));
    let mut selection = Selection::empty(rows);
    // Bit-packed codes, under a frame-of-reference base or not and with or
    // without nulls, are compared against the predicate shifted by the base,
    // which is exact and skips adding the base to every value.
    if let Some(SelectorLane {
        lane,
        matcher,
        entries,
        ..
    }) = selector_lane(
        decoder,
        session,
        low as i64,
        high as i64,
        CodeCompare::Offset,
    )? {
        lane.select_with(session, rows, &matcher, entries, &mut selection)?;
        return Ok(selection);
    }
    scan(decoder, session, rows, &mut |_, row, chunk| {
        select_chunk(&mut selection, row, chunk, low, high);
        Ok(())
    })?;
    Ok(selection)
}

/// `dictionary_range` as a selection: codes are compared, never values.
pub fn dictionary_selection(
    decoder: &DecoderIr,
    session: &mut ReadSession<'_>,
    rows: Span,
    low: i64,
    high: i64,
) -> Result<Selection, String> {
    let &DecoderNode::Dictionary {
        ids,
        dictionary,
        entries,
        sorted_unique: true,
    } = decoder.node(decoder.root())?
    else {
        return Err("dictionary selection needs a sorted dictionary root".into());
    };
    if low > high {
        return Err("vector dictionary range bounds are invalid".into());
    }
    validate_rows(decoder, rows)?;
    let mut selection = Selection::empty(rows);
    let low_id = table_bound(session, dictionary, entries, low, false)?;
    let high_id = table_bound(session, dictionary, entries, high, true)?;
    if let Some(SelectorLane {
        lane,
        matcher,
        entries,
        ..
    }) = selector_lane(decoder, session, low, high, CodeCompare::SortedDictionary)?
    {
        // Ids are read even when none can match, to confirm that every one
        // lies inside the table, as a decode would.
        lane.select_with(session, rows, &matcher, entries, &mut selection)?;
        return Ok(selection);
    }
    if low_id < high_id {
        let (low_id, last_id) = (low_id as i128, high_id as i128 - 1);
        scan_node(
            decoder,
            ids,
            session,
            rows.start,
            rows.end,
            &mut |_, row, chunk| {
                let out_of_table = match &chunk {
                    Chunk::Values(codes) => {
                        codes.iter().any(|&id| !(0..entries as i128).contains(&id))
                    }
                    Chunk::Run { value, .. } => !(0..entries as i128).contains(value),
                    Chunk::Nulls { .. } => false,
                };
                if out_of_table {
                    return Err("dictionary id is outside table".into());
                }
                select_chunk(&mut selection, row, chunk, low_id, last_id);
                Ok(())
            },
        )?;
    }
    Ok(selection)
}

/// Exact SUM of the non-null values at selected rows, in one pass over the
/// span the selection covers. Each value is added under a mask rather than a
/// branch, so a scattered selection costs the same as a dense one.
pub fn sum_selection(
    decoder: &DecoderIr,
    session: &mut ReadSession<'_>,
    selection: &Selection,
) -> Result<i128, String> {
    validate_selection(decoder, selection)?;
    let Some(hull) = selection.hull() else {
        return Ok(0);
    };
    if let Some(lane) = Lane::summable(decoder, session)? {
        return lane.sum_selection(session, hull, selection);
    }
    let mut total = 0_i128;
    scan(decoder, session, hull, &mut |_, row, chunk| {
        match chunk {
            Chunk::Values(values) => {
                let mut overflow = false;
                for (index, &value) in values.iter().enumerate() {
                    let bit = row + index - selection.rows.start;
                    let take = (selection.words[bit / 64] >> (bit % 64) & 1) as i128;
                    let (next, carried) = total.overflowing_add(value & take.wrapping_neg());
                    total = next;
                    overflow |= carried;
                }
                if overflow {
                    return Err("vector selected SUM overflow".into());
                }
            }
            Chunk::Run { value, len } => {
                let count = selection.count_range(row, row + len) as i128;
                total = value
                    .checked_mul(count)
                    .and_then(|run| total.checked_add(run))
                    .ok_or("vector selected SUM overflow")?;
            }
            Chunk::Nulls { .. } => {}
        }
        Ok(())
    })?;
    Ok(total)
}

/// One conjunct of a fused conjunctive pass: a selector page, its session,
/// the predicate, and how its codes may be compared.
pub struct ConjunctPage<'a, 'p> {
    pub decoder: &'a DecoderIr,
    pub session: ReadSession<'p>,
    pub low: i64,
    pub high: i64,
    pub compare: CodeCompare,
}

/// A value page and its session, for a fused conjunctive aggregate.
pub struct ValuePage<'a, 'p> {
    pub decoder: &'a DecoderIr,
    pub session: ReadSession<'p>,
}

/// What a fused conjunctive pass computes over the rows every conjunct keeps.
pub enum FusedAggregate<'a, 'p> {
    Count,
    Sum(Box<ValuePage<'a, 'p>>),
    SumProduct(Box<[ValuePage<'a, 'p>; 2]>),
}

/// The aggregate and matching row count of a conjunction over `rows`, in one
/// pass a batch at a time: the first conjunct yields the positions it
/// matches, each later conjunct keeps the positions it also matches, and
/// values are read only at the survivors. Conjuncts run in order of their
/// estimated selectivity, read from their code ranges. `None` when a page is
/// not bit-packed codes in the form its conjunct or the aggregate needs, and
/// the caller evaluates the conjuncts one by one instead.
pub fn fused_conjunction(
    conjuncts: &mut [ConjunctPage<'_, '_>],
    aggregate: &mut FusedAggregate<'_, '_>,
    rows: Span,
) -> Result<Option<(i128, usize)>, String> {
    struct Test {
        lane: Lane,
        cursor: LaneCursor,
        matcher: Matcher,
        entries: Option<u64>,
        selectivity: f64,
    }
    let mut tests = Vec::with_capacity(conjuncts.len());
    for conjunct in conjuncts.iter_mut() {
        if conjunct.low > conjunct.high {
            return Err("vector BETWEEN bounds are invalid".into());
        }
        validate_rows(conjunct.decoder, rows)?;
        let Some(SelectorLane {
            lane,
            matcher,
            entries,
            domain,
        }) = selector_lane(
            conjunct.decoder,
            &mut conjunct.session,
            conjunct.low,
            conjunct.high,
            conjunct.compare,
        )?
        else {
            return Ok(None);
        };
        let cursor = lane.cursor(&mut conjunct.session, rows.start)?;
        let selectivity = matcher.selectivity(domain);
        tests.push(Test {
            lane,
            cursor,
            matcher,
            entries,
            selectivity,
        });
    }
    let mut values = Vec::new();
    let mut value_sessions: Vec<&mut ReadSession<'_>> = Vec::new();
    match aggregate {
        FusedAggregate::Count => {}
        FusedAggregate::Sum(page) => {
            validate_rows(page.decoder, rows)?;
            let Some(lane) = Lane::summable(page.decoder, &mut page.session)? else {
                return Ok(None);
            };
            values.push(lane);
            value_sessions.push(&mut page.session);
        }
        FusedAggregate::SumProduct(pair) => {
            let [left, right] = pair.as_mut();
            validate_rows(left.decoder, rows)?;
            validate_rows(right.decoder, rows)?;
            let (Some(a), Some(b)) = (
                Lane::summable(left.decoder, &mut left.session)?,
                Lane::summable(right.decoder, &mut right.session)?,
            ) else {
                return Ok(None);
            };
            values.extend([a, b]);
            value_sessions.push(&mut left.session);
            value_sessions.push(&mut right.session);
        }
    }
    let mut value_cursors = values
        .iter()
        .zip(value_sessions.iter_mut())
        .map(|(lane, session)| lane.cursor(session, rows.start))
        .collect::<Result<Vec<_>, _>>()?;
    let mut order = (0..tests.len()).collect::<Vec<_>>();
    order.sort_by(|&a, &b| tests[a].selectivity.total_cmp(&tests[b].selectivity));
    let mut batch = LaneBatch::default();
    let mut value_batches = (0..values.len())
        .map(|_| LaneBatch::default())
        .collect::<Vec<_>>();
    let mut gathered = vec![Vec::new(); values.len()];
    let mut positions = Vec::with_capacity(BATCH_ROWS);
    let (mut total, mut matched) = (0_i128, 0_usize);
    let mut row = rows.start;
    while row < rows.end {
        let end = (row + BATCH_ROWS).min(rows.end);
        let span = Span::new(row, end)?;
        positions.clear();
        positions.extend(0..span.len() as u32);
        // With no conjunct, every row is alive.
        let mut alive = if order.is_empty() { span.len() } else { 0 };
        for (rank, &slot) in order.iter().enumerate() {
            let test = &mut tests[slot];
            let session = &mut conjuncts[slot].session;
            // Lanes advance batch by batch; one whose rows are no longer
            // needed is still filled, to keep its cursor in step and to
            // check dictionary ids as a decode would.
            test.lane
                .fill(session, &mut test.cursor, span, &mut batch)?;
            if let Some(entries) = test.entries
                && batch.outside(entries) != 0
            {
                return Err("dictionary id is outside table".into());
            }
            let matcher = &test.matcher;
            let hit = |code: u64| matcher.hit(code) as usize;
            let dense = batch.dense();
            if rank == 0 {
                for index in 0..span.len() {
                    let valid = dense || batch.valid[index] != 0;
                    positions[alive] = index as u32;
                    alive += hit(batch.codes[index]) & usize::from(valid);
                }
            } else {
                let mut kept = 0;
                for cursor in 0..alive {
                    let index = positions[cursor] as usize;
                    let valid = dense || batch.valid[index] != 0;
                    positions[kept] = index as u32;
                    kept += hit(batch.codes[index]) & usize::from(valid);
                }
                alive = kept;
            }
        }
        matched += alive;
        for ((lane, session), (cursor, (values_batch, values_out))) in
            values.iter().zip(value_sessions.iter_mut()).zip(
                value_cursors
                    .iter_mut()
                    .zip(value_batches.iter_mut().zip(gathered.iter_mut())),
            )
        {
            lane.fill(session, cursor, span, values_batch)?;
            lane.gather_at(values_batch, &positions[..alive], values_out)?;
        }
        if alive > 0 {
            total = match gathered.as_slice() {
                [] => total,
                [single] => {
                    let mut sum = 0_i128;
                    for value in single.iter().flatten() {
                        sum += value;
                    }
                    total.checked_add(sum).ok_or("vector SUM overflow")?
                }
                [left, right] => {
                    let mut sum = 0_i128;
                    for (x, y) in left.iter().zip(right) {
                        if let (Some(x), Some(y)) = (x, y) {
                            sum = x
                                .checked_mul(*y)
                                .and_then(|product| sum.checked_add(product))
                                .ok_or("vector product SUM overflow")?;
                        }
                    }
                    total
                        .checked_add(sum)
                        .ok_or("vector product SUM overflow")?
                }
                _ => return Err("fused aggregate reads at most two columns".into()),
            };
        }
        row = end;
    }
    if matches!(aggregate, FusedAggregate::Count) {
        total = matched as i128;
    }
    Ok(Some((total, matched)))
}

/// Exact SUM of `a * b` over the selected rows at which both values are
/// non-null, for two columns over the same rows. Pages of codes run on
/// lanes, a batch at a time; others are decoded over the selection's hull.
pub fn sum_product_selection(
    a: &DecoderIr,
    a_session: &mut ReadSession<'_>,
    b: &DecoderIr,
    b_session: &mut ReadSession<'_>,
    selection: &Selection,
) -> Result<i128, String> {
    validate_selection(a, selection)?;
    validate_selection(b, selection)?;
    let Some(hull) = selection.hull() else {
        return Ok(0);
    };
    if let (Some(left), Some(right)) =
        (Lane::summable(a, a_session)?, Lane::summable(b, b_session)?)
    {
        let (mut left_cursor, mut right_cursor) = (
            left.cursor(a_session, hull.start)?,
            right.cursor(b_session, hull.start)?,
        );
        let (mut left_batch, mut right_batch) = (LaneBatch::default(), LaneBatch::default());
        let (mut left_values, mut right_values, mut take) = (Vec::new(), Vec::new(), Vec::new());
        let mut total = 0_i128;
        let mut row = hull.start;
        while row < hull.end {
            let end = (row + BATCH_ROWS).min(hull.end);
            let batch = Span::new(row, end)?;
            left.fill(a_session, &mut left_cursor, batch, &mut left_batch)?;
            right.fill(b_session, &mut right_cursor, batch, &mut right_batch)?;
            left_batch.taken(selection, row, &mut take);
            left.values_into(&left_batch, &take, &mut left_values)?;
            right.values_into(&right_batch, &take, &mut right_values)?;
            for ((&x, &y), (&taken, (left_valid, right_valid))) in
                left_values.iter().zip(&right_values).zip(
                    take.iter()
                        .zip(left_batch.validity().zip(right_batch.validity())),
                )
            {
                let mask = i128::from(taken & left_valid & right_valid).wrapping_neg();
                let product = x.checked_mul(y).ok_or("vector product overflow")?;
                total = total
                    .checked_add(product & mask)
                    .ok_or("vector product SUM overflow")?;
            }
            row = end;
        }
        return Ok(total);
    }
    let decode = |decoder: &DecoderIr, session: &mut ReadSession<'_>| {
        let mut values = vec![None; hull.len()];
        scan(decoder, session, hull, &mut |_, start, chunk| {
            let offset = start - hull.start;
            match chunk {
                Chunk::Values(chunk) => {
                    for (index, &value) in chunk.iter().enumerate() {
                        values[offset + index] = Some(value);
                    }
                }
                Chunk::Run { value, len } => {
                    values[offset..offset + len].fill(Some(value));
                }
                Chunk::Nulls { .. } => {}
            }
            Ok(())
        })?;
        Ok::<_, String>(values)
    };
    let (left, right) = (decode(a, a_session)?, decode(b, b_session)?);
    let mut total = 0_i128;
    for (offset, (x, y)) in left.iter().zip(&right).enumerate() {
        if let (Some(x), Some(y)) = (x, y)
            && selection.contains(hull.start + offset)
        {
            total = x
                .checked_mul(*y)
                .and_then(|product| total.checked_add(product))
                .ok_or("vector product SUM overflow")?;
        }
    }
    Ok(total)
}

/// Rows of `rows` whose value, an id into a table of `hits.len()` entries,
/// indexes an entry marked true. Every value must be a valid id.
pub fn selection_by_table(
    decoder: &DecoderIr,
    session: &mut ReadSession<'_>,
    rows: Span,
    hits: &[bool],
) -> Result<Selection, String> {
    validate_rows(decoder, rows)?;
    let mut selection = Selection::empty(rows);
    let table = hits.iter().map(|&hit| u64::from(hit)).collect::<Vec<_>>();
    let mut outside = false;
    scan(decoder, session, rows, &mut |_, row, chunk| {
        let offset = row - rows.start;
        match chunk {
            Chunk::Values(values) => {
                for (index, &value) in values.iter().enumerate() {
                    let hit = usize::try_from(value).ok().and_then(|id| table.get(id));
                    outside |= hit.is_none();
                    let bit = offset + index;
                    selection.words[bit / 64] |= hit.copied().unwrap_or(0) << (bit % 64);
                }
            }
            Chunk::Run { value, len } => {
                match usize::try_from(value).ok().and_then(|id| table.get(id)) {
                    Some(1) => selection.set_range(row, row + len),
                    Some(_) => {}
                    None => outside = true,
                }
            }
            Chunk::Nulls { .. } => {}
        }
        Ok(())
    })?;
    if outside {
        return Err("id lies outside its table".into());
    }
    Ok(selection)
}

/// How a fused pass may compare a selector page's codes: as values offset
/// by a frame-of-reference base, or as ids into a sorted dictionary, which
/// only a translation licence permits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodeCompare {
    Offset,
    SortedDictionary,
}

/// SUM and count of `value` over the rows in `rows` whose `selector` value
/// lies in `[low, high] = bounds`, in one pass over both pages' codes: each
/// row is compared, masked and added without a branch, and no selection is
/// built. Either page may hold nulls. `None` when either page is not
/// bit-packed codes in the form `compare` names, and the caller decodes
/// instead.
pub fn fused_sum_between(
    selector: &DecoderIr,
    selector_session: &mut ReadSession<'_>,
    value: &DecoderIr,
    value_session: &mut ReadSession<'_>,
    rows: Span,
    (low, high): (i64, i64),
    compare: CodeCompare,
) -> Result<Option<(i128, usize)>, String> {
    if low > high {
        return Err("vector BETWEEN bounds are invalid".into());
    }
    validate_rows(selector, rows)?;
    validate_rows(value, rows)?;
    if rows.start >= rows.end {
        return Ok(Some((0, 0)));
    }
    let Some(sums) = Lane::summable(value, value_session)? else {
        return Ok(None);
    };
    let Some(SelectorLane {
        lane: codes,
        matcher,
        entries,
        ..
    }) = selector_lane(selector, selector_session, low, high, compare)?
    else {
        return Ok(None);
    };
    // With no code in range nothing matches, but dictionary ids are still
    // read to confirm they lie inside the table, as a decode would.
    if matches!(matcher, Matcher::Nothing) && entries.is_none() {
        return Ok(Some((0, 0)));
    }
    let mut selected = Vec::with_capacity(BATCH_ROWS.min(rows.len()));
    let mut value_batch = LaneBatch::default();
    let mut value_cursor = sums.cursor(value_session, rows.start)?;
    let mut total = 0_i128;
    let mut matched = 0_usize;
    let mut outside = 0_u64;
    let mut selector_cursor = codes.cursor(selector_session, rows.start)?;
    let mut selector_batch = LaneBatch::default();
    let mut row = rows.start;
    while row < rows.end {
        let end = (row + BATCH_ROWS).min(rows.end);
        let batch = Span::new(row, end)?;
        codes.fill(
            selector_session,
            &mut selector_cursor,
            batch,
            &mut selector_batch,
        )?;
        if let Some(entries) = entries {
            outside |= selector_batch.outside(entries);
        }
        if !matches!(matcher, Matcher::Nothing) {
            matcher.test_into(&selector_batch, &mut selected);
            sums.fill(value_session, &mut value_cursor, batch, &mut value_batch)?;
            let sum = sums.sum_taken(&value_batch, &selected)?;
            total = total.checked_add(sum).ok_or("vector SUM overflow")?;
            matched += selected.iter().sum::<u64>() as usize;
        }
        row = end;
    }
    if outside != 0 {
        return Err("dictionary id is outside table".into());
    }
    Ok(Some((total, matched)))
}

/// A node of bit-packed codes, optionally offset by a frame-of-reference
/// base: the shape most pages take. Its kernels work on 64-bit codes and add
/// the base once per batch rather than once per row. Codes at most
/// `SUMMABLE_WIDTH` bits wide sum without overflow over a batch.
struct Packed {
    base: i128,
    stream: FieldId,
    width: u8,
    len: usize,
    miniblock_rows: usize,
    miniblock_bytes: usize,
}

/// Runs whose lengths and values a run-length scan reads at once.
const RLE_BATCH_RUNS: usize = 256;

/// Widest code a batch of `BATCH_ROWS` codes sums in a `u64`.
const SUMMABLE_WIDTH: u8 = 53;

impl Packed {
    fn at_root(decoder: &DecoderIr, session: &mut ReadSession<'_>) -> Result<Option<Self>, String> {
        Self::at_offset(decoder, session, decoder.root())
    }

    /// Codes at `node`, under a frame-of-reference base or bare.
    fn at_offset(
        decoder: &DecoderIr,
        session: &mut ReadSession<'_>,
        node: NodeId,
    ) -> Result<Option<Self>, String> {
        let (base, codes) = match *decoder.node(node)? {
            DecoderNode::For { base, values } => (Some(base), values),
            _ => (None, node),
        };
        let Some(mut packed) = Self::at_node(decoder, codes)? else {
            return Ok(None);
        };
        if let Some(base) = base {
            packed.base = i128::from(session.read_i64(base, 0)?);
        }
        Ok(Some(packed))
    }

    /// Bare bit-packed codes at `node`, with no base.
    fn at_node(decoder: &DecoderIr, node: NodeId) -> Result<Option<Self>, String> {
        let DecoderNode::BitUnpack {
            stream,
            width,
            len,
            miniblock_rows,
            miniblock_bytes,
        } = *decoder.node(node)?
        else {
            return Ok(None);
        };
        Ok((width <= SUMMABLE_WIDTH).then_some(Self {
            base: 0,
            stream,
            width,
            len,
            miniblock_rows,
            miniblock_bytes,
        }))
    }

    /// The codes whose values lie in `[low, high]` once the base is removed,
    /// clipped to the codes the width can hold; `None` when there are none.
    fn code_range(&self, low: i128, high: i128) -> Option<(u64, u64)> {
        let largest = (1_i128 << self.width) - 1;
        let (first, last) = (low.max(0), high.min(largest));
        (first <= last).then_some((first as u64, last as u64))
    }

    /// Replaces `codes` with the codes of `rows`.
    fn unpack(
        &self,
        session: &mut ReadSession<'_>,
        rows: Span,
        codes: &mut Vec<u64>,
    ) -> Result<(), String> {
        if rows.end > self.len {
            return Err("bit-unpack scan exceeds its stream".into());
        }
        session.unpack_bits_into(
            self.stream,
            rows,
            self.width,
            self.miniblock_rows,
            self.miniblock_bytes,
            codes,
        )
    }

    /// Streams `rows` as batches of codes, each with the row it starts at.
    fn scan(
        &self,
        session: &mut ReadSession<'_>,
        rows: Span,
        mut sink: impl FnMut(usize, &[u64]) -> Result<(), String>,
    ) -> Result<(), String> {
        if rows.start >= rows.end {
            return Ok(());
        }
        session.touch_bits(
            self.stream,
            rows,
            self.width,
            self.miniblock_rows,
            self.miniblock_bytes,
        )?;
        let mut codes = Vec::with_capacity(BATCH_ROWS.min(rows.len()));
        let mut row = rows.start;
        while row < rows.end {
            let end = (row + BATCH_ROWS).min(rows.end);
            self.unpack(session, Span::new(row, end)?, &mut codes)?;
            sink(row, &codes)?;
            row = end;
        }
        Ok(())
    }

    /// `count` values whose codes sum to `codes`, added to `total`.
    fn add(&self, total: i128, count: usize, codes: u64) -> Result<i128, String> {
        self.base
            .checked_mul(count as i128)
            .and_then(|base| base.checked_add(i128::from(codes)))
            .and_then(|batch| total.checked_add(batch))
            .ok_or_else(|| "vector SUM overflow".into())
    }

    fn sum_ranges(&self, session: &mut ReadSession<'_>, ranges: &[Span]) -> Result<i128, String> {
        let mut total = 0_i128;
        for range in ranges {
            self.scan(session, *range, |_, codes| {
                total = self.add(total, codes.len(), codes.iter().sum())?;
                Ok(())
            })?;
        }
        Ok(total)
    }
}

/// Which codes a lane carries: values offset from a base, ids into a sorted
/// dictionary to compare, or ids into any dictionary to sum its entries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Codes {
    Offset,
    SortedDictionary,
    DictionaryValues,
}

/// Where a nullable page records which rows hold a value.
#[derive(Clone, Copy, Debug)]
struct Validity {
    bits: FieldId,
    rank_index: FieldId,
    rank_interval: usize,
}

/// Bit-packed codes per row, maybe behind a validity bitmap: a page stores
/// the codes of its non-null rows only, and a lane expands them to row
/// positions one batch at a time, so a page with scattered nulls costs one
/// unpack per batch rather than one per run of values.
struct Lane {
    packed: Packed,
    validity: Option<Validity>,
    /// The table the codes index, and its entries, for dictionary codes.
    dictionary: Option<(FieldId, usize)>,
    /// The table's entries, read once, for `Codes::DictionaryValues`.
    entries: Vec<i64>,
    /// Every entry is below 2^52 in magnitude, so a batch of them sums in
    /// an `i64`.
    narrow: bool,
}

/// Largest entry magnitude, in bits, that `Lane::narrow` admits.
const NARROW_BITS: u32 = 52;

/// One batch of a lane: a code per row, and, for a page with nulls, per row
/// 1 if it holds a value and 0 if it is null; a null row's code is
/// meaningless. A dense batch leaves `valid` empty, and every kernel picks
/// its loop once per batch rather than testing validity per row.
#[derive(Default)]
struct LaneBatch {
    codes: Vec<u64>,
    valid: Vec<u64>,
    compact: Vec<u64>,
}

impl LaneBatch {
    fn dense(&self) -> bool {
        self.valid.is_empty()
    }

    /// Replaces `out` with `test(code)` for each row, 0 at null rows.
    fn test_into(&self, out: &mut Vec<u64>, test: impl Fn(u64) -> u64) {
        out.clear();
        if self.dense() {
            out.extend(self.codes.iter().map(|&code| test(code)));
        } else {
            out.extend(
                self.codes
                    .iter()
                    .zip(&self.valid)
                    .map(|(&code, &valid)| test(code) & valid),
            );
        }
    }

    /// 1 or 0 per row: whether the row holds a value.
    fn validity(&self) -> impl Iterator<Item = u64> + '_ {
        let dense = self.dense();
        (0..self.codes.len()).map(move |index| if dense { 1 } else { self.valid[index] })
    }

    /// Nonzero if a non-null row's id lies outside a table of `entries`.
    fn outside(&self, entries: u64) -> u64 {
        if self.dense() {
            self.codes
                .iter()
                .fold(0, |bad, &id| bad | u64::from(id >= entries))
        } else {
            self.codes
                .iter()
                .zip(&self.valid)
                .fold(0, |bad, (&id, &valid)| {
                    bad | (u64::from(id >= entries) & valid)
                })
        }
    }

    /// The bits a selection takes from rows `row..`, as one 0 or 1 per row
    /// of the batch.
    fn taken(&self, selection: &Selection, row: usize, take: &mut Vec<u64>) {
        let offset = row - selection.rows.start;
        take.clear();
        take.extend((0..self.codes.len()).map(|index| {
            let bit = offset + index;
            selection.words[bit / 64] >> (bit % 64) & 1
        }));
    }
}

/// A lane's position: the next row's index among the stored codes.
struct LaneCursor {
    compact: usize,
}

impl Lane {
    fn at_root(
        decoder: &DecoderIr,
        session: &mut ReadSession<'_>,
        codes: Codes,
    ) -> Result<Option<Self>, String> {
        let mut node = decoder.root();
        let mut validity = None;
        if let DecoderNode::Nullable {
            validity: bits,
            rank_index,
            values,
            rank_interval,
            ..
        } = *decoder.node(node)?
        {
            validity = Some(Validity {
                bits,
                rank_index,
                rank_interval,
            });
            node = values;
        }
        let (packed, dictionary) = match codes {
            Codes::Offset => (Packed::at_offset(decoder, session, node)?, None),
            Codes::SortedDictionary | Codes::DictionaryValues => {
                let &DecoderNode::Dictionary {
                    ids,
                    dictionary,
                    entries,
                    sorted_unique,
                } = decoder.node(node)?
                else {
                    return Ok(None);
                };
                if codes == Codes::SortedDictionary && !sorted_unique {
                    return Ok(None);
                }
                (Packed::at_node(decoder, ids)?, Some((dictionary, entries)))
            }
        };
        let Some(packed) = packed else {
            return Ok(None);
        };
        let mut entries = Vec::new();
        if codes == Codes::DictionaryValues
            && let Some((table, count)) = dictionary
        {
            session.read_i64_table(table, count, &mut entries)?;
        }
        let narrow = entries
            .iter()
            .all(|entry| entry.unsigned_abs() < 1 << NARROW_BITS);
        Ok(Some(Self {
            packed,
            validity,
            dictionary,
            entries,
            narrow,
        }))
    }

    /// A lane whose values a SUM can add: offset codes or dictionary ids.
    fn summable(
        decoder: &DecoderIr,
        session: &mut ReadSession<'_>,
    ) -> Result<Option<Self>, String> {
        match Self::at_root(decoder, session, Codes::Offset)? {
            Some(lane) => Ok(Some(lane)),
            None => Self::at_root(decoder, session, Codes::DictionaryValues),
        }
    }

    /// SUM of the non-null values of `batch` at rows whose `take` is 1.
    fn sum_taken(&self, batch: &LaneBatch, take: &[u64]) -> Result<i128, String> {
        if self.dictionary.is_none() {
            let (mut sum, mut count) = (0_u64, 0_u64);
            if batch.dense() {
                for (&code, &take) in batch.codes.iter().zip(take) {
                    sum += code & take.wrapping_neg();
                    count += take;
                }
            } else {
                for ((&code, &valid), &take) in batch.codes.iter().zip(&batch.valid).zip(take) {
                    let take = take & valid;
                    sum += code & take.wrapping_neg();
                    count += take;
                }
            }
            return self.packed.add(0, count as usize, sum);
        }
        // Each taken id indexes the table; an id outside it fails the query.
        // Untaken rows, null ones among them, read a clamped entry that the
        // mask discards.
        let Some(last) = self.entries.len().checked_sub(1) else {
            return if take.iter().any(|&take| take != 0) {
                Err("dictionary id is outside table".into())
            } else {
                Ok(0)
            };
        };
        let entries = &self.entries;
        let outside = |id: u64, take: u64| u64::from(id > last as u64) & take;
        let entry = |id: u64| entries[(id as usize).min(last)];
        let (sum, outside) = match (self.narrow, batch.dense()) {
            (true, true) => {
                let (mut sum, mut bad) = (0_i64, 0_u64);
                for (&id, &take) in batch.codes.iter().zip(take) {
                    bad |= outside(id, take);
                    sum += entry(id) & (take as i64).wrapping_neg();
                }
                (i128::from(sum), bad)
            }
            (true, false) => {
                let (mut sum, mut bad) = (0_i64, 0_u64);
                for ((&id, &valid), &take) in batch.codes.iter().zip(&batch.valid).zip(take) {
                    let take = take & valid;
                    bad |= outside(id, take);
                    sum += entry(id) & (take as i64).wrapping_neg();
                }
                (i128::from(sum), bad)
            }
            (false, dense) => {
                let (mut sum, mut bad) = (0_i128, 0_u64);
                for (index, (&id, &take)) in batch.codes.iter().zip(take).enumerate() {
                    let take = if dense {
                        take
                    } else {
                        take & batch.valid[index]
                    };
                    bad |= outside(id, take);
                    sum += i128::from(entry(id)) & i128::from(take).wrapping_neg();
                }
                (sum, bad)
            }
        };
        if outside != 0 {
            return Err("dictionary id is outside table".into());
        }
        Ok(sum)
    }

    /// The values of `batch` at `positions`, `None` at null rows. A
    /// dictionary id outside the table fails.
    fn gather_at(
        &self,
        batch: &LaneBatch,
        positions: &[u32],
        out: &mut Vec<Option<i128>>,
    ) -> Result<(), String> {
        out.clear();
        let dense = batch.dense();
        for &position in positions {
            let index = position as usize;
            if !dense && batch.valid[index] == 0 {
                out.push(None);
                continue;
            }
            let code = batch.codes[index];
            out.push(Some(match self.dictionary {
                None => self.packed.base + i128::from(code),
                Some(_) => i128::from(
                    *self
                        .entries
                        .get(code as usize)
                        .ok_or("dictionary id is outside table")?,
                ),
            }));
        }
        Ok(())
    }

    /// The value of every row of `batch`, 0 at null rows. Dictionary ids
    /// of rows `take` marks must lie inside the table.
    fn values_into(
        &self,
        batch: &LaneBatch,
        take: &[u64],
        out: &mut Vec<i128>,
    ) -> Result<(), String> {
        out.clear();
        match self.dictionary {
            None => out.extend(
                batch
                    .codes
                    .iter()
                    .map(|&code| self.packed.base + i128::from(code)),
            ),
            Some((_, entries)) => {
                let last = self.entries.len().checked_sub(1);
                for ((&id, valid), &take) in batch.codes.iter().zip(batch.validity()).zip(take) {
                    if take & valid != 0 && id >= entries as u64 {
                        return Err("dictionary id is outside table".into());
                    }
                    out.push(
                        last.map_or(0, |last| i128::from(self.entries[(id as usize).min(last)])),
                    );
                }
            }
        }
        Ok(())
    }

    fn cursor(&self, session: &mut ReadSession<'_>, row: usize) -> Result<LaneCursor, String> {
        let compact = match self.validity {
            Some(validity) => nullable_prefix_rank(
                session,
                validity.bits,
                validity.rank_index,
                row,
                validity.rank_interval,
            )?,
            None => row,
        };
        Ok(LaneCursor { compact })
    }

    /// Fills `batch` with the rows `rows`, which must follow the rows the
    /// cursor last filled.
    fn fill(
        &self,
        session: &mut ReadSession<'_>,
        cursor: &mut LaneCursor,
        rows: Span,
        batch: &mut LaneBatch,
    ) -> Result<(), String> {
        let Some(validity) = self.validity else {
            batch.valid.clear();
            self.packed.unpack(session, rows, &mut batch.codes)?;
            cursor.compact = rows.end;
            return Ok(());
        };
        let first_byte = rows.start / 8;
        let bytes = session.read_range(
            validity.bits,
            Span::new(first_byte, (rows.end - 1) / 8 + 1)?,
        )?;
        batch.valid.clear();
        batch.valid.extend(
            (rows.start..rows.end)
                .map(|row| u64::from(bytes[row / 8 - first_byte] >> (row % 8) & 1)),
        );
        let present = batch.valid.iter().sum::<u64>() as usize;
        let compact = Span::new(cursor.compact, cursor.compact + present)?;
        self.packed.unpack(session, compact, &mut batch.compact)?;
        cursor.compact = compact.end;
        batch.codes.clear();
        if present == 0 {
            batch.codes.resize(rows.len(), 0);
            return Ok(());
        }
        // Each row takes the next stored code; a null row repeats the one
        // before it, which the validity mask then discards.
        let last = present - 1;
        let mut next = 0_usize;
        for &valid in &batch.valid {
            batch.codes.push(batch.compact[next.min(last)]);
            next += valid as usize;
        }
        Ok(())
    }

    /// Streams `rows` in batches, each with the row it starts at.
    fn scan(
        &self,
        session: &mut ReadSession<'_>,
        rows: Span,
        mut sink: impl FnMut(&mut ReadSession<'_>, usize, &LaneBatch) -> Result<(), String>,
    ) -> Result<(), String> {
        if rows.start >= rows.end {
            return Ok(());
        }
        let mut cursor = self.cursor(session, rows.start)?;
        let mut batch = LaneBatch::default();
        let mut row = rows.start;
        while row < rows.end {
            let end = (row + BATCH_ROWS).min(rows.end);
            self.fill(session, &mut cursor, Span::new(row, end)?, &mut batch)?;
            sink(session, row, &batch)?;
            row = end;
        }
        Ok(())
    }

    fn sum_selection(
        &self,
        session: &mut ReadSession<'_>,
        hull: Span,
        selection: &Selection,
    ) -> Result<i128, String> {
        let mut total = 0_i128;
        let mut take = Vec::new();
        self.scan(session, hull, |_, row, batch| {
            batch.taken(selection, row, &mut take);
            let sum = self.sum_taken(batch, &take)?;
            total = total
                .checked_add(sum)
                .ok_or("vector selected SUM overflow")?;
            Ok(())
        })?;
        Ok(total)
    }

    /// Sets the bit of every non-null row in `rows` that `matcher` keeps,
    /// failing if a non-null row's id lies outside a table of `entries`.
    fn select_with(
        &self,
        session: &mut ReadSession<'_>,
        rows: Span,
        matcher: &Matcher,
        entries: Option<u64>,
        selection: &mut Selection,
    ) -> Result<(), String> {
        let mut bits = Vec::new();
        self.scan(session, rows, |_, row, batch| {
            if let Some(entries) = entries
                && batch.outside(entries) != 0
            {
                return Err("dictionary id is outside table".into());
            }
            if !matches!(matcher, Matcher::Nothing) {
                matcher.test_into(batch, &mut bits);
                set_bits(selection, row, &bits);
            }
            Ok(())
        })
    }
}

/// How a selector lane decides whether a row's value lies in the predicate.
enum Matcher {
    /// Codes in `[first, first + spread]`: offset codes, or ids into a
    /// sorted dictionary.
    Range { first: u64, spread: u64 },
    /// The outcome for each dictionary id, from testing every entry once:
    /// a scan of a dictionary page that needs no order between its entries.
    Entries(Vec<u64>),
    /// No code can match.
    Nothing,
}

impl Matcher {
    /// Fraction of codes that match, for ordering conjuncts.
    fn selectivity(&self, domain: f64) -> f64 {
        match self {
            Self::Range { spread, .. } => (*spread as f64 + 1.0) / domain.max(1.0),
            Self::Entries(hits) => hits.iter().sum::<u64>() as f64 / hits.len().max(1) as f64,
            Self::Nothing => 0.0,
        }
    }

    /// Replaces `out` with 1 or 0 per row of `batch`: whether it holds a
    /// matching value. The variant is chosen once per batch.
    fn test_into(&self, batch: &LaneBatch, out: &mut Vec<u64>) {
        match self {
            Self::Range { first, spread } => {
                batch.test_into(out, |code| u64::from(code.wrapping_sub(*first) <= *spread));
            }
            Self::Entries(hits) => {
                batch.test_into(out, |code| hits.get(code as usize).copied().unwrap_or(0));
            }
            Self::Nothing => {
                out.clear();
                out.resize(batch.codes.len(), 0);
            }
        }
    }

    fn hit(&self, code: u64) -> u64 {
        match self {
            Self::Range { first, spread } => u64::from(code.wrapping_sub(*first) <= *spread),
            Self::Entries(hits) => hits.get(code as usize).copied().unwrap_or(0),
            Self::Nothing => 0,
        }
    }
}

/// A selector page read as a lane, with the matcher for its predicate.
struct SelectorLane {
    lane: Lane,
    matcher: Matcher,
    /// The table size its ids must stay below, for dictionary pages.
    entries: Option<u64>,
    /// How many distinct codes the page can hold.
    domain: f64,
}

/// A selector page read as a lane, the matcher for `[low, high]`, the
/// table size its ids must stay below, and the size of its code domain.
/// Offset codes compare with the predicate shifted by the base. Under
/// `CodeCompare::Offset` a dictionary page is scanned by testing each entry
/// once, which needs no order; `SortedDictionary` compares ids with the
/// predicate's id range, which a translation licence permits. `None` when
/// the page is not codes in either form.
fn selector_lane(
    decoder: &DecoderIr,
    session: &mut ReadSession<'_>,
    low: i64,
    high: i64,
    compare: CodeCompare,
) -> Result<Option<SelectorLane>, String> {
    let (low, high) = (i128::from(low), i128::from(high));
    if compare == CodeCompare::Offset {
        if let Some(lane) = Lane::at_root(decoder, session, Codes::Offset)? {
            let base = lane.packed.base;
            let matcher = match lane.packed.code_range(low - base, high - base) {
                Some((first, last)) => Matcher::Range {
                    first,
                    spread: last - first,
                },
                None => Matcher::Nothing,
            };
            let domain = (1_u128 << lane.packed.width) as f64;
            return Ok(Some(SelectorLane {
                lane,
                matcher,
                entries: None,
                domain,
            }));
        }
        let Some(lane) = Lane::at_root(decoder, session, Codes::DictionaryValues)? else {
            return Ok(None);
        };
        let (_, entries) = lane.dictionary.ok_or("dictionary lane lacks its table")?;
        let hits = lane
            .entries
            .iter()
            .map(|&entry| u64::from(low <= i128::from(entry) && i128::from(entry) <= high))
            .collect::<Vec<_>>();
        let matcher = if hits.contains(&1) {
            Matcher::Entries(hits)
        } else {
            Matcher::Nothing
        };
        return Ok(Some(SelectorLane {
            lane,
            matcher,
            entries: Some(entries as u64),
            domain: entries as f64,
        }));
    }
    let Some(lane) = Lane::at_root(decoder, session, Codes::SortedDictionary)? else {
        return Ok(None);
    };
    let (table, entries) = lane.dictionary.ok_or("dictionary lane lacks its table")?;
    let first = table_bound(session, table, entries, low as i64, false)?;
    let end = table_bound(session, table, entries, high as i64, true)?;
    let matcher = if first < end {
        Matcher::Range {
            first: first as u64,
            spread: (end - 1 - first) as u64,
        }
    } else {
        Matcher::Nothing
    };
    Ok(Some(SelectorLane {
        lane,
        matcher,
        entries: Some(entries as u64),
        domain: entries as f64,
    }))
}

/// ORs one bit per row, starting at `row`, into `selection`, a whole word at
/// a time when the row is word-aligned.
fn set_bits(selection: &mut Selection, row: usize, bits: &[u64]) {
    let offset = row - selection.rows.start;
    if offset.is_multiple_of(64) {
        for (word, block) in bits.chunks(64).enumerate() {
            let packed = block
                .iter()
                .enumerate()
                .fold(0_u64, |word, (index, &bit)| word | bit << index);
            selection.words[offset / 64 + word] |= packed;
        }
    } else {
        for (index, &bit) in bits.iter().enumerate() {
            let at = offset + index;
            selection.words[at / 64] |= bit << (at % 64);
        }
    }
}

fn select_chunk(selection: &mut Selection, row: usize, chunk: Chunk<'_>, low: i128, high: i128) {
    match chunk {
        Chunk::Values(values) => {
            let offset = row - selection.rows.start;
            for (index, &value) in values.iter().enumerate() {
                let hit = u64::from(low <= value && value <= high);
                let bit = offset + index;
                selection.words[bit / 64] |= hit << (bit % 64);
            }
        }
        Chunk::Run { value, len } if low <= value && value <= high => {
            selection.set_range(row, row + len);
        }
        Chunk::Run { .. } | Chunk::Nulls { .. } => {}
    }
}

/// Exact SUM of the non-null values in ordered, disjoint `ranges`.
pub fn sum_ranges(
    decoder: &DecoderIr,
    session: &mut ReadSession<'_>,
    ranges: &[Span],
) -> Result<i128, String> {
    validate_ranges(decoder, ranges)?;
    if let Some(packed) = Packed::at_root(decoder, session)? {
        return packed.sum_ranges(session, ranges);
    }
    // Nulls add nothing, so a nullable page of codes sums the stored codes
    // of each range's non-null rows: a contiguous run of them.
    if let Some(lane) = Lane::at_root(decoder, session, Codes::Offset)?
        && let Some(validity) = lane.validity
    {
        let rows = decoder.len()?;
        let mut stored = Vec::with_capacity(ranges.len());
        for range in ranges {
            // Every row's rank comes from the rank index, except the end
            // of the column, which ranks after every stored code.
            let mut rank = |row| {
                if row == rows {
                    Ok(lane.packed.len)
                } else {
                    nullable_prefix_rank(
                        session,
                        validity.bits,
                        validity.rank_index,
                        row,
                        validity.rank_interval,
                    )
                }
            };
            let (start, end) = (rank(range.start)?, rank(range.end)?);
            stored.push(Span::new(start, end)?);
        }
        return lane.packed.sum_ranges(session, &stored);
    }
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
/// answers from its validity bits alone, and a tree with no nullable node
/// has no nulls at all.
pub fn non_null(
    decoder: &DecoderIr,
    session: &mut ReadSession<'_>,
    rows: Span,
) -> Result<Vec<Span>, String> {
    validate_rows(decoder, rows)?;
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
        // Only a nullable node yields nulls; a tree without one is dense, so
        // asking a run-length or patched page for its non-null rows decodes
        // nothing.
        _ if !decoder
            .nodes()
            .iter()
            .any(|node| matches!(node, DecoderNode::Nullable { .. })) =>
        {
            return Ok(vec![rows]);
        }
        _ => {}
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
    validate_rows(decoder, rows)?;
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
    if low > high {
        return Err("vector BETWEEN bounds are invalid".into());
    }
    validate_rows(decoder, rows)?;
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
    validate_rows(decoder, rows)?;
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
            // Runs are read a batch at a time: their lengths as one range,
            // their values with one scan of the value node.
            let mut lengths = Vec::new();
            let mut run_values: Vec<Option<i128>> = Vec::new();
            while run < runs && run_start < end {
                let batch_end = (run + RLE_BATCH_RUNS).min(runs);
                session.read_u32_into(run_lengths, Span::new(run, batch_end)?, &mut lengths)?;
                run_values.clear();
                scan_node(
                    decoder,
                    values,
                    session,
                    run,
                    batch_end,
                    &mut |_, _, chunk| {
                        match chunk {
                            Chunk::Values(values) => {
                                run_values.extend(values.iter().map(|&value| Some(value)));
                            }
                            Chunk::Run { value, len } => {
                                run_values.extend(std::iter::repeat_n(Some(value), len));
                            }
                            Chunk::Nulls { len } => {
                                run_values.extend(std::iter::repeat_n(None, len));
                            }
                        }
                        Ok(())
                    },
                )?;
                for (&length, &value) in lengths.iter().zip(&run_values) {
                    if run_start >= end {
                        break;
                    }
                    let run_end = run_start
                        .checked_add(length)
                        .ok_or("run-length row overflow")?;
                    let span_start = start.max(run_start);
                    let span_end = end.min(run_end);
                    if span_start < span_end {
                        let len = span_end - span_start;
                        let chunk = match value {
                            Some(value) => Chunk::Run { value, len },
                            None => Chunk::Nulls { len },
                        };
                        sink(session, span_start, chunk)?;
                    }
                    run_start = run_end;
                    run += 1;
                }
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
            let compact =
                nullable_prefix_rank(session, validity, rank_index, start, rank_interval)?;
            let first_byte = start / 8;
            let bits = session
                .read_range(validity, Span::new(first_byte, (end - 1) / 8 + 1)?)?
                .to_vec();
            let valid = |row: usize| bits[row / 8 - first_byte] >> (row % 8) & 1 == 1;
            let present = (start..end).filter(|&row| valid(row)).count();
            // The stored values are scanned once, and each chunk is laid over
            // the next valid rows, with the null runs between them emitted
            // as they are reached.
            let mut logical = start;
            let emit_nulls = |session: &mut ReadSession<'p>,
                              logical: &mut usize,
                              sink: &mut Sink<'_, 'p>|
             -> Result<(), String> {
                let null_start = *logical;
                while *logical < end && !valid(*logical) {
                    *logical += 1;
                }
                if *logical > null_start {
                    sink(
                        session,
                        null_start,
                        Chunk::Nulls {
                            len: *logical - null_start,
                        },
                    )?;
                }
                Ok(())
            };
            scan_node(
                decoder,
                values,
                session,
                compact,
                compact + present,
                &mut |session, _, chunk| {
                    let mut remaining = chunk.len();
                    let mut offset = 0;
                    let mut chunk = chunk;
                    while remaining > 0 {
                        emit_nulls(session, &mut logical, sink)?;
                        let run_start = logical;
                        while logical < end && valid(logical) && logical - run_start < remaining {
                            logical += 1;
                        }
                        let len = logical - run_start;
                        if len == 0 {
                            return Err("nullable values outrun their validity".into());
                        }
                        match &mut chunk {
                            Chunk::Values(values) => {
                                sink(
                                    session,
                                    run_start,
                                    Chunk::Values(&mut values[offset..offset + len]),
                                )?;
                            }
                            Chunk::Run { value, .. } => {
                                sink(session, run_start, Chunk::Run { value: *value, len })?;
                            }
                            Chunk::Nulls { .. } => {
                                sink(session, run_start, Chunk::Nulls { len })?;
                            }
                        }
                        offset += len;
                        remaining -= len;
                    }
                    Ok(())
                },
            )?;
            emit_nulls(session, &mut logical, sink)?;
            if logical != end {
                return Err("nullable values end before their validity".into());
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

fn validate_rows(decoder: &DecoderIr, rows: Span) -> Result<(), String> {
    if rows.start > rows.end || rows.end > decoder.len()? {
        return Err("vector row range is invalid".into());
    }
    Ok(())
}

/// Validate externally supplied bitmap selections before using their hull,
/// count, or words, including bits beyond the last logical row.
pub(super) fn validate_selection(decoder: &DecoderIr, selection: &Selection) -> Result<(), String> {
    validate_rows(decoder, selection.rows)?;
    let len = selection.rows.len();
    if selection.words.len() != len.div_ceil(64) {
        return Err("vector selection bitmap length is invalid".into());
    }
    let tail = len % 64;
    if tail != 0 && selection.words.last().is_some_and(|word| word >> tail != 0) {
        return Err("vector selection contains rows beyond its range".into());
    }
    Ok(())
}

pub(super) fn validate_ranges(decoder: &DecoderIr, ranges: &[Span]) -> Result<(), String> {
    let len = decoder.len()?;
    let mut previous = 0;
    for range in ranges {
        if range.start > range.end || range.start < previous || range.end > len {
            return Err("vector selection ranges are invalid".into());
        }
        previous = range.end;
    }
    Ok(())
}
