//! Selective SUM queries over paged columns: Witness pages planned from
//! their facts against Parquet pages pruned by stored statistics.

use std::error::Error;
use std::fs;
use std::hint::black_box;
use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use bytes::Bytes;
use witness::access_compiler::paged::{self, PageCounts, PagePolicy, PagedColumn, Planning};
use witness::access_compiler::{
    ClosureMode, ReadSession, derive_invariants, primitive_rule_fingerprint,
};
use witness::experiment::time_window::{parquet_full_sum, parquet_indexed_sum, parquet_table_file};
use witness::experiment::types::Column;

use crate::checker;
use crate::common::{
    PAGE_ROWS, benchmark, compact_menu, csv, direct_menu, page_rows, quote, truth,
};
use crate::corpus::{self, Pair};
use crate::workload::predicates;

const DEFAULT_RESULT_DIR: &str = "experiments/results/paged_study";
const SCALING_PAIRS: usize = 12;
const SCALING_THREADS: [usize; 7] = [1, 2, 4, 8, 16, 32, 64];

/// Page encodings for one column: `direct` keeps every field addressable,
/// `compact` may also compress a page in one Zstd frame.
pub struct Layouts {
    pub direct: PagedColumn,
    pub compact: PagedColumn,
}

#[derive(Clone, Copy, Debug)]
enum Arm {
    Witness { compact: bool, planning: Planning },
    ParquetIndexed { compressed: bool },
    ParquetFull { compressed: bool },
}

const ARMS: [Arm; 8] = [
    Arm::Witness {
        compact: false,
        planning: Planning::Facts,
    },
    Arm::Witness {
        compact: false,
        planning: Planning::ScanOnly,
    },
    Arm::Witness {
        compact: false,
        planning: Planning::StoredStatistics,
    },
    Arm::Witness {
        compact: true,
        planning: Planning::Facts,
    },
    Arm::ParquetIndexed { compressed: false },
    Arm::ParquetIndexed { compressed: true },
    Arm::ParquetFull { compressed: false },
    Arm::ParquetFull { compressed: true },
];

