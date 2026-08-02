use std::fs::{self, File};
use std::hint::black_box;
use std::io::{BufWriter, Write};
use std::time::Instant;

use witness::access_compiler::{
    AccessMetrics, Answer, ClosureMode, CostFeatures, DecoderNode, EncodedColumn, Execution,
    HeldoutCase, Span, encode, heldout_cases, input_for, primitive_rule_fingerprint,
};

use super::model;
use super::scan;
use super::storage::{StorageBundle, StorageTier};
use crate::generated;

pub const ROWS: usize = 16_384;
pub const TRAINING_CASES: usize = 10;
pub const EVALUATION_CASES: usize = 5;
pub const TOTAL_CASES: usize = TRAINING_CASES + EVALUATION_CASES;
const REPEATS: usize = 5;
const TARGET_NS: u128 = 500_000;
const RESULT_DIR: &str = "experiments/results/access_crossover";

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
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

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
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

#[derive(Clone, Copy, Debug)]
pub struct StructureFeatures {
    pub restart_entries: usize,
    pub rle_runs: usize,
    pub dictionary_entries: usize,
    pub patch_count: usize,
    pub nullable_nodes: usize,
}

#[derive(Clone, Debug)]
pub struct CurveCell {
    pub case: usize,
    pub recipe: String,
    pub query: QueryKind,
    pub rows: Span,
    pub tier: StorageTier,
    pub plan: PlanKind,
    pub p25_ns: f64,
    pub median_ns: f64,
    pub p75_ns: f64,
    pub metrics: AccessMetrics,
    pub decoded_rows: usize,
    pub file_bytes: usize,
    pub metadata_bytes: usize,
    pub layout_frames: usize,
    pub structure: StructureFeatures,
}

impl CurveCell {
    pub fn exact_features(&self) -> CostFeatures {
        CostFeatures {
            selected_rows: self.rows.len(),
            decoded_rows: self.decoded_rows,
            delivered_bytes: self.metrics.delivered_bytes,
            transfer_operations: self.metrics.transfer_operations,
            frames_decoded: self.metrics.frames_decoded,
            restart_entries: self.structure.restart_entries,
            rle_runs: self.structure.rle_runs,
            dictionary_entries: self.structure.dictionary_entries,
            patch_count: self.structure.patch_count,
            nullable_nodes: self.structure.nullable_nodes,
        }
    }

    pub fn preflight_free_features(&self) -> CostFeatures {
        preflight_free_features(
            self.rows.len(),
            ROWS,
            self.file_bytes,
            self.metadata_bytes,
            self.layout_frames,
            self.plan,
            self.structure,
        )
    }
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    if generated::RULE_FINGERPRINT != primitive_rule_fingerprint() {
        return Err("generated source is stale; run generate_access_compiler".into());
    }
    fs::create_dir_all(RESULT_DIR)?;
    let cases = study_cases()?;
    let columns = cases
        .iter()
        .map(|case| encode(&case.recipe, input_for(case, ROWS)))
        .collect::<Result<Vec<_>, _>>()?;
    let pages = columns
        .iter()
        .map(|column| column.page.bytes())
        .collect::<Vec<_>>();
    let bundle = StorageBundle::build(format!("{RESULT_DIR}/curve_bundle.acp"), &pages, 4096)?;
    let mut cells = Vec::new();
    let mut output = BufWriter::new(File::create(format!("{RESULT_DIR}/curve.csv"))?);
    writeln!(
        output,
        "case,split,recipe,query,rows,selectivity,tier,plan,p25_ns,median_ns,p75_ns,logical_bytes,delivered_bytes,transferred_bytes,transfer_operations,frames_decoded,decoded_rows,file_bytes,metadata_bytes,layout_frames,restart_entries,rle_runs,dictionary_entries,patch_count,nullable_nodes"
    )?;

