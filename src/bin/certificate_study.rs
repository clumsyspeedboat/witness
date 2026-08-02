use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::hint::black_box;
use std::time::Instant;

use witness::access_compiler::{
    BlockBloom, BlockMinMax, CandidatePlan, OutputGuarantee, SparseFence, intersect_candidates,
    refine_in,
};
use witness::experiment::datasets::invariant_census_columns;

const ROWS: usize = 131_072;
const BLOCK_ROWS: usize = 1_024;
const BLOOM_BITS_PER_VALUE: usize = 11;
const REPEATS: usize = 9;
const RESULT_DIR: &str = "experiments/results/certificate_study";

#[derive(Clone)]
struct QuerySpec {
    name: &'static str,
    targets: Vec<i64>,
}

#[derive(Clone)]
struct Observation {
    group: String,
    source: String,
    column: String,
    query: String,
    plan: String,
    rows: usize,
    metadata_bytes: usize,
    candidate_rows: usize,
    false_positive_blocks: usize,
    matches: usize,
    p25_ns: f64,
    median_ns: f64,
    p75_ns: f64,
}

fn main() -> Result<(), Box<dyn Error>> {
    std::fs::create_dir_all(RESULT_DIR)?;
    let mut observations = Vec::new();
    for column in invariant_census_columns(ROWS)? {
        let bloom = BlockBloom::build(&column.values, BLOCK_ROWS, BLOOM_BITS_PER_VALUE)?;
        let minmax = BlockMinMax::build(&column.values, BLOCK_ROWS)?;
        let fence = SparseFence::build_equal_budget(&column.values, bloom.bytes()).ok();
        for query in query_specs(&column.values)? {
            let blocks = column.values.len().div_ceil(BLOCK_ROWS);
            let scan = CandidatePlan {
                blocks: (0..blocks).collect(),
                block_rows: BLOCK_ROWS,
                metadata_bytes: 0,
                guarantee: OutputGuarantee::CandidateBitmap,
            };
            let bloom_plan = bloom.probe_in(&query.targets);
            let minmax_plan = minmax.probe_in(&query.targets);
            let combined = intersect_candidates(&bloom_plan, &minmax_plan);
            for (name, plan) in [
                ("scan", scan),
                ("minmax", minmax_plan),
                ("bloom", bloom_plan),
                ("minmax_bloom", combined),
            ] {
                observations.push(observe(&column, &query, name, plan, || match name {
                    "scan" => CandidatePlan {
                        blocks: (0..blocks).collect(),
                        block_rows: BLOCK_ROWS,
                        metadata_bytes: 0,
                        guarantee: OutputGuarantee::CandidateBitmap,
                    },
                    "minmax" => minmax.probe_in(&query.targets),
                    "bloom" => bloom.probe_in(&query.targets),
                    _ => intersect_candidates(
                        &bloom.probe_in(&query.targets),
                        &minmax.probe_in(&query.targets),
                    ),
                })?);
            }
            if let Some(fence) = &fence {
                let plan = fence_candidates(fence, &query.targets);
                observations.push(observe(&column, &query, "sparse_fence", plan, || {
                    fence_candidates(fence, &query.targets)
                })?);
            }
        }
    }
    write_cells(&observations)?;
    write_summary(&observations)?;
    println!("wrote {} certificate observations", observations.len());
    Ok(())
}

