use std::fs::{self, File};
use std::hint::black_box;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Instant;

use witness::access_compiler::{
    ClosureMode, EncodedColumn, Execution, Predicate, Query, Span, compile, encode,
    execute_interpreted, generate_rust_module, heldout_cases, input_for,
    primitive_rule_fingerprint,
};

#[allow(dead_code)]
mod generated {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/experiments/generated/access_compiler/generated.rs"
    ));
}

const ROWS: usize = 16_384;
const REPEATS: usize = 7;
const TARGET_NS: u128 = 5_000_000;
const RESULT_DIR: &str = "experiments/results/access_compiler";

#[derive(Clone, Copy, Debug)]
enum Baseline {
    FullDecode,
    FusedDecode,
    Interpreted,
    GeneratedFullPageAccess,
    Generated,
    HandwrittenMonomorphized,
}

impl Baseline {
    fn name(self) -> &'static str {
        match self {
            Self::FullDecode => "full_decode",
            Self::FusedDecode => "fused_decode",
            Self::Interpreted => "interpreted_plan",
            Self::GeneratedFullPageAccess => "generated_full_page_access",
            Self::Generated => "generated",
            Self::HandwrittenMonomorphized => "handwritten_monomorphized",
        }
    }
}

struct QuerySpec {
    name: &'static str,
    query: Query,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if generated::RULE_FINGERPRINT != primitive_rule_fingerprint() {
        return Err("generated source is stale; run generate_access_compiler".into());
    }
    if generated::PLAN_SIGNATURES.contains(&0) {
        return Err("generated source has an invalid Plan IR signature".into());
    }
    fs::create_dir_all(RESULT_DIR)?;
    let cases = heldout_cases();
    let columns = cases
        .iter()
        .map(|case| encode(&case.recipe, input_for(case, ROWS)))
        .collect::<Result<Vec<_>, _>>()?;
    let mut raw = csv(format!("{RESULT_DIR}/benchmark.csv"))?;
    let mut summary = csv(format!("{RESULT_DIR}/summary.csv"))?;
    let mut costs = csv(format!("{RESULT_DIR}/compile_costs.csv"))?;
    writeln!(
        raw,
        "case,recipe,query,baseline,repeat,iterations,ns_per_op,logical_bytes,delivered_bytes,transferred_bytes,file_bytes,decoded_rows"
    )?;
    writeln!(
        summary,
        "case,recipe,query,baseline,p25_ns,median_ns,p75_ns,median_over_handwritten,fused_over_baseline,logical_bytes,delivered_bytes,transferred_bytes,file_bytes,delivered_fraction,decoded_rows,planner_ns"
    )?;
    writeln!(costs, "kind,case,query,p25_ns,median_ns,p75_ns")?;

    let codegen = benchmark_codegen(&columns)?;
    writeln!(
        costs,
        "rust_source_generation,all,all,{:.1},{:.1},{:.1}",
        codegen.0, codegen.1, codegen.2
    )?;
    for (case, column) in cases.iter().zip(&columns) {
        let saved = fs::read(format!("{RESULT_DIR}/pages/case_{:02}.acp", case.id))?;
        if saved != column.page.bytes() {
            return Err(format!("serialized case {} does not reproduce", case.id).into());
        }
        for query in queries(&column.truth) {
            run_cell(
                case.id,
                &case.name(),
                column,
                &query,
                &mut raw,
                &mut summary,
                &mut costs,
            )?;
        }
    }
    raw.flush()?;
    summary.flush()?;
    costs.flush()?;
    println!(
        "wrote six-baseline study for {} cases at fingerprint {:#018x}",
        cases.len(),
        primitive_rule_fingerprint()
    );
    Ok(())
}