    for (case, column) in cases.iter().zip(&columns) {
        let structure = structure_features(column);
        let (low, high) = bounds(&column.truth);
        for width in query_widths() {
            let start = (ROWS - width) / 2;
            let rows = Span::new(start, start + width)?;
            for query in [QueryKind::Sum, QueryKind::Between] {
                for tier in StorageTier::ALL {
                    let mut pair = Vec::new();
                    for plan in [PlanKind::Selective, PlanKind::Fused] {
                        let execution = execute(
                            case.id, column, query, rows, low, high, plan, tier, &bundle, case.id,
                        )?;
                        let samples = benchmark(
                            case.id, column, query, rows, low, high, plan, tier, &bundle, case.id,
                        )?;
                        let (p25_ns, median_ns, p75_ns) = quartiles(samples);
                        let cell = CurveCell {
                            case: case.id,
                            recipe: case.name(),
                            query,
                            rows,
                            tier,
                            plan,
                            p25_ns,
                            median_ns,
                            p75_ns,
                            metrics: execution.metrics,
                            decoded_rows: execution.decoded_rows,
                            file_bytes: column.page.bytes().len(),
                            metadata_bytes: column.page.layout().fields[0].length,
                            layout_frames: column.page.layout().frames.len(),
                            structure,
                        };
                        write_cell(&mut output, &cell)?;
                        pair.push((execution.answer, cell));
                    }
                    if pair[0].0 != pair[1].0 {
                        return Err(format!(
                            "answer mismatch for case {} {} rows {}",
                            case.id,
                            query.name(),
                            width
                        )
                        .into());
                    }
                    cells.extend(pair.into_iter().map(|(_, cell)| cell));
                }
            }
        }
        println!(
            "measured crossover curve for case {:02}: {}",
            case.id,
            case.name()
        );
    }
    output.flush()?;
    let models = model::fit_and_evaluate(&cells, RESULT_DIR)?;
    scan::run(&columns, &cells, &models, RESULT_DIR)?;
    println!(
        "wrote crossover, held-out model, and scan studies at fingerprint {:#018x}",
        primitive_rule_fingerprint()
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn execute(
    case: usize,
    column: &EncodedColumn,
    query: QueryKind,
    rows: Span,
    low: i64,
    high: i64,
    plan: PlanKind,
    tier: StorageTier,
    bundle: &StorageBundle,
    page_index: usize,
) -> Result<Execution, String> {
    let mode = match plan {
        PlanKind::Selective => ClosureMode::Selective,
        PlanKind::Fused => ClosureMode::FullPage,
    };
    let mut session = bundle.session(&column.page, tier, page_index, mode)?;
    match (plan, query) {
        (PlanKind::Selective, QueryKind::Sum) => {
            generated::SUM_SESSION_FNS[case](column, rows, &mut session)
        }
        (PlanKind::Selective, QueryKind::Between) => {
            generated::BETWEEN_SESSION_FNS[case](column, rows, low, high, &mut session)
        }
        (PlanKind::Fused, QueryKind::Sum) => {
            generated::FUSED_SUM_SESSION_FNS[case](column, rows, &mut session)
        }
        (PlanKind::Fused, QueryKind::Between) => {
            generated::FUSED_BETWEEN_SESSION_FNS[case](column, rows, low, high, &mut session)
        }
    }
}

pub fn query_widths() -> Vec<usize> {
    let mut widths = vec![1, 4, 16, 64, 256];
    for percentage in [1, 5, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100] {
        widths.push((ROWS * percentage / 100).max(1));
    }
    widths.sort_unstable();
    widths.dedup();
    widths
}

pub fn bounds(values: &[Option<i64>]) -> (i64, i64) {
    let mut present = values.iter().flatten().copied().collect::<Vec<_>>();
    present.sort_unstable();
    (present[present.len() / 4], present[present.len() * 3 / 4])
}

pub fn preflight_free_features(
    selected_rows: usize,
    total_rows: usize,
    file_bytes: usize,
    metadata_bytes: usize,
    layout_frames: usize,
    plan: PlanKind,
    structure: StructureFeatures,
) -> CostFeatures {
    let fused = matches!(plan, PlanKind::Fused);
    let estimated_payload = file_bytes.saturating_sub(metadata_bytes);
    let delivered_bytes = if fused || layout_frames > 0 {
        file_bytes
    } else {
        metadata_bytes
            + estimated_payload
                .saturating_mul(selected_rows)
                .div_ceil(total_rows)
    };
    CostFeatures {
        selected_rows,
        decoded_rows: if fused { total_rows } else { selected_rows },
        delivered_bytes,
        transfer_operations: if fused {
            1
        } else {
            1 + layout_frames
                + usize::from(structure.restart_entries > 0)
                + usize::from(structure.rle_runs > 0)
                + usize::from(structure.dictionary_entries > 0)
                + usize::from(structure.patch_count > 0)
                + structure.nullable_nodes
        },
        frames_decoded: layout_frames,
        restart_entries: structure.restart_entries,
        rle_runs: structure.rle_runs,
        dictionary_entries: structure.dictionary_entries,
        patch_count: structure.patch_count,
        nullable_nodes: structure.nullable_nodes,
    }
}

fn study_cases() -> Result<Vec<HeldoutCase>, String> {
    let cases = heldout_cases()
        .into_iter()
        .take(TOTAL_CASES)
        .collect::<Vec<_>>();
    if cases.len() != TOTAL_CASES
        || cases
            .iter()
            .enumerate()
            .any(|(expected, case)| case.id != expected)
    {
        return Err(format!(
            "crossover study requires contiguous cases 0 through {}",
            TOTAL_CASES - 1
        ));
    }
    Ok(cases)
}

fn structure_features(column: &EncodedColumn) -> StructureFeatures {
    let mut features = StructureFeatures {
        restart_entries: 0,
        rle_runs: 0,
        dictionary_entries: 0,
        patch_count: 0,
        nullable_nodes: 0,
    };
    for node in column.decoder.nodes() {
        match node {
            DecoderNode::Delta {
                restart_interval,
                len,
                ..
            } => features.restart_entries += len.div_ceil(*restart_interval),
            DecoderNode::Rle { runs, .. } => features.rle_runs += runs,
            DecoderNode::Dictionary { entries, .. } => features.dictionary_entries += entries,
            DecoderNode::Patch { count, .. } => features.patch_count += count,
            DecoderNode::Nullable { .. } => features.nullable_nodes += 1,
            DecoderNode::BitUnpack { .. } | DecoderNode::For { .. } => {}
        }
    }
    features
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
    page_index: usize,
) -> Result<Vec<f64>, String> {
    let run = || {
        execute(
            case, column, query, rows, low, high, plan, tier, bundle, page_index,
        )
    };
    if tier.is_cold() {
        let mut samples = Vec::with_capacity(REPEATS);
        for _ in 0..REPEATS {
            bundle.evict_page(page_index)?;
            let started = Instant::now();
            black_box(run()?);
            samples.push(started.elapsed().as_nanos() as f64);
        }
        return Ok(samples);
    }
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
    Ok(samples)
}

fn quartiles(mut samples: Vec<f64>) -> (f64, f64, f64) {
    samples.sort_by(f64::total_cmp);
    (
        samples[samples.len() / 4],
        samples[samples.len() / 2],
        samples[samples.len() * 3 / 4],
    )
}

fn write_cell(output: &mut impl Write, cell: &CurveCell) -> std::io::Result<()> {
    writeln!(
        output,
        "{},{},{},{},{},{:.8},{},{},{:.1},{:.1},{:.1},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
        cell.case,
        if cell.case < TRAINING_CASES {
            "train"
        } else {
            "heldout"
        },
        quote(&cell.recipe),
        cell.query.name(),
        cell.rows.len(),
        cell.rows.len() as f64 / ROWS as f64,
        cell.tier.name(),
        cell.plan.name(),
        cell.p25_ns,
        cell.median_ns,
        cell.p75_ns,
        cell.metrics.logical_bytes,
        cell.metrics.delivered_bytes,
        cell.metrics.transferred_bytes,
        cell.metrics.transfer_operations,
        cell.metrics.frames_decoded,
        cell.decoded_rows,
        cell.file_bytes,
        cell.metadata_bytes,
        cell.layout_frames,
        cell.structure.restart_entries,
        cell.structure.rle_runs,
        cell.structure.dictionary_entries,
        cell.structure.patch_count,
        cell.structure.nullable_nodes,
    )
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

pub fn answer_checksum(answer: &Answer) -> i128 {
    match answer {
        Answer::Value(value) => i128::from(value.unwrap_or_default()),
        Answer::Sum(sum) => *sum,
        Answer::Ranges(ranges) => ranges.iter().map(|range| range.len() as i128).sum(),
        Answer::Count(count) => *count as i128,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crossover_partition_stays_ten_training_and_five_evaluation_cases() {
        let cases = study_cases().unwrap();

        assert_eq!(TRAINING_CASES, 10);
        assert_eq!(EVALUATION_CASES, 5);
        assert_eq!(cases.len(), TOTAL_CASES);
        assert_eq!(cases[TRAINING_CASES].id, 10);
        assert_eq!(cases[TOTAL_CASES - 1].id, 14);
    }
}