fn observe(
    column: &witness::experiment::types::Column,
    query: &QuerySpec,
    name: &str,
    plan: CandidatePlan,
    mut build: impl FnMut() -> CandidatePlan,
) -> Result<Observation, Box<dyn Error>> {
    let expected = column
        .values
        .iter()
        .filter(|value| value.is_some_and(|value| query.targets.contains(&value)))
        .count();
    let actual = refine_in(&column.values, &plan, &query.targets)?;
    let actual_count = actual.iter().map(|range| range.len()).sum::<usize>();
    if actual_count != expected {
        return Err(format!("{name} dropped an equality match in {}", column.name).into());
    }
    let false_positive_blocks = plan
        .blocks
        .iter()
        .filter(|&&block| {
            let start = block * plan.block_rows;
            let end = (start + plan.block_rows).min(column.values.len());
            !column.values[start..end]
                .iter()
                .any(|value| value.is_some_and(|value| query.targets.contains(&value)))
        })
        .count();
    let mut timings = Vec::with_capacity(REPEATS);
    for _ in 0..REPEATS {
        let started = Instant::now();
        let candidate = black_box(build());
        let answer = refine_in(
            black_box(&column.values),
            &candidate,
            black_box(&query.targets),
        )?;
        black_box(answer);
        timings.push(started.elapsed().as_nanos() as f64);
    }
    timings.sort_by(f64::total_cmp);
    Ok(Observation {
        group: column.group.clone(),
        source: column.source.clone(),
        column: column.name.clone(),
        query: query.name.into(),
        plan: name.into(),
        rows: column.values.len(),
        metadata_bytes: plan.metadata_bytes,
        candidate_rows: plan.candidate_rows(column.values.len()),
        false_positive_blocks,
        matches: expected,
        p25_ns: timings[REPEATS / 4],
        median_ns: timings[REPEATS / 2],
        p75_ns: timings[REPEATS * 3 / 4],
    })
}

fn query_specs(values: &[Option<i64>]) -> Result<Vec<QuerySpec>, Box<dyn Error>> {
    let mut counts = BTreeMap::new();
    for &value in values.iter().flatten() {
        *counts.entry(value).or_insert(0_usize) += 1;
    }
    if counts.is_empty() {
        return Err("certificate study column contains no values".into());
    }
    let rare = counts
        .iter()
        .min_by_key(|(value, count)| (**count, **value))
        .map(|(&value, _)| value)
        .unwrap();
    let frequent = counts
        .iter()
        .max_by_key(|(value, count)| (**count, std::cmp::Reverse(**value)))
        .map(|(&value, _)| value)
        .unwrap();
    let absent = absent_value(&counts)?;
    let mut mixed = vec![rare, frequent, absent];
    mixed.sort_unstable();
    mixed.dedup();
    Ok(vec![
        QuerySpec {
            name: "eq_rare",
            targets: vec![rare],
        },
        QuerySpec {
            name: "eq_frequent",
            targets: vec![frequent],
        },
        QuerySpec {
            name: "eq_absent",
            targets: vec![absent],
        },
        QuerySpec {
            name: "in_mixed",
            targets: mixed,
        },
    ])
}

fn absent_value(counts: &BTreeMap<i64, usize>) -> Result<i64, Box<dyn Error>> {
    let keys = counts.keys().copied().collect::<Vec<_>>();
    for pair in keys.windows(2) {
        if pair[0].checked_add(1).is_some_and(|value| value < pair[1]) {
            return Ok(pair[0] + 1);
        }
    }
    keys.last()
        .and_then(|value| value.checked_add(1))
        .or_else(|| keys.first().and_then(|value| value.checked_sub(1)))
        .ok_or_else(|| "column spans the complete i64 domain".into())
}

fn fence_candidates(fence: &SparseFence, targets: &[i64]) -> CandidatePlan {
    let plans = targets
        .iter()
        .map(|&target| fence.probe_eq(target))
        .collect::<Vec<_>>();
    let block_rows = plans[0].block_rows;
    let mut blocks = plans
        .iter()
        .flat_map(|plan| plan.blocks.iter().copied())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    blocks.sort_unstable();
    CandidatePlan {
        blocks,
        block_rows,
        metadata_bytes: fence.bytes(),
        guarantee: OutputGuarantee::CandidateBitmap,
    }
}

