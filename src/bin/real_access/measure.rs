use std::fs::{self, File};
use std::hint::black_box;
use std::io::{BufWriter, Write};
use std::time::Instant;

use witness::access_compiler::{
    Answer, ClosureMode, EncodedColumn, Execution, ReadSession, Span, primitive_rule_fingerprint,
};
use witness::experiment::access_real::{RealAccessColumn, real_access_columns};

use super::scan;
use super::storage::{StorageBundle, StorageTier};
use crate::generated;

const RESULT_DIR: &str = "experiments/results/real_access";
const REPEATS: usize = 5;
const TARGET_NS: u128 = 500_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryKind {
    Sum,
    Between,
}

impl QueryKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Sum => "SUM",
            Self::Between => "BETWEEN",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanKind {
    Selective,
    Fused,
}

impl PlanKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Selective => "selective_closure",
            Self::Fused => "broad_fused",
        }
    }
}

#[derive(Clone, Copy)]
pub struct Task {
    pub page_index: usize,
    pub case: usize,
    pub query: QueryKind,
    pub rows: Span,
    pub low: i64,
    pub high: i64,
    pub plan: PlanKind,
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    if generated::RULE_FINGERPRINT != primitive_rule_fingerprint() {
        return Err("real generated source is stale; run generate_real_access".into());
    }
    fs::create_dir_all(RESULT_DIR)?;
    let columns = real_access_columns()?;
    if generated::CASE_COUNT != columns.len() {
        return Err("real generated source has the wrong column count".into());
    }
    verify_pages(&columns)?;
    let pages = columns
        .iter()
        .map(|column| column.size_selected.page.bytes())
        .collect::<Vec<_>>();
    let bundle = StorageBundle::build(format!("{RESULT_DIR}/query_bundle.acp"), &pages, 4096)?;
    let mut output = BufWriter::new(File::create(format!("{RESULT_DIR}/queries.csv"))?);
    let mut distribution = BufWriter::new(File::create(format!(
        "{RESULT_DIR}/query_distribution.csv"
    ))?);
    writeln!(
        output,
        "column,group,source,name,recipe,query,rows,selectivity,tier,chosen_plan,selective_ns,fused_ns,chosen_ns,logical_bytes,delivered_bytes,transferred_bytes,transfer_operations,frames_decoded,file_bytes"
    )?;
    writeln!(distribution, "column,query,rows,selectivity,weight")?;
    for (case, column) in columns.iter().enumerate() {
        let encoded = &column.size_selected;
        let (low, high) = bounds(&encoded.truth);
        for width in widths(encoded.truth.len()) {
            let start = (encoded.truth.len() - width) / 2;
            let rows = Span::new(start, start + width)?;
            for query in [QueryKind::Sum, QueryKind::Between] {
                writeln!(
                    distribution,
                    "{},{},{},{:.8},1",
                    case,
                    query.name(),
                    width,
                    width as f64 / encoded.truth.len() as f64,
                )?;
                for tier in [StorageTier::Memory, StorageTier::BufferedHot] {
                    let selective = benchmark(
                        case,
                        encoded,
                        query,
                        rows,
                        low,
                        high,
                        PlanKind::Selective,
                        tier,
                        &bundle,
                    )?;
                    let fused = benchmark(
                        case,
                        encoded,
                        query,
                        rows,
                        low,
                        high,
                        PlanKind::Fused,
                        tier,
                        &bundle,
                    )?;
                    if selective.execution.answer != fused.execution.answer {
                        return Err(format!("real query mismatch on column {case}").into());
                    }
                    let chosen_plan = if width == encoded.truth.len() {
                        PlanKind::Fused
                    } else {
                        PlanKind::Selective
                    };
                    let chosen = match chosen_plan {
                        PlanKind::Selective => &selective,
                        PlanKind::Fused => &fused,
                    };
                    writeln!(
                        output,
                        "{},{},{},{},{},{},{},{:.8},{},{},{:.1},{:.1},{:.1},{},{},{},{},{},{}",
                        case,
                        quote(&column.group),
                        quote(&column.source),
                        quote(&column.name),
                        quote(&encoded.recipe.name()),
                        query.name(),
                        width,
                        width as f64 / encoded.truth.len() as f64,
                        tier.name(),
                        chosen_plan.name(),
                        selective.median_ns,
                        fused.median_ns,
                        chosen.median_ns,
                        chosen.execution.metrics.logical_bytes,
                        chosen.execution.metrics.delivered_bytes,
                        chosen.execution.metrics.transferred_bytes,
                        chosen.execution.metrics.transfer_operations,
                        chosen.execution.metrics.frames_decoded,
                        encoded.page.bytes().len(),
                    )?;
                }
            }
        }
        println!(
            "real column {case:02}: {}/{}/{} -> {}",
            column.group,
            column.source,
            column.name,
            column.size_selected.recipe.name()
        );
    }
    output.flush()?;
    distribution.flush()?;
    scan::run(&columns, RESULT_DIR)?;
    Ok(())
}

