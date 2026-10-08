//! String columns as pages of a sorted dictionary and integer ids.
//!
//! Each page stores its distinct non-null strings once, in ascending byte
//! order, and every row as the position of its string in that dictionary,
//! or null. The ids of all pages form an integer paged column, so they are
//! encoded, planned, and executed by the integer machinery unchanged.
//!
//! Sorting gives the page two facts. Its first and last entries bound every
//! string it holds, and ids follow string order, so a range of strings is a
//! range of ids. A predicate `low <= s <= high` (or a prefix, which is a
//! half-open range) therefore becomes, per page, a skip, an acceptance of
//! every non-null row, or a comparison of integer ids, found by two binary
//! searches over the dictionary. A checker confirms a plan by re-reading the
//! at most four entries on either side of the id range's two boundaries.
//!
//! The order of the entries is checked evidence: the writer sorts, and
//! `StringColumn::from_values` verifies the order of every dictionary it
//! builds. A dictionary is only trusted to be sorted because its writer said
//! so, as with the integer column certificate.

use std::ops::Bound;

use super::paged::{self, PageCounts, PagePolicy, PagedColumn, Planning, SumAction};
use super::{ClosureMode, ReadSession, Span, vector};

/// The sorted distinct strings of one page.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Dictionary {
    /// `offsets[i]..offsets[i + 1]` delimits entry `i` in `bytes`.
    offsets: Vec<u32>,
    bytes: Vec<u8>,
}

impl Dictionary {
    pub fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn entry(&self, index: usize) -> &[u8] {
        &self.bytes[self.offsets[index] as usize..self.offsets[index + 1] as usize]
    }

    /// Bytes the dictionary occupies: its entries and their offsets.
    pub fn stored_bytes(&self) -> usize {
        self.bytes.len() + 4 * self.offsets.len()
    }

    fn from_sorted(entries: &[&[u8]]) -> Result<Self, String> {
        let mut dictionary = Self {
            offsets: Vec::with_capacity(entries.len() + 1),
            bytes: Vec::new(),
        };
        dictionary.offsets.push(0);
        for entry in entries {
            dictionary.bytes.extend_from_slice(entry);
            dictionary
                .offsets
                .push(u32::try_from(dictionary.bytes.len()).map_err(|_| "dictionary too large")?);
        }
        if (1..dictionary.len()).any(|index| dictionary.entry(index - 1) >= dictionary.entry(index))
        {
            return Err("dictionary entries are not strictly ascending".into());
        }
        Ok(dictionary)
    }

    /// The first entry not ordered before `bound`'s range on the low side.
    fn lower(&self, low: Bound<&[u8]>, probes: &mut usize) -> usize {
        self.partition(|entry| below(entry, low), probes)
    }

    /// The first entry above `high`.
    fn upper(&self, high: Bound<&[u8]>, probes: &mut usize) -> usize {
        self.partition(|entry| !above(entry, high), probes)
    }

    fn partition(&self, mut left_side: impl FnMut(&[u8]) -> bool, probes: &mut usize) -> usize {
        let (mut low, mut high) = (0, self.len());
        while low < high {
            let middle = low + (high - low) / 2;
            *probes += 1;
            if left_side(self.entry(middle)) {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low
    }
}

/// Whether `entry` lies below a range's lower bound.
fn below(entry: &[u8], low: Bound<&[u8]>) -> bool {
    match low {
        Bound::Unbounded => false,
        Bound::Included(low) => entry < low,
        Bound::Excluded(low) => entry <= low,
    }
}

/// Whether `entry` lies above a range's upper bound.
fn above(entry: &[u8], high: Bound<&[u8]>) -> bool {
    match high {
        Bound::Unbounded => false,
        Bound::Included(high) => entry > high,
        Bound::Excluded(high) => entry >= high,
    }
}

/// A range of strings in byte order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StringRange<'a> {
    pub low: Bound<&'a [u8]>,
    pub high: Bound<&'a [u8]>,
}

impl<'a> StringRange<'a> {
    /// `low <= s <= high`.
    pub fn between(low: &'a [u8], high: &'a [u8]) -> Self {
        Self {
            low: Bound::Included(low),
            high: Bound::Included(high),
        }
    }