fn write_cells(rows: &[Observation]) -> Result<(), Box<dyn Error>> {
    let mut csv = csv::Writer::from_path(format!("{RESULT_DIR}/cells.csv"))?;
    csv.write_record([
        "group",
        "source",
        "column",
        "query",
        "plan",
        "rows",
        "metadata_bytes",
        "candidate_rows",
        "candidate_fraction",
        "modeled_bytes",
        "false_positive_blocks",
        "matches",
        "p25_ns",
        "median_ns",
        "p75_ns",
    ])?;
    for row in rows {
        csv.write_record([
            row.group.clone(),
            row.source.clone(),
            row.column.clone(),
            row.query.clone(),
            row.plan.clone(),
            row.rows.to_string(),
            row.metadata_bytes.to_string(),
            row.candidate_rows.to_string(),
            format!("{:.8}", row.candidate_rows as f64 / row.rows as f64),
            (row.metadata_bytes + row.candidate_rows * 8).to_string(),
            row.false_positive_blocks.to_string(),
            row.matches.to_string(),
            format!("{:.1}", row.p25_ns),
            format!("{:.1}", row.median_ns),
            format!("{:.1}", row.p75_ns),
        ])?;
    }
    csv.flush()?;
    Ok(())
}

fn write_summary(rows: &[Observation]) -> Result<(), Box<dyn Error>> {
    let scan = rows
        .iter()
        .filter(|row| row.plan == "scan")
        .map(|row| ((&row.group, &row.source, &row.column, &row.query), row))
        .collect::<BTreeMap<_, _>>();
    let mut groups: BTreeMap<(&str, &str), Vec<&Observation>> = BTreeMap::new();
    for row in rows {
        groups.entry((&row.plan, &row.query)).or_default().push(row);
    }
    let mut csv = csv::Writer::from_path(format!("{RESULT_DIR}/summary.csv"))?;
    csv.write_record([
        "plan",
        "query",
        "cells",
        "sources",
        "candidate_fraction_p25",
        "candidate_fraction_median",
        "candidate_fraction_p75",
        "latency_over_scan_median",
        "modeled_bytes_over_scan_median",
        "cell_wins",
        "source_latency_ratio_p25",
        "source_latency_ratio_median",
        "source_latency_ratio_p75",
        "source_median_ci_low",
        "source_median_ci_high",
        "source_wins",
        "false_positive_blocks",
    ])?;
    for ((plan, query), group) in groups {
        let mut candidate = Vec::new();
        let mut latency = Vec::new();
        let mut bytes = Vec::new();
        let mut wins = 0;
        let mut sources = BTreeSet::new();
        let mut false_positives = 0;
        let mut source_cells: BTreeMap<(&str, &str), Vec<f64>> = BTreeMap::new();
        for row in &group {
            let baseline = scan[&(&row.group, &row.source, &row.column, &row.query)];
            candidate.push(row.candidate_rows as f64 / row.rows as f64);
            latency.push(row.median_ns / baseline.median_ns);
            source_cells
                .entry((&row.group, &row.source))
                .or_default()
                .push(row.median_ns / baseline.median_ns);
            bytes.push(
                (row.metadata_bytes + row.candidate_rows * 8) as f64 / (baseline.rows * 8) as f64,
            );
            wins += usize::from(row.median_ns < baseline.median_ns);
            sources.insert((&row.group, &row.source));
            false_positives += row.false_positive_blocks;
        }
        candidate.sort_by(f64::total_cmp);
        latency.sort_by(f64::total_cmp);
        bytes.sort_by(f64::total_cmp);
        let mut source_ratios = source_cells
            .into_values()
            .map(|mut values| {
                values.sort_by(f64::total_cmp);
                median(&values)
            })
            .collect::<Vec<_>>();
        source_ratios.sort_by(f64::total_cmp);
        let (ci_low, ci_high) = bootstrap_median_ci(&source_ratios, 2_000);
        let source_wins = source_ratios.iter().filter(|ratio| **ratio < 1.0).count();
        csv.write_record([
            plan.into(),
            query.into(),
            group.len().to_string(),
            sources.len().to_string(),
            format!("{:.6}", lower_empirical_quantile(&candidate, 1, 4)),
            format!("{:.6}", median(&candidate)),
            format!("{:.6}", lower_empirical_quantile(&candidate, 3, 4)),
            format!("{:.6}", median(&latency)),
            format!("{:.6}", median(&bytes)),
            wins.to_string(),
            format!("{:.6}", lower_empirical_quantile(&source_ratios, 1, 4)),
            format!("{:.6}", median(&source_ratios)),
            format!("{:.6}", lower_empirical_quantile(&source_ratios, 3, 4)),
            format!("{ci_low:.6}"),
            format!("{ci_high:.6}"),
            source_wins.to_string(),
            false_positives.to_string(),
        ])?;
    }
    csv.flush()?;
    Ok(())
}