struct BenchmarkResult {
    median_ns: f64,
    execution: Execution,
}

#[allow(clippy::too_many_arguments)]
fn benchmark(
    case: usize,
    column: &EncodedColumn,
    query: QueryKind,
    rows: Span,
    low: i64,
    high: i64,
    plan: PlanKind,
    tier: StorageTier,
    bundle: &StorageBundle,
) -> Result<BenchmarkResult, String> {
    let run = || {
        let mode = match plan {
            PlanKind::Selective => ClosureMode::Selective,
            PlanKind::Fused => ClosureMode::FullPage,
        };
        let mut session = bundle.session(&column.page, tier, case, mode)?;
        execute_session(case, column, query, rows, low, high, plan, &mut session)
    };
    let execution = run()?;
    black_box(run()?);
    let started = Instant::now();
    black_box(run()?);
    let probe = started.elapsed().as_nanos().max(1);
    let iterations = usize::try_from((TARGET_NS / probe).clamp(1, 10_000)).unwrap();
    let mut samples = Vec::with_capacity(REPEATS);
    for _ in 0..REPEATS {
        let started = Instant::now();
        for _ in 0..iterations {
            black_box(run()?);
        }
        samples.push(started.elapsed().as_nanos() as f64 / iterations as f64);
    }
    samples.sort_by(f64::total_cmp);
    Ok(BenchmarkResult {
        median_ns: samples[REPEATS / 2],
        execution,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn execute_session(
    case: usize,
    column: &EncodedColumn,
    query: QueryKind,
    rows: Span,
    low: i64,
    high: i64,
    plan: PlanKind,
    session: &mut ReadSession<'_>,
) -> Result<Execution, String> {
    match (plan, query) {
        (PlanKind::Selective, QueryKind::Sum) => {
            generated::SUM_SESSION_FNS[case](column, rows, session)
        }
        (PlanKind::Selective, QueryKind::Between) => {
            generated::BETWEEN_SESSION_FNS[case](column, rows, low, high, session)
        }
        (PlanKind::Fused, QueryKind::Sum) => {
            generated::FUSED_SUM_SESSION_FNS[case](column, rows, session)
        }
        (PlanKind::Fused, QueryKind::Between) => {
            generated::FUSED_BETWEEN_SESSION_FNS[case](column, rows, low, high, session)
        }
    }
}

pub fn bounds(values: &[Option<i64>]) -> (i64, i64) {
    let mut present = values.iter().flatten().copied().collect::<Vec<_>>();
    present.sort_unstable();
    (present[present.len() / 4], present[present.len() * 3 / 4])
}

pub fn widths(rows: usize) -> Vec<usize> {
    let mut widths = vec![
        1,
        (rows / 1_000).max(1),
        (rows / 100).max(1),
        rows / 10,
        rows / 2,
        rows,
    ];
    widths.sort_unstable();
    widths.dedup();
    widths
}

pub fn answer_checksum(answer: &Answer) -> i128 {
    match answer {
        Answer::Value(value) => i128::from(value.unwrap_or_default()),
        Answer::Sum(sum) => *sum,
        Answer::Ranges(ranges) => ranges.iter().map(|range| range.len() as i128).sum(),
        Answer::Count(count) => *count as i128,
    }
}

fn verify_pages(columns: &[RealAccessColumn]) -> Result<(), Box<dyn std::error::Error>> {
    for (index, column) in columns.iter().enumerate() {
        let saved = fs::read(format!("{RESULT_DIR}/pages/column_{index:02}.acp"))?;
        if saved != column.size_selected.page.bytes() {
            return Err(format!("real serialized page {index} is stale").into());
        }
    }
    Ok(())
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}
