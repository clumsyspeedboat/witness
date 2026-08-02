use super::Span;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadSchedule {
    pub ranges: Vec<Span>,
    pub required_bytes: usize,
    pub scheduled_bytes: usize,
}

pub fn coalesce_ranges(mut ranges: Vec<Span>, maximum_gap: usize) -> Result<ReadSchedule, String> {
    ranges.retain(|range| !range.is_empty());
    ranges.sort_unstable();
    let required_bytes = union_bytes(&ranges);
    let mut coalesced: Vec<Span> = Vec::new();
    for range in ranges {
        if let Some(last) = coalesced.last_mut() {
            let merge_limit = last
                .end
                .checked_add(maximum_gap)
                .ok_or("coalescing gap overflow")?;
            if range.start <= merge_limit {
                last.end = last.end.max(range.end);
                continue;
            }
        }
        coalesced.push(range);
    }
    Ok(ReadSchedule {
        scheduled_bytes: coalesced.iter().copied().map(Span::len).sum(),
        ranges: coalesced,
        required_bytes,
    })
}

fn union_bytes(ranges: &[Span]) -> usize {
    let mut merged: Vec<Span> = Vec::new();
    for &range in ranges {
        if let Some(last) = merged.last_mut()
            && range.start <= last.end
        {
            last.end = last.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    merged.into_iter().map(Span::len).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalescing_trades_calls_for_explicit_gap_bytes() {
        let schedule = coalesce_ranges(
            vec![
                Span::new(100, 120).unwrap(),
                Span::new(125, 140).unwrap(),
                Span::new(300, 310).unwrap(),
            ],
            8,
        )
        .unwrap();
        assert_eq!(
            schedule.ranges,
            vec![
                Span {
                    start: 100,
                    end: 140
                },
                Span {
                    start: 300,
                    end: 310
                }
            ]
        );
        assert_eq!(schedule.required_bytes, 45);
        assert_eq!(schedule.scheduled_bytes, 50);
    }
}
