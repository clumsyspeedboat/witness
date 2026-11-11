//! Opt-in prepared/cost-planning study; no canonical result is overwritten.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs;
use std::hint::black_box;
use std::io::Write;
use std::time::Instant;

use witness::access_compiler::native_file;
use witness::access_compiler::paged::{self, PagePolicy, PagedColumn, Planning, PreparedSum};
use witness::access_compiler::primitive_rule_fingerprint;
use witness::experiment::evidence_layout::{Compression, compression_menu};
use witness::experiment::types::Column;

#[allow(dead_code)]
#[path = "paged_study/common.rs"]
mod common;
#[path = "paged_study/corpus.rs"]
mod corpus;
#[allow(dead_code)]
#[path = "predicate_pipeline/workload.rs"]
mod workload;

const ROUNDS: usize = 9;
const TARGET_NS: u128 = 2_000_000;
const REPEATED: [usize; 6] = [1, 2, 4, 8, 16, 32];
const ARMS: [&str; 11] = [
    "facts_default",
    "facts_prepared",
    "facts_regions",
    "facts_regions_prepared",
    "zonemapplus_default",
    "zonemapplus_prepared",
    "zonemapplus_regions",
    "zonemapplus_regions_prepared",
    "facts_cost_prepared",
    "zonemapplus_cost_prepared",
    "scan_prepared",
];
const TOTAL_ARMS: [usize; 6] = [0, 1, 3, 4, 5, 7];

