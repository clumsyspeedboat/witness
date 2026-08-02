use witness::access_compiler::Span;

#[derive(Clone, Debug)]
pub struct PredicateSpec {
    pub label: String,
    pub requested: String,
    pub low: i64,
    pub high: i64,
}

impl PredicateSpec {
    pub fn upper_exclusive(&self) -> Result<i64, String> {
        self.high
            .checked_add(1)
            .ok_or_else(|| "predicate upper bound cannot be made exclusive".into())
    }
}

pub fn predicates(values: &[Option<i64>]) -> Result<Vec<PredicateSpec>, String> {
    let mut present = values.iter().flatten().copied().collect::<Vec<_>>();
    present.sort_unstable();
    if present.is_empty() {
        return Err("selector contains no values".into());
    }
    let candidates = [1_usize, 10, 100, 500]
        .into_iter()
        .zip(["0.1pct", "1pct", "10pct", "50pct"])
        .map(|(per_thousand, label)| {
            let width = (present.len() * per_thousand / 1_000).max(1);
            let start = (present.len() - width) / 2;
            let high = present[start + width - 1];
            high.checked_add(1)
                .ok_or_else(|| "selector maximum prevents a half-open predicate".to_string())?;
            Ok(PredicateSpec {
                label: label.into(),
                requested: format!("{:.3}", per_thousand as f64 / 1_000.0),
                low: present[start],
                high,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut unique: Vec<PredicateSpec> = Vec::new();
    for candidate in candidates {
        if let Some(existing) = unique
            .iter_mut()
            .find(|item| item.low == candidate.low && item.high == candidate.high)
        {
            existing.label.push_str(&format!("+{}", candidate.label));
            existing
                .requested
                .push_str(&format!("|{}", candidate.requested));
        } else {
            unique.push(candidate);
        }
    }
    Ok(unique)
}

pub fn truth(
    selector: &[Option<i64>],
    values: &[Option<i64>],
    predicate: &PredicateSpec,
) -> Result<(Vec<Span>, i128), String> {
    if selector.len() != values.len() {
        return Err("selector/value row count differs".into());
    }
    let mut ranges: Vec<Span> = Vec::new();
    let mut sum = 0_i128;
    for (row, (selector, value)) in selector.iter().zip(values).enumerate() {
        if selector.is_some_and(|value| predicate.low <= value && value <= predicate.high) {
            if let Some(previous) = ranges.last_mut()
                && previous.end == row
            {
                previous.end = row + 1;
            } else {
                ranges.push(Span::new(row, row + 1)?);
            }
            if let Some(value) = value {
                sum = sum
                    .checked_add(i128::from(*value))
                    .ok_or("truth SUM overflow")?;
            }
        }
    }
    Ok((ranges, sum))
}

pub fn selected_rows(ranges: &[Span]) -> usize {
    ranges.iter().map(|range| range.len()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tied_domains_collapse_duplicate_predicates() {
        let values = vec![Some(38); 1_000];
        let predicates = predicates(&values).unwrap();
        assert_eq!(predicates.len(), 1);
        assert_eq!(predicates[0].requested, "0.001|0.010|0.100|0.500");
    }

    #[test]
    fn distinct_domains_retain_four_selectivities() {
        let values = (0..10_000).map(Some).collect::<Vec<_>>();
        assert_eq!(predicates(&values).unwrap().len(), 4);
    }

    #[test]
    fn truth_merges_rows_and_skips_null_values_in_sum() {
        let predicate = PredicateSpec {
            label: "test".into(),
            requested: "test".into(),
            low: 2,
            high: 4,
        };
        let selector = [Some(1), Some(2), Some(3), Some(4), Some(8)];
        let values = [Some(10), Some(20), None, Some(40), Some(80)];
        let (ranges, sum) = truth(&selector, &values, &predicate).unwrap();
        assert_eq!(ranges, [Span::new(1, 4).unwrap()]);
        assert_eq!(sum, 60);
    }
}