fn run_cell(
    case: usize,
    recipe: &str,
    column: &EncodedColumn,
    query: &QuerySpec,
    raw: &mut impl Write,
    summary: &mut impl Write,
    costs: &mut impl Write,
) -> Result<(), Box<dyn std::error::Error>> {
    let planner = benchmark_planner(column, &query.query)?;
    writeln!(
        costs,
        "plan_compilation,{},{},{:.1},{:.1},{:.1}",
        case, query.name, planner.0, planner.1, planner.2
    )?;
    let baselines = [
        Baseline::FullDecode,
        Baseline::FusedDecode,
        Baseline::Interpreted,
        Baseline::GeneratedFullPageAccess,
        Baseline::Generated,
        Baseline::HandwrittenMonomorphized,
    ];
    let expected = run(case, column, &query.query, Baseline::FullDecode)?;
    let mut results = Vec::new();
    for baseline in baselines {
        let diagnostic = run(case, column, &query.query, baseline)?;
        if diagnostic.answer != expected.answer {
            return Err(format!(
                "{recipe} {} {} answer mismatch",
                query.name,
                baseline.name()
            )
            .into());
        }
        if matches!(baseline, Baseline::GeneratedFullPageAccess)
            && diagnostic.metrics.delivered_bytes != column.page.bytes().len()
        {
            return Err("full-page generated plan did not deliver the full page".into());
        }
        let samples = benchmark(case, column, &query.query, baseline)?;
        for (repeat, (iterations, ns)) in samples.iter().enumerate() {
            writeln!(
                raw,
                "{},{},{},{},{},{},{:.1},{},{},{},{},{}",
                case,
                quote(recipe),
                query.name,
                baseline.name(),
                repeat,
                iterations,
                ns,
                diagnostic.metrics.logical_bytes,
                diagnostic.metrics.delivered_bytes,
                diagnostic.metrics.transferred_bytes,
                column.page.bytes().len(),
                diagnostic.decoded_rows,
            )?;
        }
        results.push((
            baseline,
            quartiles(samples.into_iter().map(|(_, ns)| ns).collect()),
            diagnostic,
        ));
    }
    let handwritten = results
        .iter()
        .find(|(baseline, _, _)| matches!(baseline, Baseline::HandwrittenMonomorphized))
        .unwrap()
        .1
        .1;
    let fused = results
        .iter()
        .find(|(baseline, _, _)| matches!(baseline, Baseline::FusedDecode))
        .unwrap()
        .1
        .1;
    for (baseline, (p25, median, p75), diagnostic) in results {
        writeln!(
            summary,
            "{},{},{},{},{:.1},{:.1},{:.1},{:.4},{:.4},{},{},{},{},{:.6},{},{:.1}",
            case,
            quote(recipe),
            query.name,
            baseline.name(),
            p25,
            median,
            p75,
            median / handwritten,
            fused / median,
            diagnostic.metrics.logical_bytes,
            diagnostic.metrics.delivered_bytes,
            diagnostic.metrics.transferred_bytes,
            column.page.bytes().len(),
            diagnostic.metrics.delivered_bytes as f64 / column.page.bytes().len() as f64,
            diagnostic.decoded_rows,
            planner.1,
        )?;
    }
    println!(
        "[{case:02}] {query_name:<14} {recipe}",
        query_name = query.name
    );
    Ok(())
}

fn benchmark(
    case: usize,
    column: &EncodedColumn,
    query: &Query,
    baseline: Baseline,
) -> Result<Vec<(usize, f64)>, String> {
    let execute = || run(case, column, query, baseline);
    black_box(execute()?);
    let started = Instant::now();
    black_box(execute()?);
    let probe = started.elapsed().as_nanos().max(1);
    let iterations = usize::try_from((TARGET_NS / probe).clamp(1, 20_000)).unwrap();
    let mut samples = Vec::with_capacity(REPEATS);
    for _ in 0..REPEATS {
        let started = Instant::now();
        for _ in 0..iterations {
            black_box(execute()?);
        }
        samples.push((
            iterations,
            started.elapsed().as_nanos() as f64 / iterations as f64,
        ));
    }
    Ok(samples)
}

fn run(
    case: usize,
    column: &EncodedColumn,
    query: &Query,
    baseline: Baseline,
) -> Result<Execution, String> {
    match baseline {
        Baseline::FullDecode => generated_materialized_run(case, column, query),
        Baseline::FusedDecode => generated_fused_run(case, column, query),
        Baseline::Interpreted => execute_interpreted(column, query, ClosureMode::Selective),
        Baseline::GeneratedFullPageAccess => {
            generated_run(case, column, query, ClosureMode::FullPage)
        }
        Baseline::Generated => generated_run(case, column, query, ClosureMode::Selective),
        Baseline::HandwrittenMonomorphized => {
            monomorphized_run(case, column, query, ClosureMode::Selective)
        }
    }
}

fn generated_materialized_run(
    case: usize,
    column: &EncodedColumn,
    query: &Query,
) -> Result<Execution, String> {
    generated_dispatch(
        case,
        column,
        query,
        &generated::MATERIALIZED_GET_FNS,
        &generated::MATERIALIZED_SUM_FNS,
        &generated::MATERIALIZED_BETWEEN_FNS,
    )
}

fn generated_fused_run(
    case: usize,
    column: &EncodedColumn,
    query: &Query,
) -> Result<Execution, String> {
    generated_dispatch(
        case,
        column,
        query,
        &generated::FUSED_GET_FNS,
        &generated::FUSED_SUM_FNS,
        &generated::FUSED_BETWEEN_FNS,
    )
}

fn generated_run(
    case: usize,
    column: &EncodedColumn,
    query: &Query,
    mode: ClosureMode,
) -> Result<Execution, String> {
    match *query {
        Query::Get { row } => generated::GET_FNS[case](column, row, mode),
        Query::Sum { rows } => generated::SUM_FNS[case](column, rows, mode),
        Query::Between { rows, low, high } => {
            generated::BETWEEN_FNS[case](column, rows, low, high, mode)
        }
        Query::Filter {
            predicate: Predicate::Between { low, high },
        } => generated::FILTER_FNS[case](column, low, high, mode),
        Query::Filter {
            predicate: Predicate::Equals { .. },
        } => Err("access_compiler_study has no generated Equals-filter kernels".into()),
        Query::Count { .. } => Err("access_compiler_study has no generated Count kernels".into()),
    }
}