impl Arm {
    fn name(self) -> &'static str {
        match self {
            Self::Witness {
                compact: false,
                planning: Planning::Facts,
            } => "witness_direct_facts",
            Self::Witness {
                compact: false,
                planning: Planning::ScanOnly,
            } => "witness_direct_scan",
            Self::Witness {
                compact: true,
                planning: Planning::Facts,
            } => "witness_compact_facts",
            Self::Witness {
                compact: true,
                planning: Planning::ScanOnly,
            } => "witness_compact_scan",
            Self::Witness {
                compact: false,
                planning: Planning::StoredStatistics,
            } => "witness_direct_zonemap",
            Self::Witness {
                compact: true,
                planning: Planning::StoredStatistics,
            } => "witness_compact_zonemap",
            Self::ParquetIndexed { compressed: false } => "parquet_plain_indexed",
            Self::ParquetIndexed { compressed: true } => "parquet_zstd_indexed",
            Self::ParquetFull { compressed: false } => "parquet_plain_full",
            Self::ParquetFull { compressed: true } => "parquet_zstd_full",
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Outcome {
    sum: i128,
    matched: usize,
    counts: PageCounts,
    candidate_pages: Option<usize>,
}

pub fn run() -> Result<(), Box<dyn Error>> {
    let page_rows = page_rows()?;
    // A sensitivity run at another page size writes beside the main study
    // and measures queries and costs only.
    let main_run = page_rows == PAGE_ROWS;
    let result_dir = std::env::var("WITNESS_PAGED_RESULT_DIR").unwrap_or_else(|_| {
        if main_run {
            DEFAULT_RESULT_DIR.into()
        } else {
            format!("{DEFAULT_RESULT_DIR}_rows{page_rows}")
        }
    });
    fs::create_dir_all(&result_dir)?;
    let started = Instant::now();
    let (columns, pairs) = corpus::pairs()?;
    if pairs.len() < 100 {
        return Err(format!("paged corpus is unexpectedly small: {} pairs", pairs.len()).into());
    }
    let layouts = parallel(columns.len(), |index| {
        encode_layouts(&columns[index], page_rows)
    })?;
    let parquet = parallel(pairs.len(), |index| {
        let pair = &pairs[index];
        let (selector, value) = (&columns[pair.selector].values, &columns[pair.value].values);
        Ok((
            Bytes::from(parquet_table_file(selector, value, page_rows, false)?),
            Bytes::from(parquet_table_file(selector, value, page_rows, true)?),
        ))
    })?;
    if main_run {
        // The plain files, for engines outside this crate to query.
        let export = format!("{result_dir}/parquet");
        fs::create_dir_all(&export)?;
        for (pair_id, (plain, _)) in parquet.iter().enumerate() {
            fs::write(format!("{export}/pair_{pair_id}.parquet"), plain)?;
        }
    }
    eprintln!(
        "encoded {} columns and {} Parquet pairs in {:.0?}",
        columns.len(),
        pairs.len(),
        started.elapsed()
    );

    write_layouts(&result_dir, &columns, &pairs, &layouts, &parquet)?;
    let mut queries = csv(format!("{result_dir}/queries.csv"))?;
    writeln!(
        queries,
        "pair,predicate,requested,low,high,matched_rows,expected_sum,pages,arm,p25_ns,median_ns,p75_ns,skipped,accepted,searched,translated,scanned,constant_sums,candidate_pages,oracle_skipped,oracle_accepted"
    )?;
    for (pair_id, pair) in pairs.iter().enumerate() {
        let selector = &columns[pair.selector].values;
        let value = &columns[pair.value].values;
        for predicate in predicates(selector)? {
            let expected = truth(selector, value, predicate.low, predicate.high);
            let (oracle_skipped, oracle_accepted) =
                exact_zone_map(selector, predicate.low, predicate.high, page_rows);
            let pages = layouts[pair.selector].direct.pages().len();
            for arm in ARMS {
                let run = || {
                    run_arm(
                        arm,
                        pair,
                        &layouts,
                        &parquet[pair_id],
                        predicate.low,
                        predicate.high,
                        1,
                    )
                };
                let outcome = run()?;
                if (outcome.sum, outcome.matched) != expected {
                    return Err(format!(
                        "{} returned {:?} for pair {pair_id} {}, expected {expected:?}",
                        arm.name(),
                        (outcome.sum, outcome.matched),
                        predicate.label
                    )
                    .into());
                }
                let (p25, median, p75) = benchmark(run)?;
                let counts = outcome.counts;
                writeln!(
                    queries,
                    "{pair_id},{},{},{},{},{},{},{pages},{},{p25:.1},{median:.1},{p75:.1},{},{},{},{},{},{},{},{oracle_skipped},{oracle_accepted}",
                    predicate.label,
                    predicate.requested,
                    predicate.low,
                    predicate.high,
                    expected.1,
                    expected.0,
                    arm.name(),
                    counts.skipped,
                    counts.accepted,
                    counts.searched,
                    counts.translated,
                    counts.scanned,
                    counts.constant_sums,
                    outcome
                        .candidate_pages
                        .map(|pages| pages.to_string())
                        .unwrap_or_default(),
                )?;
            }
        }
        if pair_id % 50 == 49 {
            eprintln!("measured {} of {} pairs", pair_id + 1, pairs.len());
        }
    }
    queries.flush()?;
    bounds(&result_dir, &pairs, &layouts)?;
    costs(&result_dir, &columns, &pairs, &layouts)?;
    if main_run {
        scaling(&result_dir, &columns, &pairs, &layouts)?;
        checker::run(&result_dir, &columns, &pairs, &layouts)?;
    }
    fs::write(
        format!("{result_dir}/freeze.csv"),
        format!(
            "rule_fingerprint,page_rows,max_rows,columns,pairs\n{:#018x},{page_rows},{},{},{}\n",
            primitive_rule_fingerprint(),
            corpus::MAX_ROWS,
            columns.len(),
            pairs.len()
        ),
    )?;
    eprintln!("paged study finished in {:.0?}", started.elapsed());
    Ok(())
}

fn encode_layouts(column: &Column, page_rows: usize) -> Result<Layouts, String> {
    let direct = direct_menu();
    let compact = compact_menu();
    Ok(Layouts {
        direct: PagedColumn::from_values(&column.values, page_rows, &PagePolicy::smallest(direct))?,
        compact: PagedColumn::from_values(
            &column.values,
            page_rows,
            &PagePolicy::smallest(compact),
        )?,
    })
}

fn run_arm(
    arm: Arm,
    pair: &Pair,
    layouts: &[Layouts],
    parquet: &(Bytes, Bytes),
    low: i64,
    high: i64,
    threads: usize,
) -> Result<Outcome, String> {
    let upper = high
        .checked_add(1)
        .ok_or("predicate upper bound cannot be made exclusive")?;
    match arm {
        Arm::Witness { compact, planning } => {
            let pick = |index: usize| {
                if compact {
                    &layouts[index].compact
                } else {
                    &layouts[index].direct
                }
            };
            let (sum, matched, counts) = paged::sum_where(
                pick(pair.selector),
                pick(pair.value),
                low,
                high,
                planning,
                threads,
            )?;
            Ok(Outcome {
                sum,
                matched,
                counts,
                candidate_pages: None,
            })
        }
        Arm::ParquetIndexed { compressed } | Arm::ParquetFull { compressed } => {
            let bytes = if compressed {
                parquet.1.clone()
            } else {
                parquet.0.clone()
            };
            let result = if matches!(arm, Arm::ParquetIndexed { .. }) {
                parquet_indexed_sum(bytes, low, upper)?
            } else {
                parquet_full_sum(bytes, low, upper)?
            };
            Ok(Outcome {
                sum: result.sum,
                matched: result.matched_rows,
                counts: PageCounts::default(),
                candidate_pages: matches!(arm, Arm::ParquetIndexed { .. })
                    .then_some(result.candidate_pages),
            })
        }
    }
}

/// Pages an exact per-page minimum and maximum would skip and accept: the
/// most any zone map can decide, and so the yardstick for structural bounds.
fn exact_zone_map(
    selector: &[Option<i64>],
    low: i64,
    high: i64,
    page_rows: usize,
) -> (usize, usize) {
    let mut skipped = 0;
    let mut accepted = 0;
    for page in selector.chunks(page_rows) {
        let mut present = page.iter().flatten();
        let Some(&first) = present.next() else {
            skipped += 1;
            continue;
        };
        let (minimum, maximum) = present.fold((first, first), |(minimum, maximum), &value| {
            (minimum.min(value), maximum.max(value))
        });
        if maximum < low || minimum > high {
            skipped += 1;
        } else if low <= minimum && maximum <= high {
            accepted += 1;
        }
    }
    (skipped, accepted)
}

/// Every selector page's exact value range next to the tightest range its
/// bound facts give: the slack a structural bound carries.
fn bounds(result_dir: &str, pairs: &[Pair], layouts: &[Layouts]) -> Result<(), Box<dyn Error>> {
    let mut output = csv(format!("{result_dir}/bounds.csv"))?;
    writeln!(
        output,
        "pair,page,recipe,rows,non_null,stat_low,stat_high,bound_low,bound_high"
    )?;
    for (pair_id, pair) in pairs.iter().enumerate() {
        for (index, page) in layouts[pair.selector].direct.pages().iter().enumerate() {
            let field =
                |value: Option<i128>| value.map(|value| value.to_string()).unwrap_or_default();
            let bound = paged::derived_bounds(page)?;
            writeln!(
                output,
                "{pair_id},{index},{},{},{},{},{},{},{}",
                quote(&page.column.recipe.name()),
                page.rows(),
                page.non_null,
                field(page.stats.map(|stats| stats.0)),
                field(page.stats.map(|stats| stats.1)),
                field(bound.map(|bound| bound.0)),
                field(bound.map(|bound| bound.1)),
            )?;
        }
    }
    output.flush()?;
    Ok(())
}

/// What consulting facts costs, next to decoding: deriving every page's facts,
/// planning one query over every page (median over the predicates), and the
/// bytes that planning reads, against a scan-only pass over the same column.
fn costs(
    result_dir: &str,
    columns: &[Column],
    pairs: &[Pair],
    layouts: &[Layouts],
) -> Result<(), Box<dyn Error>> {
    let mut output = csv(format!("{result_dir}/costs.csv"))?;
    writeln!(
        output,
        "pair,pages,rows,derive_ns,plan_ns,plan_logical_bytes,scan_ns"
    )?;
    for (pair_id, pair) in pairs.iter().enumerate() {
        let column = &layouts[pair.selector].direct;
        let predicates = predicates(&columns[pair.selector].values)?;
        let (_, derive_ns, _) = benchmark(|| {
            for page in column.pages() {
                black_box(derive_invariants(
                    &page.column.decoder,
                    page.column.page.layout(),
                    page.column.page.invariants(),
                )?);
            }
            Ok(())
        })?;
        let mut plan_ns = Vec::with_capacity(predicates.len());
        for predicate in &predicates {
            let (_, median, _) = benchmark(|| {
                for page in column.pages() {
                    let mut session =
                        ReadSession::new_untracked(&page.column.page, ClosureMode::Selective);
                    black_box(paged::plan_between(
                        page,
                        &mut session,
                        predicate.low,
                        predicate.high,
                        Planning::Facts,
                    )?);
                }
                Ok(())
            })?;
            plan_ns.push(median);
        }
        plan_ns.sort_by(f64::total_cmp);
        let predicate = &predicates[predicates.len() / 2];
        let mut plan_bytes = 0;
        for page in column.pages() {
            let mut session = ReadSession::new(&page.column.page, ClosureMode::Selective);
            paged::plan_between(
                page,
                &mut session,
                predicate.low,
                predicate.high,
                Planning::Facts,
            )?;
            plan_bytes += session.metrics().logical_bytes;
        }
        let (_, scan_ns, _) = benchmark(|| {
            paged::between(column, predicate.low, predicate.high, Planning::ScanOnly, 1)
        })?;
        writeln!(
            output,
            "{pair_id},{},{},{derive_ns:.1},{:.1},{plan_bytes},{scan_ns:.1}",
            column.pages().len(),
            column.rows(),
            plan_ns[plan_ns.len() / 2],
        )?;
    }
    output.flush()?;
    Ok(())
}

/// Thread scaling on the largest pairs at 10% and 50% target selectivity.
fn scaling(
    result_dir: &str,
    columns: &[Column],
    pairs: &[Pair],
    layouts: &[Layouts],
) -> Result<(), Box<dyn Error>> {
    let mut order = (0..pairs.len()).collect::<Vec<_>>();
    order.sort_by_key(|&index| std::cmp::Reverse(columns[pairs[index].selector].values.len()));
    let mut output = csv(format!("{result_dir}/scaling.csv"))?;
    writeln!(output, "pair,predicate,arm,threads,p25_ns,median_ns,p75_ns")?;
    let no_parquet = (Bytes::new(), Bytes::new());
    for &pair_id in order.iter().take(SCALING_PAIRS) {
        let pair = &pairs[pair_id];
        for predicate in predicates(&columns[pair.selector].values)?
            .into_iter()
            .filter(|predicate| {
                predicate.label.contains("10pct") || predicate.label.contains("50pct")
            })
        {
            for planning in [Planning::Facts, Planning::ScanOnly] {
                let arm = Arm::Witness {
                    compact: false,
                    planning,
                };
                for threads in SCALING_THREADS {
                    let (p25, median, p75) = benchmark(|| {
                        run_arm(
                            arm,
                            pair,
                            layouts,
                            &no_parquet,
                            predicate.low,
                            predicate.high,
                            threads,
                        )
                    })?;
                    writeln!(
                        output,
                        "{pair_id},{},{},{threads},{p25:.1},{median:.1},{p75:.1}",
                        predicate.label,
                        arm.name()
                    )?;
                }
            }
        }
    }
    output.flush()?;
    Ok(())
}

fn write_layouts(
    result_dir: &str,
    columns: &[Column],
    pairs: &[Pair],
    layouts: &[Layouts],
    parquet: &[(Bytes, Bytes)],
) -> Result<(), Box<dyn Error>> {
    let mut output = csv(format!("{result_dir}/pairs.csv"))?;
    writeln!(
        output,
        "pair,group,source,selector,value,rows,pages,witness_direct_bytes,witness_compact_bytes,parquet_plain_bytes,parquet_zstd_bytes,selector_direct_recipes"
    )?;
    for (pair_id, pair) in pairs.iter().enumerate() {
        let selector = &layouts[pair.selector];
        let value = &layouts[pair.value];
        let mut recipes = std::collections::BTreeMap::<String, usize>::new();
        for page in selector.direct.pages() {
            *recipes.entry(page.column.recipe.name()).or_default() += 1;
        }
        let recipes = recipes
            .into_iter()
            .map(|(name, count)| format!("{name}:{count}"))
            .collect::<Vec<_>>()
            .join(";");
        writeln!(
            output,
            "{pair_id},{},{},{},{},{},{},{},{},{},{},{}",
            quote(&pair.group),
            quote(&pair.source),
            quote(&columns[pair.selector].name),
            quote(&columns[pair.value].name),
            selector.direct.rows(),
            selector.direct.pages().len(),
            selector.direct.bytes() + value.direct.bytes(),
            selector.compact.bytes() + value.compact.bytes(),
            parquet[pair_id].0.len(),
            parquet[pair_id].1.len(),
            quote(&recipes),
        )?;
    }
    output.flush()?;
    Ok(())
}

/// Runs `work` for every index on all cores, returning results in index order.
pub fn parallel<T, F>(count: usize, work: F) -> Result<Vec<T>, String>
where
    T: Send,
    F: Fn(usize) -> Result<T, String> + Sync,
{
    let next = AtomicUsize::new(0);
    let slots = Mutex::new((0..count).map(|_| None).collect::<Vec<_>>());
    let threads = std::thread::available_parallelism().map_or(1, usize::from);
    std::thread::scope(|scope| {
        for _ in 0..threads.min(count.max(1)) {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= count {
                        break;
                    }
                    let result = work(index);
                    slots.lock().unwrap()[index] = Some(result);
                }
            });
        }
    });
    slots
        .into_inner()
        .unwrap()
        .into_iter()
        .map(|slot| slot.unwrap_or_else(|| Err("work item was not run".into())))
        .collect()
}
