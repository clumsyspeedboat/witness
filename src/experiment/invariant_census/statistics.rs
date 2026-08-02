use std::collections::BTreeSet;

use super::model::PageRow;

pub const PAGE_SIZES: [usize; 2] = [1_024, 16_384];
pub const DISPLACEMENT_THRESHOLDS: [usize; 6] = [0, 1, 4, 16, 64, 256];
pub const MIN_MONOTONE_SEGMENT: usize = 128;

#[derive(Clone, Debug)]
pub struct ValueStatistics {
    pub nulls: usize,
    pub unique_non_null: usize,
    pub monotone_non_null: bool,
    pub null_placement: &'static str,
    pub distinct_non_null: bool,
    pub max_rank_displacement: usize,
    pub monotone_segment_rows: usize,
    pub monotone_segments: usize,
}

pub fn analyze_values(values: &[Option<i64>]) -> ValueStatistics {
    let dense = values.iter().flatten().copied().collect::<Vec<_>>();
    let unique_non_null = dense.iter().copied().collect::<BTreeSet<_>>().len();
    let (monotone_segment_rows, monotone_segments) = monotone_segment_coverage(&dense);
    ValueStatistics {
        nulls: values.len() - dense.len(),
        unique_non_null,
        monotone_non_null: is_monotone(&dense),
        null_placement: null_placement(values),
        distinct_non_null: unique_non_null == dense.len(),
        max_rank_displacement: max_rank_displacement(&dense),
        monotone_segment_rows,
        monotone_segments,
    }
}

pub fn analyze_pages(column: usize, values: &[Option<i64>]) -> Vec<PageRow> {
    let mut rows = Vec::new();
    for page_size in PAGE_SIZES {
        for (page, values) in values.chunks(page_size).enumerate() {
            let stats = analyze_values(values);
            rows.push(PageRow {
                column,
                page_size,
                page,
                rows: values.len(),
                nulls: stats.nulls,
                monotone_non_null: stats.monotone_non_null,
                distinct_non_null: stats.distinct_non_null,
                max_rank_displacement: stats.max_rank_displacement,
                unique_non_null: stats.unique_non_null,
            });
        }
    }
    rows
}

pub fn median(mut values: Vec<f64>) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    if values.len().is_multiple_of(2) {
        (values[values.len() / 2 - 1] + values[values.len() / 2]) / 2.0
    } else {
        values[values.len() / 2]
    }
}

fn is_monotone(values: &[i64]) -> bool {
    values.windows(2).all(|pair| pair[0] <= pair[1])
}

fn null_placement(values: &[Option<i64>]) -> &'static str {
    if values.iter().all(Option::is_some) {
        return "none";
    }
    let first = values.iter().position(Option::is_some);
    let last = values.iter().rposition(Option::is_some);
    match (first, last) {
        (Some(first), Some(last))
            if values[..first].iter().all(Option::is_none)
                && values[first..=last].iter().all(Option::is_some)
                && last + 1 == values.len() =>
        {
            "first"
        }
        (Some(0), Some(last))
            if values[..=last].iter().all(Option::is_some)
                && values[last + 1..].iter().all(Option::is_none) =>
        {
            "last"
        }
        _ => "arbitrary",
    }
}

fn max_rank_displacement(values: &[i64]) -> usize {
    let mut ranked = values
        .iter()
        .copied()
        .enumerate()
        .collect::<Vec<(usize, i64)>>();
    ranked.sort_unstable_by_key(|&(position, value)| (value, position));
    ranked
        .iter()
        .enumerate()
        .map(|(sorted, &(original, _))| sorted.abs_diff(original))
        .max()
        .unwrap_or(0)
}

fn monotone_segment_coverage(values: &[i64]) -> (usize, usize) {
    if values.is_empty() {
        return (0, 0);
    }
    let mut covered = 0;
    let mut segments = 0;
    let mut start = 0;
    for end in 1..=values.len() {
        let boundary = end == values.len() || values[end] < values[end - 1];
        if boundary {
            let len = end - start;
            if len >= MIN_MONOTONE_SEGMENT {
                covered += len;
                segments += 1;
            }
            start = end;
        }
    }
    (covered, segments)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_rank_displacement_handles_duplicates() {
        assert_eq!(max_rank_displacement(&[1, 1, 2, 2]), 0);
        assert_eq!(max_rank_displacement(&[2, 1, 1, 2]), 2);
    }

    #[test]
    fn short_monotone_fragments_do_not_fake_segment_coverage() {
        let values = (0..1_000).map(|row| (row % 10) as i64).collect::<Vec<_>>();
        assert_eq!(monotone_segment_coverage(&values), (0, 0));
        assert_eq!(
            monotone_segment_coverage(&(0..128).collect::<Vec<_>>()),
            (128, 1)
        );
    }
}