    pub fn contains(&self, value: &[u8]) -> bool {
        !below(value, self.low) && !above(value, self.high)
    }
}

/// A borrowed view of an owned bound.
pub fn bound_view(bound: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match bound {
        Bound::Included(value) => Bound::Included(value.as_slice()),
        Bound::Excluded(value) => Bound::Excluded(value.as_slice()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

/// The strings starting with `prefix`, as a range: `prefix <= s < next`,
/// where `next` is the shortest string above every extension of `prefix`
/// (none when `prefix` is empty or all 0xff bytes).
pub fn prefix_range(prefix: &[u8]) -> (Vec<u8>, Option<Vec<u8>>) {
    let mut next = prefix.to_vec();
    while let Some(last) = next.pop() {
        if last < 0xff {
            next.push(last + 1);
            return (prefix.to_vec(), Some(next));
        }
    }
    (prefix.to_vec(), None)
}

/// A string column: one sorted dictionary per page and the ids of every row.
#[derive(Clone, Debug)]
pub struct StringColumn {
    dictionaries: Vec<Dictionary>,
    pub ids: PagedColumn,
}

impl StringColumn {
    pub fn from_values(
        values: &[Option<&[u8]>],
        page_rows: usize,
        policy: &PagePolicy,
    ) -> Result<Self, String> {
        let mut ids = PagedColumn::new(page_rows)?;
        let mut dictionaries = Vec::new();
        for chunk in values.chunks(page_rows) {
            let mut entries = chunk.iter().flatten().copied().collect::<Vec<_>>();
            entries.sort_unstable();
            entries.dedup();
            let dictionary = Dictionary::from_sorted(&entries)?;
            let page_ids = chunk
                .iter()
                .map(|value| {
                    value.map(|value| {
                        entries
                            .binary_search(&value)
                            .map(|id| id as i64)
                            .expect("every value is in its page's dictionary")
                    })
                })
                .collect::<Vec<_>>();
            ids.append(&page_ids, policy)?;
            dictionaries.push(dictionary);
        }
        Ok(Self { dictionaries, ids })
    }

    pub fn pages(&self) -> usize {
        self.dictionaries.len()
    }

    pub fn dictionary(&self, page: usize) -> &Dictionary {
        &self.dictionaries[page]
    }

    /// Bytes the column occupies: dictionaries and id pages.
    pub fn stored_bytes(&self) -> usize {
        self.dictionaries
            .iter()
            .map(Dictionary::stored_bytes)
            .sum::<usize>()
            + self.ids.bytes()
    }
}

/// What a string predicate does on one page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StringAction {
    /// No row can match: the page holds no string, or its ends exclude the
    /// range.
    Skip,
    /// No row can match: the range falls before entry `at` and after the
    /// entry before it.
    SkipAt(u64),
    /// Every non-null row matches.
    Accept,
    /// Rows match exactly when their id lies in `first..=last`.
    Translate { first: u64, last: u64 },
    /// Every row's string is tested.
    Scan,
}

/// Plans `range` on page `page` and checks the plan. `Planning::Facts` uses
/// the dictionary's order; `Planning::StoredStatistics` uses the page's exact
/// smallest and largest string, as a zone map stores them; `Planning::ScanOnly`
/// uses nothing. `probes` counts the dictionary entries read.
pub fn plan_string(
    column: &StringColumn,
    page: usize,
    range: StringRange<'_>,
    planning: Planning,
    probes: &mut usize,
) -> Result<StringAction, String> {
    let dictionary = column.dictionary(page);
    let entries = dictionary.len();
    let action = match planning {
        Planning::ScanOnly => StringAction::Scan,
        _ if entries == 0 => StringAction::Skip,
        Planning::StoredStatistics => {
            *probes += 2;
            let (smallest, largest) = (dictionary.entry(0), dictionary.entry(entries - 1));
            if above(smallest, range.high) || below(largest, range.low) {
                StringAction::Skip
            } else if range.contains(smallest) && range.contains(largest) {
                StringAction::Accept
            } else {
                StringAction::Scan
            }
        }
        Planning::Facts => {
            let first = dictionary.lower(range.low, probes);
            let end = dictionary.upper(range.high, probes);
            if first >= end {
                StringAction::SkipAt(first as u64)
            } else if first == 0 && end == entries {
                StringAction::Accept
            } else {
                StringAction::Translate {
                    first: first as u64,
                    last: end as u64 - 1,
                }
            }
        }
    };
    check_string(dictionary, action, range, planning)?;
    Ok(action)
}

/// Confirms that `action` is what the dictionary implies for `range`, by
/// reading the entries on either side of each boundary.
pub fn check_string(
    dictionary: &Dictionary,
    action: StringAction,
    range: StringRange<'_>,
    planning: Planning,
) -> Result<(), String> {
    let entries = dictionary.len();
    let entry = |index: usize| dictionary.entry(index);
    let (first, end) = match action {
        StringAction::Scan => return Ok(()),
        StringAction::Skip if entries == 0 => return Ok(()),
        StringAction::Skip => {
            return if above(entry(0), range.high) || below(entry(entries - 1), range.low) {
                Ok(())
            } else {
                Err("string skip is not justified by the page's ends".into())
            };
        }
        StringAction::SkipAt(at) => {
            if planning != Planning::Facts {
                return Err("only dictionary order places a skip between entries".into());
            }
            (at as usize, at as usize)
        }
        StringAction::Accept => (0, entries),
        StringAction::Translate { first, last } => {
            if planning != Planning::Facts {
                return Err("only dictionary order licenses an id translation".into());
            }
            (first as usize, last as usize + 1)
        }
    };
    if first > end || end > entries {
        return Err("string plan's id range lies outside the dictionary".into());
    }
    let fits = (first == 0 || below(entry(first - 1), range.low))
        && (first == entries || !below(entry(first), range.low))
        && (end == 0 || !above(entry(end - 1), range.high))
        && (end == entries || above(entry(end), range.high));
    let first_matches = first < end && range.contains(entry(first));
    let consistent = match action {
        StringAction::SkipAt(_) => first == end && fits,
        _ => fits && first_matches,
    };
    if consistent {
        Ok(())
    } else {
        Err("string plan disagrees with its dictionary".into())
    }
}

/// `SELECT SUM(value), COUNT(*) WHERE s IN range` over a string column and
/// an integer column paged alike. Returns the sum, the matching rows, the
/// page counts, and the dictionary entries read.
pub fn string_sum_where(
    column: &StringColumn,
    range: StringRange<'_>,
    value: &PagedColumn,
    planning: Planning,
) -> Result<(i128, usize, PageCounts, usize), String> {
    if column.ids.rows() != value.rows()
        || column.ids.page_rows() != value.page_rows()
        || column.pages() != value.pages().len()
    {
        return Err("a string predicate needs a value column paged alike".into());
    }
    let mut counts = PageCounts::default();
    let mut probes = 0;
    let (mut total, mut matched) = (0_i128, 0_usize);
    for (index, (id_page, value_page)) in column.ids.pages().iter().zip(value.pages()).enumerate() {
        let action = plan_string(column, index, range, planning, &mut probes)?;
        let mut session = ReadSession::new_untracked(&id_page.column.page, ClosureMode::Selective);
        let all = Span::new(0, id_page.rows())?;
        let selection = match action {
            StringAction::Skip | StringAction::SkipAt(_) => {
                counts.skipped += 1;
                continue;
            }
            StringAction::Accept => {
                counts.accepted += 1;
                let mut selection = vector::Selection::empty(all);
                for span in vector::non_null(&id_page.column.decoder, &mut session, all)? {
                    selection.set_range(span.start, span.end);
                }
                selection
            }
            StringAction::Translate { first, last } => {
                counts.translated += 1;
                let (first, last) = (
                    i64::try_from(first).map_err(|_| "id out of range")?,
                    i64::try_from(last).map_err(|_| "id out of range")?,
                );
                vector::between_selection(&id_page.column.decoder, &mut session, all, first, last)?
            }
            StringAction::Scan => {
                // Without order, every entry is tested once and each row's
                // outcome is looked up by its id.
                counts.scanned += 1;
                let dictionary = column.dictionary(index);
                probes += dictionary.len();
                let hits = (0..dictionary.len())
                    .map(|entry| range.contains(dictionary.entry(entry)))
                    .collect::<Vec<_>>();
                vector::selection_by_table(&id_page.column.decoder, &mut session, all, &hits)?
            }
        };
        let rows = selection.count();
        if rows == 0 {
            continue;
        }
        let mut value_session =
            ReadSession::new_untracked(&value_page.column.page, ClosureMode::Selective);
        let sum_action = paged::plan_sum(value_page, &mut value_session, planning)?;
        counts.constant_sums += usize::from(matches!(sum_action, SumAction::Constant(_)));
        let sum =
            paged::execute_sum_selection(value_page, &mut value_session, &sum_action, &selection)?;
        total = total.checked_add(sum).ok_or("string SUM overflow")?;
        matched += rows;
    }
    Ok((total, matched, counts, probes))
}