fn generated_dispatch(
    case: usize,
    column: &EncodedColumn,
    query: &Query,
    get: &[generated::GetFn; generated::CASE_COUNT],
    sum: &[generated::SumFn; generated::CASE_COUNT],
    between: &[generated::BetweenFn; generated::CASE_COUNT],
) -> Result<Execution, String> {
    match *query {
        Query::Get { row } => get[case](column, row, ClosureMode::Selective),
        Query::Sum { rows } => sum[case](column, rows, ClosureMode::Selective),
        Query::Between { rows, low, high } => {
            between[case](column, rows, low, high, ClosureMode::Selective)
        }
        Query::Filter {
            predicate: Predicate::Between { low, high },
        } => between[case](
            column,
            Span::new(0, column.truth.len())?,
            low,
            high,
            ClosureMode::Selective,
        ),
        Query::Filter {
            predicate: Predicate::Equals { .. },
        } => Err("access_compiler_study has no generated Equals-filter kernels".into()),
        Query::Count { .. } => Err("access_compiler_study has no generated Count kernels".into()),
    }
}

fn monomorphized_run(
    case: usize,
    column: &EncodedColumn,
    query: &Query,
    mode: ClosureMode,
) -> Result<Execution, String> {
    match *query {
        Query::Get { row } => generated::STATIC_GET_FNS[case](column, row, mode),
        Query::Sum { rows } => generated::STATIC_SUM_FNS[case](column, rows, mode),
        Query::Between { rows, low, high } => {
            generated::STATIC_BETWEEN_FNS[case](column, rows, low, high, mode)
        }
        Query::Filter {
            predicate: Predicate::Between { low, high },
        } => generated::STATIC_FILTER_FNS[case](column, low, high, mode),
        Query::Filter {
            predicate: Predicate::Equals { .. },
        } => Err("access_compiler_study has no static Equals-filter kernels".into()),
        Query::Count { .. } => Err("access_compiler_study has no static Count kernels".into()),
    }
}

fn benchmark_planner(column: &EncodedColumn, query: &Query) -> Result<(f64, f64, f64), String> {
    let started = Instant::now();
    black_box(compile(column, query.clone())?);
    let probe = started.elapsed().as_nanos().max(1);
    let iterations = usize::try_from((TARGET_NS / probe).clamp(8, 20_000)).unwrap();
    let mut samples = Vec::with_capacity(REPEATS);
    for _ in 0..REPEATS {
        let started = Instant::now();
        for _ in 0..iterations {
            black_box(compile(column, query.clone())?);
        }
        samples.push(started.elapsed().as_nanos() as f64 / iterations as f64);
    }
    Ok(quartiles(samples))
}

fn benchmark_codegen(columns: &[EncodedColumn]) -> Result<(f64, f64, f64), String> {
    let mut samples = Vec::with_capacity(REPEATS);
    for _ in 0..REPEATS {
        let started = Instant::now();
        black_box(generate_rust_module(columns)?);
        samples.push(started.elapsed().as_nanos() as f64);
    }
    Ok(quartiles(samples))
}

fn queries(values: &[Option<i64>]) -> Vec<QuerySpec> {
    let n = values.len();
    let narrow = (n / 100).max(1);
    let narrow_start = n / 2 - narrow / 2;
    let wide_start = n / 7;
    let wide_end = n * 6 / 7;
    let mut present = values.iter().flatten().copied().collect::<Vec<_>>();
    present.sort_unstable();
    let low = present[present.len() / 4];
    let middle = present[present.len() / 2];
    let high = present[present.len() * 3 / 4];
    vec![
        QuerySpec {
            name: "GET",
            query: Query::Get { row: n / 2 + 7 },
        },
        QuerySpec {
            name: "SUM_1PCT",
            query: Query::Sum {
                rows: Span::new(narrow_start, narrow_start + narrow).unwrap(),
            },
        },
        QuerySpec {
            name: "SUM_70PCT",
            query: Query::Sum {
                rows: Span::new(wide_start, wide_end).unwrap(),
            },
        },
        QuerySpec {
            name: "BETWEEN_1PCT",
            query: Query::Between {
                rows: Span::new(narrow_start, narrow_start + narrow).unwrap(),
                low,
                high,
            },
        },
        QuerySpec {
            name: "BETWEEN_70PCT",
            query: Query::Between {
                rows: Span::new(wide_start, wide_end).unwrap(),
                low,
                high,
            },
        },
        QuerySpec {
            name: "FILTER_POINT",
            query: Query::Filter {
                predicate: Predicate::Between {
                    low: middle,
                    high: middle,
                },
            },
        },
        QuerySpec {
            name: "FILTER_50PCT",
            query: Query::Filter {
                predicate: Predicate::Between { low, high },
            },
        },
    ]
}

fn quartiles(mut values: Vec<f64>) -> (f64, f64, f64) {
    values.sort_by(f64::total_cmp);
    (
        values[values.len() / 4],
        values[values.len() / 2],
        values[values.len() * 3 / 4],
    )
}

fn csv(path: impl AsRef<Path>) -> std::io::Result<BufWriter<File>> {
    Ok(BufWriter::new(File::create(path)?))
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}