fn main() -> Result<(), Box<dyn Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() > 1
        || args
            .iter()
            .any(|arg| !["--smoke", "--pilot", "--all"].contains(&arg.as_str()))
    {
        return Err("usage: query_optimization_study [--smoke | --pilot | --all]".into());
    }
    if std::env::var_os("WITNESS_PAGED_MAX_PAIRS").is_some()
        || std::env::var_os("WITNESS_PAGE_ROWS").is_some()
    {
        return Err("unset page/pair overrides: this study freezes its workload".into());
    }
    let smoke = args.first().is_some_and(|arg| arg == "--smoke");
    let all = args.first().is_some_and(|arg| arg == "--all");
    let mode = if smoke {
        "smoke"
    } else if all {
        "all"
    } else {
        "pilot"
    };
    let selection = if smoke {
        "three_synthetic_pairs"
    } else if all {
        "all_canonical_pairs"
    } else {
        "8_development_16_heldout_sources_one_pair_each"
    };
    let directory = std::env::var("WITNESS_QUERY_OPTIMIZATION_RESULT_DIR")
        .map_err(|_| "set WITNESS_QUERY_OPTIMIZATION_RESULT_DIR to a fresh directory")?;
    fs::create_dir_all(&directory)?;
    let mut freeze = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(format!("{directory}/freeze.csv"))?;
    writeln!(
        freeze,
        "key,value\nstatus,started\nrule_fingerprint,{:016x}\nmode,{mode}\nrounds,{ROUNDS}\nrow_cap,1048576\npage_rows,1024|16384\npartition,fnv1a_group_source_mod3_zero_is_development\nselection,{selection}\nwarm_scope,resident_after_initialization_regions_and_preparation\ntotal_scope,resident_bytes_to_answer_including_both_columns_optional_regions_preparation_and_teardown\nrepetitions,1|2|4|8|16|32\narm_count,{}\ncost_gate,no_measured_query_tuning",
        primitive_rule_fingerprint(),
        ARMS.len()
    )?;
    writeln!(
        freeze,
        "decision_protocol,prepared_selective_synthetic_at_least10pct_gain_heldout_direct_sourceweighted_ratio_at_most1.05_cost_heldout_geo_at_most0.95_sourceweighted_at_most1_regressions20pct_at_most5pct\nbootstrap_seed,20261025\nbootstrap_replicates,1000"
    )?;
    freeze.flush()?;
    let started = Instant::now();
    let (columns, pairs) = if smoke {
        synthetic(false)
    } else {
        corpus::pairs()?
    };
    let selected = if smoke || all {
        (0..pairs.len()).collect()
    } else {
        select_pairs(&pairs)
    };
    let mut shapes = common::csv(format!("{directory}/shapes.csv"))?;
    let mut queries = common::csv(format!("{directory}/queries.csv"))?;
    let mut totals = common::csv(format!("{directory}/totals.csv"))?;
    writeln!(
        shapes,
        "case,split,group,source,selector,value,page_rows,layout,rows,pages,global_ordered,regions,certified_pages,multi_page_regions,longest_region,selector_file_bytes,value_file_bytes,region_entry_bytes,init_p25_ns,init_median_ns,init_p75_ns,region_p25_ns,region_median_ns,region_p75_ns,prepare_p25_ns,prepare_median_ns,prepare_p75_ns,region_prepare_p25_ns,region_prepare_median_ns,region_prepare_p75_ns,predicates,rule_fingerprint"
    )?;
    writeln!(
        queries,
        "case,page_rows,layout,predicate,requested,low,high,expected_sum,expected_count,arm,p25_ns,median_ns,p75_ns,sum,matched,candidate_pages,skipped,accepted,searched,translated,scanned,rule_fingerprint"
    )?;
    writeln!(
        totals,
        "case,page_rows,layout,predicate,repetitions,arm,p25_ns,median_ns,p75_ns,sum,matched,rule_fingerprint"
    )?;
    let mut cases = 0;
    for (slot, &pair_id) in selected.iter().enumerate() {
        let pair = &pairs[pair_id];
        let split = if smoke { "synthetic" } else { partition(pair) };
        for page_rows in [1_024, 16_384] {
            for layout in ["direct", "whole", "payload", "compact"] {
                // Framing comparisons need only the canonical page size. The
                // smaller direct pages expose naturally surviving seams.
                if page_rows == 1_024 && layout != "direct" {
                    continue;
                }
                let policy = policy(layout)?;
                measure_case(
                    pair_id,
                    split,
                    pair,
                    &columns[pair.selector],
                    &columns[pair.value],
                    page_rows,
                    layout,
                    &policy,
                    &mut shapes,
                    &mut queries,
                    &mut totals,
                )?;
                cases += 1;
            }
        }
        shapes.flush()?;
        queries.flush()?;
        totals.flush()?;
        eprintln!(
            "query optimizations: pair {}/{} {}/{} ({split}), {:.1?}",
            slot + 1,
            selected.len(),
            pair.source,
            columns[pair.selector].name,
            started.elapsed()
        );
    }
    // Controlled scaling is separate from every natural-source statistic.
    // Smoke keeps it small; pilot includes the original 4,096-page case.
    let (synthetic_columns, _) = synthetic(true);
    let scaling_sizes: &[usize] = if smoke { &[64] } else { &[64, 512, 4_096] };
    for &pages in scaling_sizes {
        let rows = pages * 1_024;
        let mut selector = synthetic_columns[0].clone();
        let mut value = synthetic_columns[1].clone();
        selector.source = format!("four_batches_{pages}");
        value.source = selector.source.clone();
        selector.values = (0..rows).map(|r| Some((r % (rows / 4)) as i64)).collect();
        value.values = (0..rows).map(|r| Some((r % 997) as i64 - 500)).collect();
        let scaled_pair = corpus::Pair {
            group: "synthetic".into(),
            source: selector.source.clone(),
            selector: 0,
            value: 1,
        };
        measure_case(
            pairs.len() + pages,
            "synthetic",
            &scaled_pair,
            &selector,
            &value,
            1_024,
            "direct",
            &policy("direct")?,
            &mut shapes,
            &mut queries,
            &mut totals,
        )?;
        cases += 1;
        shapes.flush()?;
        queries.flush()?;
        totals.flush()?;
    }
    let sources = selected
        .iter()
        .map(|&id| (&pairs[id].group, &pairs[id].source))
        .collect::<BTreeSet<_>>()
        .len();
    writeln!(
        freeze,
        "pairs,{}\nsources,{sources}\ncases,{cases}\nstatus,complete\nelapsed_seconds,{:.3}",
        selected.len(),
        started.elapsed().as_secs_f64()
    )?;
    freeze.flush()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn measure_case(
    case: usize,
    split: &str,
    pair: &corpus::Pair,
    selector: &Column,
    value: &Column,
    page_rows: usize,
    layout: &str,
    policy: &PagePolicy,
    shapes: &mut impl Write,
    queries: &mut impl Write,
    totals: &mut impl Write,
) -> Result<(), Box<dyn Error>> {
    let fingerprint = primitive_rule_fingerprint();
    let encoded_s = PagedColumn::from_values(&selector.values, page_rows, policy)?;
    let encoded_v = PagedColumn::from_values(&value.values, page_rows, policy)?;
    let bytes_s = native_file::to_bytes(&encoded_s)?;
    let bytes_v = native_file::to_bytes(&encoded_v)?;
    drop(encoded_s);
    drop(encoded_v);
    let mut init_samples = Vec::new();
    let mut region_samples = Vec::new();
    for _ in 0..ROUNDS {
        let clock = Instant::now();
        let a = native_file::from_bytes(black_box(&bytes_s))?;
        let b = native_file::from_bytes(black_box(&bytes_v))?;
        init_samples.push(clock.elapsed().as_nanos() as f64);
        black_box(&b);
        let mut a = a.column;
        let clock = Instant::now();
        a.enable_ordered_regions();
        region_samples.push(clock.elapsed().as_nanos() as f64);
        black_box(&a);
    }
    let regular = native_file::from_bytes(&bytes_s)?.column;
    let aggregate = native_file::from_bytes(&bytes_v)?.column;
    let mut regional = native_file::from_bytes(&bytes_s)?.column;
    regional.enable_ordered_regions();
    let (mut prepare_samples, mut region_prepare_samples) = (Vec::new(), Vec::new());
    for _ in 0..ROUNDS {
        let clock = Instant::now();
        black_box(PreparedSum::new(&regular, &aggregate)?);
        prepare_samples.push(clock.elapsed().as_nanos() as f64);
        let clock = Instant::now();
        black_box(PreparedSum::new(&regional, &aggregate)?);
        region_prepare_samples.push(clock.elapsed().as_nanos() as f64);
    }
    let prepared = PreparedSum::new(&regular, &aggregate)?;
    let prepared_regions = PreparedSum::new(&regional, &aggregate)?;
    let predicates = workload::predicates(&selector.values)?;
    let regions = regional.ordered_regions();
    let certified: usize = regions.iter().map(|r| r.len()).sum();
    let multi = regions.iter().filter(|r| r.len() > 1).count();
    let longest = regions.iter().map(|r| r.len()).max().unwrap_or(0);
    let init = quartiles(&mut init_samples);
    let build = quartiles(&mut region_samples);
    let prep = quartiles(&mut prepare_samples);
    let regional_prep = quartiles(&mut region_prepare_samples);
    writeln!(
        shapes,
        "{case},{split},{},{},{},{},{page_rows},{layout},{},{},{},{},{certified},{multi},{longest},{},{},{},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1},{},{fingerprint:016x}",
        common::quote(&pair.group),
        common::quote(&pair.source),
        common::quote(&selector.name),
        common::quote(&value.name),
        regular.rows(),
        regular.pages().len(),
        regular.ordered(),
        regions.len(),
        bytes_s.len(),
        bytes_v.len(),
        std::mem::size_of_val(regions),
        init.0,
        init.1,
        init.2,
        build.0,
        build.1,
        build.2,
        prep.0,
        prep.1,
        prep.2,
        regional_prep.0,
        regional_prep.1,
        regional_prep.2,
        predicates.len()
    )?;
    for (predicate_id, predicate) in predicates.iter().enumerate() {
        let expected = common::truth(
            &selector.values,
            &value.values,
            predicate.low,
            predicate.high,
        );
        let run = |arm| -> Result<_, String> {
            let planning = planner(arm);
            if is_prepared(arm) {
                if uses_regions(arm) {
                    prepared_regions.execute(predicate.low, predicate.high, planning, 1)
                } else {
                    prepared.execute(predicate.low, predicate.high, planning, 1)
                }
            } else {
                let column = if uses_regions(arm) {
                    &regional
                } else {
                    &regular
                };
                paged::sum_where(
                    column,
                    &aggregate,
                    predicate.low,
                    predicate.high,
                    planning,
                    1,
                )
            }
        };
        let (samples, outcomes) = benchmark(ARMS.len(), |arm| {
            let result = run(arm)?;
            if (result.0, result.1) != expected {
                return Err("warm optimization answer mismatch".into());
            }
            Ok(result)
        })?;
        for (arm, mut samples) in samples.into_iter().enumerate() {
            let timing = quartiles(&mut samples);
            let (sum, matched, counts) = outcomes[arm];
            let column = if uses_regions(arm) {
                &regional
            } else {
                &regular
            };
            let candidates = if planner(arm).uses_shortcuts() {
                column
                    .candidate_page_ranges(i128::from(predicate.low), i128::from(predicate.high))?
                    .iter()
                    .map(|r| r.len())
                    .sum::<usize>()
            } else {
                column.pages().len()
            };
            writeln!(
                queries,
                "{case},{page_rows},{layout},{predicate_id},{},{},{},{},{},{},{:.1},{:.1},{:.1},{sum},{matched},{candidates},{},{},{},{},{},{fingerprint:016x}",
                common::quote(&predicate.requested),
                predicate.low,
                predicate.high,
                expected.0,
                expected.1,
                ARMS[arm],
                timing.0,
                timing.1,
                timing.2,
                counts.skipped,
                counts.accepted,
                counts.searched,
                counts.translated,
                counts.scanned
            )?;
        }
        if layout == "direct" {
            for repetitions in REPEATED {
                let (samples, outcomes) = benchmark_once(TOTAL_ARMS.len(), |slot| {
                    let arm = TOTAL_ARMS[slot];
                    // These measured totals include reconstructing BOTH columns,
                    // optional certificate construction, preparation, and Q queries.
                    let mut a = native_file::from_bytes(black_box(&bytes_s))?.column;
                    let b = native_file::from_bytes(black_box(&bytes_v))?.column;
                    if uses_regions(arm) {
                        a.enable_ordered_regions();
                    }
                    let context = if is_prepared(arm) {
                        Some(PreparedSum::new(&a, &b)?)
                    } else {
                        None
                    };
                    let mut answer = None;
                    for _ in 0..repetitions {
                        let result = if let Some(context) = &context {
                            context.execute(predicate.low, predicate.high, planner(arm), 1)?
                        } else {
                            paged::sum_where(
                                &a,
                                &b,
                                predicate.low,
                                predicate.high,
                                planner(arm),
                                1,
                            )?
                        };
                        if (result.0, result.1) != expected {
                            return Err("end-to-end optimization answer mismatch".into());
                        }
                        answer = Some(black_box(result));
                    }
                    Ok(answer.expect("positive repetitions"))
                })?;
                for (slot, mut samples) in samples.into_iter().enumerate() {
                    let timing = quartiles(&mut samples);
                    let (sum, matched, _) = outcomes[slot];
                    writeln!(
                        totals,
                        "{case},{page_rows},{layout},{predicate_id},{repetitions},{},{:.1},{:.1},{:.1},{sum},{matched},{fingerprint:016x}",
                        ARMS[TOTAL_ARMS[slot]], timing.0, timing.1, timing.2
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn planner(arm: usize) -> Planning {
    match arm {
        0..=3 => Planning::Facts,
        4..=7 => Planning::StoredStatisticsPlus,
        8 => Planning::FactsCost,
        9 => Planning::StoredStatisticsPlusCost,
        _ => Planning::ScanOnly,
    }
}
fn is_prepared(arm: usize) -> bool {
    ![0, 2, 4, 6].contains(&arm)
}
fn uses_regions(arm: usize) -> bool {
    [2, 3, 6, 7].contains(&arm)
}

fn policy(layout: &str) -> Result<PagePolicy, String> {
    let menu = match layout {
        "direct" => common::direct_menu(),
        "whole" => compression_menu(&common::direct_menu(), Compression::Whole, false)?,
        "payload" => compression_menu(&common::direct_menu(), Compression::Whole, true)?,
        "compact" => common::compact_menu(),
        _ => return Err("unknown layout".into()),
    };
    Ok(PagePolicy::smallest(menu))
}

type Outcome = (i128, usize, paged::PageCounts);
type Samples = Vec<Vec<f64>>;
fn benchmark<F>(arms: usize, mut run: F) -> Result<(Samples, Vec<Outcome>), String>
where
    F: FnMut(usize) -> Result<Outcome, String>,
{
    let mut iterations = vec![1; arms];
    let mut outcomes = Vec::with_capacity(arms);
    for (arm, count) in iterations.iter_mut().enumerate() {
        outcomes.push(run(arm)?);
        let clock = Instant::now();
        black_box(run(arm)?);
        *count = (TARGET_NS / clock.elapsed().as_nanos().max(1)).clamp(1, 1000) as usize;
    }
    let mut samples = vec![Vec::with_capacity(ROUNDS); arms];
    for round in 0..ROUNDS {
        for offset in 0..arms {
            let arm = (offset + round) % arms;
            let clock = Instant::now();
            for _ in 0..iterations[arm] {
                black_box(run(arm)?);
            }
            samples[arm].push(clock.elapsed().as_nanos() as f64 / iterations[arm] as f64);
        }
    }
    Ok((samples, outcomes))
}
fn benchmark_once<F>(arms: usize, mut run: F) -> Result<(Samples, Vec<Outcome>), String>
where
    F: FnMut(usize) -> Result<Outcome, String>,
{
    let mut samples = vec![Vec::with_capacity(ROUNDS); arms];
    let mut outcomes = Vec::with_capacity(arms);
    for arm in 0..arms {
        outcomes.push(run(arm)?);
    }
    for round in 0..ROUNDS {
        for offset in 0..arms {
            let arm = (offset + round) % arms;
            let clock = Instant::now();
            black_box(run(arm)?);
            samples[arm].push(clock.elapsed().as_nanos() as f64);
        }
    }
    Ok((samples, outcomes))
}
fn quartiles(samples: &mut [f64]) -> (f64, f64, f64) {
    samples.sort_by(f64::total_cmp);
    let at = |q| samples[(samples.len() - 1) * q / 4];
    (at(1), at(2), at(3))
}
fn hash(pair: &corpus::Pair) -> u64 {
    pair.group
        .bytes()
        .chain([0])
        .chain(pair.source.bytes())
        .fold(0xcbf2_9ce4_8422_2325, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        })
}
fn partition(pair: &corpus::Pair) -> &'static str {
    if hash(pair).is_multiple_of(3) {
        "development"
    } else {
        "heldout"
    }
}
fn select_pairs(pairs: &[corpus::Pair]) -> Vec<usize> {
    let mut sources = BTreeMap::<(&str, &str), Vec<usize>>::new();
    for (id, pair) in pairs.iter().enumerate() {
        sources
            .entry((&pair.group, &pair.source))
            .or_default()
            .push(id);
    }
    let mut selected = Vec::new();
    for (split, limit) in [("development", 8), ("heldout", 16)] {
        let eligible = sources
            .values()
            .filter(|ids| partition(&pairs[ids[0]]) == split)
            .collect::<Vec<_>>();
        let count = limit.min(eligible.len());
        for slot in 0..count {
            let ids = eligible[slot * eligible.len() / count];
            selected.push(ids[(hash(&pairs[ids[0]]) as usize / 3) % ids.len()]);
        }
    }
    selected
}
fn synthetic(scaling: bool) -> (Vec<Column>, Vec<corpus::Pair>) {
    let mut columns = Vec::new();
    let mut pairs = Vec::new();
    let cases = if scaling { 1 } else { 3 };
    for case in 0..cases {
        let rows = 65_536;
        let values = (0..rows)
            .map(|row| match case {
                0 => Some(row as i64),
                1 => (row % 11 != 0).then_some((row % 127) as i64 * 10),
                _ => Some(((row * 7919) % 65521) as i64),
            })
            .collect();
        let make = |name: &str, values| Column {
            group: "synthetic".into(),
            source: format!("case{case}"),
            name: name.into(),
            mode: "exact".into(),
            values,
            float_values: None,
            exact_i64: true,
        };
        let selector = columns.len();
        columns.push(make("selector", values));
        columns.push(make(
            "value",
            (0..rows)
                .map(|r| (r % 13 != 0).then_some((r % 997) as i64 - 500))
                .collect(),
        ));
        pairs.push(corpus::Pair {
            group: "synthetic".into(),
            source: format!("case{case}"),
            selector,
            value: selector + 1,
        });
    }
    (columns, pairs)
}

#[cfg(test)]
mod partition_hash_tests {
    use super::{corpus, hash};

    #[test]
    fn source_partition_hash_matches_fnv1a_known_vector() {
        let pair = corpus::Pair {
            group: "tpch".into(),
            source: "lineitem".into(),
            selector: 0,
            value: 1,
        };
        assert_eq!(hash(&pair), 0x20ca_6864_c1d4_7ea3);
    }
}