/// Lower empirical quantile over an already-sorted sample: index
/// `(len - 1) * numerator / denominator`, truncated. This is not the
/// conventional nearest-rank rule -- it never rounds up, so for most sample
/// sizes it returns the order statistic at or below the requested position
/// and is biased low. It is used for the p25/p75 spread columns and for the
/// 2.5%/97.5% bootstrap endpoints, where a conservative envelope is
/// acceptable. It must not be used for a central estimate: see `median`.
fn lower_empirical_quantile(values: &[f64], numerator: usize, denominator: usize) -> f64 {
    values[(values.len() - 1) * numerator / denominator]
}

/// Standard median: the mean of the two central order statistics when the
/// sample size is even. Nearest-rank would bias the central estimate low,
/// and most sources contribute an even number of cells.
fn median(values: &[f64]) -> f64 {
    let middle = values.len() / 2;
    if values.len() % 2 == 1 {
        values[middle]
    } else {
        (values[middle - 1] + values[middle]) / 2.0
    }
}

fn bootstrap_median_ci(values: &[f64], iterations: usize) -> (f64, f64) {
    let mut state = 0x243f_6a88_85a3_08d3_u64 ^ values.len() as u64;
    let mut medians = Vec::with_capacity(iterations);
    let mut sample = Vec::with_capacity(values.len());
    for _ in 0..iterations {
        sample.clear();
        for _ in values {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            sample.push(values[state as usize % values.len()]);
        }
        sample.sort_by(f64::total_cmp);
        medians.push(median(&sample));
    }
    medians.sort_by(f64::total_cmp);
    (
        lower_empirical_quantile(&medians, 25, 1_000),
        lower_empirical_quantile(&medians, 975, 1_000),
    )
}

#[cfg(test)]
mod tests {
    use super::{bootstrap_median_ci, lower_empirical_quantile, median};

    #[test]
    fn median_averages_the_two_central_values_when_even() {
        assert_eq!(median(&[1.0, 2.0, 3.0, 4.0]), 2.5);
    }

    #[test]
    fn median_takes_the_centre_when_odd() {
        assert_eq!(median(&[1.0, 2.0, 3.0]), 2.0);
    }

    #[test]
    fn median_handles_duplicates() {
        assert_eq!(median(&[1.0, 1.0, 3.0, 3.0]), 2.0);
        assert_eq!(median(&[5.0, 5.0, 5.0, 5.0]), 5.0);
    }

    /// The defect this guards: `lower_empirical_quantile(_, 1, 2)` was used as
    /// the central estimate. It is biased low for even samples, so it must not
    /// agree with the median here.
    #[test]
    fn lower_quantile_at_one_half_is_not_the_median() {
        let sorted = [0.10, 0.20, 0.60, 0.80];
        assert_eq!(median(&sorted), 0.4);
        assert_eq!(lower_empirical_quantile(&sorted, 1, 2), 0.20);
        assert_ne!(median(&sorted), lower_empirical_quantile(&sorted, 1, 2));
    }

    /// The bootstrap must resample through the corrected estimator, otherwise
    /// the interval is centred on a statistic the paper never reports.
    #[test]
    fn bootstrap_interval_brackets_the_median_of_an_even_sample() {
        let sample = [0.10, 0.20, 0.60, 0.80];
        let (low, high) = bootstrap_median_ci(&sample, 2_000);
        let centre = median(&sample);
        assert!(low <= centre && centre <= high, "{low} {centre} {high}");
        // Every resample median is an average of two drawn values, so the
        // interval cannot escape the sample's own range.
        assert!(low >= sample[0] && high <= sample[sample.len() - 1]);
    }

    #[test]
    fn bootstrap_is_deterministic_for_a_fixed_sample() {
        let sample = [0.3, 0.9, 0.1, 0.7, 0.5, 0.2];
        assert_eq!(
            bootstrap_median_ci(&sample, 500),
            bootstrap_median_ci(&sample, 500)
        );
    }
}
