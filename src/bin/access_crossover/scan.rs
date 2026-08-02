use std::fs::File;
use std::hint::black_box;
use std::io::{BufWriter, Write};
use std::time::Instant;

use witness::access_compiler::EncodedColumn;

use super::measure::{
    CurveCell, PlanKind, QueryKind, ROWS, TOTAL_CASES, TRAINING_CASES, answer_checksum, bounds,
    execute,
};
use super::model::{FeatureMode, ModelSet};
use super::storage::{StorageBundle, StorageTier};

const SCAN_PAGES: usize = 2_048;
const SCAN_ALIGNMENT: usize = 65_536;
const REPEATS: usize = 3;

#[derive(Clone, Copy, Debug)]
enum Policy {
    AlwaysSelective,
    AlwaysFused,
    CostAware,
    Oracle,
}

impl Policy {
    const ALL: [Self; 4] = [
        Self::AlwaysSelective,
        Self::AlwaysFused,
        Self::CostAware,
        Self::Oracle,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::AlwaysSelective => "always_selective",
            Self::AlwaysFused => "always_fused",
            Self::CostAware => "cost_model_preflight_free",
            Self::Oracle => "microcurve_oracle",
        }
    }
}

#[derive(Clone, Copy)]
struct Task {
    page_index: usize,
    case: usize,
    query: QueryKind,
    rows: witness::access_compiler::Span,
    low: i64,
    high: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ScanResult {
    checksum: i128,
    logical_bytes: usize,
    delivered_bytes: usize,
    transferred_bytes: usize,
    transfer_operations: usize,
    frames_decoded: usize,
    decoded_rows: usize,
}

#[derive(Clone, Copy)]
struct SystemCounters {
    read_bytes: u64,
    read_chars: u64,
    minor_faults: i64,
    major_faults: i64,
}

impl SystemCounters {
    fn delta(self, before: Self) -> Self {
        Self {
            read_bytes: self.read_bytes.saturating_sub(before.read_bytes),
            read_chars: self.read_chars.saturating_sub(before.read_chars),
            minor_faults: self.minor_faults.saturating_sub(before.minor_faults),
            major_faults: self.major_faults.saturating_sub(before.major_faults),
        }
    }
}

struct TimedSample {
    ns: f64,
    result: ScanResult,
    counters: SystemCounters,
}

pub fn run(
    columns: &[EncodedColumn],
    cells: &[CurveCell],
    models: &ModelSet,
    result_dir: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let page_cases = (0..SCAN_PAGES)
        .map(|page| TRAINING_CASES + page % (TOTAL_CASES - TRAINING_CASES))
        .collect::<Vec<_>>();
    let pages = page_cases
        .iter()
        .map(|&case| columns[case].page.bytes())
        .collect::<Vec<_>>();
    let bundle = StorageBundle::build(
        format!("{result_dir}/scan_bundle.acp"),
        &pages,
        SCAN_ALIGNMENT,
    )?;
    let tasks = tasks(columns, &page_cases)?;
    let mut output = BufWriter::new(File::create(format!("{result_dir}/scan.csv"))?);
    writeln!(
        output,
        "tier,policy,pages,logical_rows,selected_rows,bundle_bytes,planning_ns,selective_pages,fused_pages,p25_ns,median_ns,p75_ns,logical_bytes,delivered_bytes,transferred_bytes,transfer_operations,frames_decoded,decoded_rows,checksum,os_read_bytes,os_read_chars,minor_faults,major_faults"
    )?;
    let mut expected = None;
    for tier in [
        StorageTier::MmapHot,
        StorageTier::BufferedHot,
        StorageTier::BufferedCold,
    ] {
        for policy in Policy::ALL {
            let planning_started = Instant::now();
            let plans = tasks
                .iter()
                .map(|task| choose(policy, tier, task, cells, models))
                .collect::<Vec<_>>();
            let planning_ns = planning_started.elapsed().as_nanos();
            let selective_pages = plans
                .iter()
                .filter(|&&plan| plan == PlanKind::Selective)
                .count();
            let mut samples = Vec::with_capacity(REPEATS);
            for _ in 0..REPEATS {
                if tier.is_cold() {
                    bundle.evict_all()?;
                }
                let counters = system_counters()?;
                let started = Instant::now();
                let result = black_box(scan_once(columns, &tasks, &plans, tier, &bundle)?);
                let ns = started.elapsed().as_nanos() as f64;
                samples.push(TimedSample {
                    ns,
                    result,
                    counters: system_counters()?.delta(counters),
                });
            }
            if samples
                .iter()
                .any(|sample| sample.result != samples[0].result)
            {
                return Err("multipage scan repetitions disagree".into());
            }
            if let Some(expected) = expected {
                if samples[0].result.checksum != expected {
                    return Err("multipage scan policies disagree".into());
                }
            } else {
                expected = Some(samples[0].result.checksum);
            }
            samples.sort_by(|left, right| left.ns.total_cmp(&right.ns));
            let sample = &samples[1];
            let result = sample.result;
            let selected_rows = tasks.iter().map(|task| task.rows.len()).sum::<usize>();
            writeln!(
                output,
                "{},{},{},{},{},{},{},{},{},{:.1},{:.1},{:.1},{},{},{},{},{},{},{},{},{},{},{}",
                tier.name(),
                policy.name(),
                SCAN_PAGES,
                SCAN_PAGES * ROWS,
                selected_rows,
                bundle.file_len(),
                planning_ns,
                selective_pages,
                SCAN_PAGES - selective_pages,
                samples[0].ns,
                sample.ns,
                samples[2].ns,
                result.logical_bytes,
                result.delivered_bytes,
                result.transferred_bytes,
                result.transfer_operations,
                result.frames_decoded,
                result.decoded_rows,
                result.checksum,
                sample.counters.read_bytes,
                sample.counters.read_chars,
                sample.counters.minor_faults,
                sample.counters.major_faults,
            )?;
            println!("scan {:<19} {}", tier.name(), policy.name());
        }
    }
    output.flush()?;
    Ok(())
}

fn system_counters() -> Result<SystemCounters, String> {
    let io = std::fs::read_to_string("/proc/self/io")
        .map_err(|error| format!("cannot read /proc/self/io: {error}"))?;
    let value = |name: &str| -> Result<u64, String> {
        io.lines()
            .find_map(|line| line.strip_prefix(name))
            .ok_or_else(|| format!("/proc/self/io lacks {name}"))?
            .trim()
            .parse()
            .map_err(|error| format!("invalid /proc/self/io counter: {error}"))
    };
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage initializes the supplied rusage object on success.
    let status = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if status != 0 {
        return Err(format!(
            "getrusage failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: successful getrusage initialized `usage` above.
    let usage = unsafe { usage.assume_init() };
    Ok(SystemCounters {
        read_bytes: value("read_bytes:")?,
        read_chars: value("rchar:")?,
        minor_faults: usage.ru_minflt,
        major_faults: usage.ru_majflt,
    })
}

fn tasks(columns: &[EncodedColumn], page_cases: &[usize]) -> Result<Vec<Task>, String> {
    let widths = [1, 64, ROWS / 100, ROWS / 10, ROWS / 2, ROWS];
    page_cases
        .iter()
        .enumerate()
        .map(|(page_index, &case)| {
            let width = widths[page_index % widths.len()];
            let start = (ROWS - width) / 2;
            let (low, high) = bounds(&columns[case].truth);
            Ok(Task {
                page_index,
                case,
                query: if page_index.is_multiple_of(2) {
                    QueryKind::Sum
                } else {
                    QueryKind::Between
                },
                rows: witness::access_compiler::Span::new(start, start + width)?,
                low,
                high,
            })
        })
        .collect()
}

fn choose(
    policy: Policy,
    tier: StorageTier,
    task: &Task,
    cells: &[CurveCell],
    models: &ModelSet,
) -> PlanKind {
    match policy {
        Policy::AlwaysSelective => PlanKind::Selective,
        Policy::AlwaysFused => PlanKind::Fused,
        Policy::CostAware => {
            let selective = cell(cells, task, tier, PlanKind::Selective);
            let fused = cell(cells, task, tier, PlanKind::Fused);
            models.choose(
                FeatureMode::PreflightFree,
                tier,
                task.query,
                selective.preflight_free_features(),
                fused.preflight_free_features(),
            )
        }
        Policy::Oracle => {
            let selective = cell(cells, task, tier, PlanKind::Selective);
            let fused = cell(cells, task, tier, PlanKind::Fused);
            if selective.median_ns <= fused.median_ns {
                PlanKind::Selective
            } else {
                PlanKind::Fused
            }
        }
    }
}

fn cell<'a>(
    cells: &'a [CurveCell],
    task: &Task,
    tier: StorageTier,
    plan: PlanKind,
) -> &'a CurveCell {
    cells
        .iter()
        .find(|cell| {
            cell.case == task.case
                && cell.query == task.query
                && cell.rows.len() == task.rows.len()
                && cell.tier == tier
                && cell.plan == plan
        })
        .expect("scan workload is absent from crossover curve")
}

fn scan_once(
    columns: &[EncodedColumn],
    tasks: &[Task],
    plans: &[PlanKind],
    tier: StorageTier,
    bundle: &StorageBundle,
) -> Result<ScanResult, String> {
    let mut result = ScanResult {
        checksum: 0,
        logical_bytes: 0,
        delivered_bytes: 0,
        transferred_bytes: 0,
        transfer_operations: 0,
        frames_decoded: 0,
        decoded_rows: 0,
    };
    for (task, &plan) in tasks.iter().zip(plans) {
        let execution = execute(
            task.case,
            &columns[task.case],
            task.query,
            task.rows,
            task.low,
            task.high,
            plan,
            tier,
            bundle,
            task.page_index,
        )?;
        result.checksum = result
            .checksum
            .checked_add(answer_checksum(&execution.answer))
            .ok_or("scan checksum overflow")?;
        result.logical_bytes += execution.metrics.logical_bytes;
        result.delivered_bytes += execution.metrics.delivered_bytes;
        result.transferred_bytes += execution.metrics.transferred_bytes;
        result.transfer_operations += execution.metrics.transfer_operations;
        result.frames_decoded += execution.metrics.frames_decoded;
        result.decoded_rows += execution.decoded_rows;
    }
    Ok(result)
}
