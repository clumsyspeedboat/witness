use crate::experiment::types::{Column, YELLOW_PARQUET};
use arrow_array::{Array, Float64Array, Int32Array, Int64Array, TimestampMicrosecondArray};
use arrow_schema::{DataType, TimeUnit};
use chrono::{NaiveDateTime, TimeZone, Utc};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use std::error::Error;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct StudyPair {
    pub class: &'static str,
    pub group: String,
    pub source: String,
    pub timestamp: Column,
    pub value: Column,
}

#[derive(Clone, Debug)]
pub struct StudySource {
    pub class: &'static str,
    pub group: String,
    pub source: String,
    pub timestamp: Column,
    pub values: Vec<Column>,
}

#[derive(Clone, Debug)]
pub struct PredicateSource {
    pub class: &'static str,
    pub selector_kind: &'static str,
    pub group: String,
    pub source: String,
    pub selector: Column,
    pub values: Vec<Column>,
}

pub fn predicate_sources(max_rows: usize) -> Result<Vec<PredicateSource>, Box<dyn Error>> {
    if max_rows < 1_024 {
        return Err("predicate corpus requires at least 1024 rows per source".into());
    }
    let mut sources = predicate_nab_sources(max_rows, usize::MAX)?;
    add_household_predicate_source(&mut sources, max_rows)?;
    add_directory_predicate_sources(&mut sources, max_rows)?;
    add_taxi_predicate_source(&mut sources, max_rows)?;
    sources.sort_by(|left, right| (&left.group, &left.source).cmp(&(&right.group, &right.source)));
    Ok(sources)
}

pub fn all_columns() -> Result<Vec<Column>, Box<dyn Error>> {
    let mut columns = small_columns()?;
    columns.extend(large_columns()?);
    columns.extend(breadth_columns()?);
    Ok(columns)
}

/// Bounded breadth corpus for invariant prevalence. Unlike the query-pair
/// corpus, this includes every locally available exact integer column.
pub fn invariant_census_columns(max_rows: usize) -> Result<Vec<Column>, Box<dyn Error>> {
    if max_rows < 1_024 {
        return Err("invariant census requires at least 1024 rows per column".into());
    }
    let mut columns = Vec::new();
    for source in predicate_nab_sources(max_rows, usize::MAX)? {
        columns.push(source.selector);
        columns.extend(source.values);
    }
    columns.extend(
        household_power_columns_limited(Some(max_rows))?
            .into_iter()
            .filter(|column| column.exact_i64),
    );
    for (group, directory) in [
        ("publicbi", "eval/data/publicbi"),
        ("tpch", "eval/data/tpch"),
        ("clickbench", "eval/data/clickbench"),
    ] {
        for mut column in dir_columns(group, directory)? {
            column.values.truncate(max_rows);
            if column.exact_i64 {
                columns.push(column);
            }
        }
    }
    columns.extend(
        yellow_columns(Some(max_rows))?
            .into_iter()
            .filter(|column| column.exact_i64),
    );
    columns.retain(|column| column.values.len() >= 1_024);
    columns.sort_by(|left, right| {
        (&left.group, &left.source, &left.name).cmp(&(&right.group, &right.source, &right.name))
    });
    columns.dedup_by(|right, left| {
        (&right.group, &right.source, &right.name) == (&left.group, &left.source, &left.name)
    });
    Ok(columns)
}

/// Bounded, named corpus slice used by the canonical study. Large
/// benchmark columns use their first one million rows so a laptop can reproduce
/// the complete artifact without loading the multi-gigabyte breadth census.
pub fn study_columns() -> Result<Vec<Column>, Box<dyn Error>> {
    let mut out = synthetic_columns("synthetic", 262_144);
    for (source, path) in [
        ("nyc_taxi", "eval/data/realKnownCause__nyc_taxi.csv"),
        (
            "ambient_temperature",
            "eval/data/realKnownCause__ambient_temperature_system_failure.csv",
        ),
        (
            "twitter_aapl",
            "eval/data/realTweets__Twitter_volume_AAPL.csv",
        ),
    ] {
        let path = study_data_path(path);
        if path.exists() {
            let (timestamps, values) = read_nab_csv(path.to_str().ok_or("non-UTF8 data path")?)?;
            out.push(Column {
                group: "nab".to_string(),
                source: source.to_string(),
                name: "timestamp".to_string(),
                mode: "timestamp_epoch_s".to_string(),
                values: timestamps.into_iter().map(Some).collect(),
                float_values: None,
                exact_i64: true,
            });
            match values {
                NabValues::Exact { values, scale } => out.push(Column {
                    group: "nab".to_string(),
                    source: source.to_string(),
                    name: format!("value_x{scale}"),
                    mode: format!("value_scaled_x{scale}_verified"),
                    values: values.into_iter().map(Some).collect(),
                    float_values: None,
                    exact_i64: true,
                }),
                NabValues::Float(values) => out.push(Column {
                    group: "nab".to_string(),
                    source: source.to_string(),
                    name: "value".to_string(),
                    mode: "float_native".to_string(),
                    values: Vec::new(),
                    float_values: Some(values.into_iter().map(Some).collect()),
                    exact_i64: false,
                }),
            }
        }
    }

    for (group, path) in [
        ("tpch", "eval/data/tpch/lineitem__c0_l_orderkey.csv"),
        ("tpch", "eval/data/tpch/lineitem__c3_l_linenumber.csv"),
        ("tpch", "eval/data/tpch/lineitem__c4_l_quantity.csv"),
        ("clickbench", "eval/data/clickbench/hits__c1_CounterID.csv"),
        ("clickbench", "eval/data/clickbench/hits__c2_RegionID.csv"),
        (
            "clickbench",
            "eval/data/clickbench/hits__c3_ResolutionWidth.csv",
        ),
        ("publicbi", "eval/data/publicbi/Bimbo__c0_Agencia_ID.csv"),
        ("publicbi", "eval/data/publicbi/Bimbo__c2_Cliente_ID.csv"),
        (
            "publicbi",
            "eval/data/publicbi/Bimbo__c3_Demanda_uni_equil.csv",
        ),
    ] {
        let path = study_data_path(path);
        if path.exists() {
            out.push(single_column_file(
                group,
                path.to_str().ok_or("non-UTF8 data path")?,
                1_000_000,
            )?);
        }
    }
    Ok(out)
}

/// Predeclared causal corpus. Admission depends only on schema, alignment, and
/// lossless `i64` conversion, never on an encoding or timing outcome.
pub fn study_sources() -> Result<Vec<StudySource>, Box<dyn Error>> {
    let mut sources = Vec::new();
    visit_study_sources(|source| {
        sources.push(source);
        Ok(())
    })?;
    Ok(sources)
}

/// Load one causal source at a time. The visitor owns each source so callers
/// can release million-row matrices before the next source is parsed.
pub fn visit_study_sources(
    mut visit: impl FnMut(StudySource) -> Result<(), Box<dyn Error>>,
) -> Result<(), Box<dyn Error>> {
    let synthetic = synthetic_columns("synthetic", 262_144);
    visit(make_source(
        "controlled",
        "synthetic",
        "linear_clock",
        synthetic
            .iter()
            .find(|column| column.name == "linear")
            .ok_or("synthetic timestamp column missing")?
            .clone(),
        vec![
            synthetic
                .iter()
                .find(|column| column.name == "noisy_lcg")
                .ok_or("synthetic value column missing")?
                .clone(),
        ],
    )?)?;

    for source in real_nab_study_sources()? {
        visit(source)?;
    }

    let household = household_power_columns_limited(Some(1_000_000))?;
    if let Some(timestamp) = household
        .iter()
        .find(|column| column.name == "timestamp")
        .cloned()
    {
        let values = household
            .into_iter()
            .filter(|column| column.name != "timestamp" && column.exact_i64)
            .collect();
        visit(make_source(
            "structured",
            "household",
            "uci_household_power",
            timestamp,
            values,
        )?)?;
    }

    for (group, source, timestamp_file, directory) in [
        (
            "tpch",
            "lineitem",
            "lineitem__c8_timestamp.csv",
            "eval/data/tpch",
        ),
        (
            "clickbench",
            "hits",
            "hits__c0_timestamp.csv",
            "eval/data/clickbench",
        ),
    ] {
        if let Some(source) = directory_study_source(
            "analytical",
            group,
            source,
            timestamp_file,
            directory,
            1_000_000,
        )? {
            visit(source)?;
        }
    }

    if let Some(source) = yellow_taxi_study_source(1_000_000)? {
        visit(source)?;
    }
    Ok(())
}

pub fn study_pairs() -> Result<Vec<StudyPair>, Box<dyn Error>> {
    let mut pairs = Vec::new();
    for source in study_sources()? {
        for value in source.values {
            pairs.push(make_pair(
                source.class,
                &source.group,
                &source.source,
                source.timestamp.clone(),
                value,
            )?);
        }
    }
    Ok(pairs)
}

fn make_pair(
    class: &'static str,
    group: &str,
    source: &str,
    timestamp: Column,
    value: Column,
) -> Result<StudyPair, Box<dyn Error>> {
    if timestamp.len() != value.len() || timestamp.is_empty() {
        return Err(format!("unaligned study pair {group}/{source}").into());
    }
    Ok(StudyPair {
        class,
        group: group.to_string(),
        source: source.to_string(),
        timestamp,
        value,
    })
}

fn make_source(
    class: &'static str,
    group: &str,
    source: &str,
    timestamp: Column,
    values: Vec<Column>,
) -> Result<StudySource, Box<dyn Error>> {
    if timestamp.is_empty() || values.is_empty() {
        return Err(format!("empty study source {group}/{source}").into());
    }
    if values
        .iter()
        .any(|value| !value.exact_i64 || value.len() != timestamp.len())
    {
        return Err(format!("unaligned or inexact study source {group}/{source}").into());
    }
    Ok(StudySource {
        class,
        group: group.to_string(),
        source: source.to_string(),
        timestamp,
        values,
    })
}

fn real_nab_study_sources() -> Result<Vec<StudySource>, Box<dyn Error>> {
    let root = study_data_path("eval/data/nab");
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for category in [
        "realAWSCloudwatch",
        "realAdExchange",
        "realKnownCause",
        "realTraffic",
        "realTweets",
    ] {
        let directory = root.join(category);
        if !directory.exists() {
            continue;
        }
        for entry in fs::read_dir(directory)? {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) == Some("csv") {
                paths.push((category, path));
            }
        }
    }
    paths.sort_by(|left, right| left.1.cmp(&right.1));

    let mut out = Vec::new();
    for (category, path) in paths {
        let source_name = path
            .file_stem()
            .and_then(|value| value.to_str())
            .ok_or("non-UTF8 NAB filename")?;
        let source = format!("{category}/{source_name}");
        let (timestamps, parsed) = read_nab_csv(path.to_str().ok_or("non-UTF8 NAB path")?)?;
        let NabValues::Exact { values, scale } = parsed else {
            continue;
        };
        let timestamp = Column {
            group: "nab".to_string(),
            source: source.clone(),
            name: "timestamp".to_string(),
            mode: "timestamp_epoch_s".to_string(),
            values: timestamps.into_iter().map(Some).collect(),
            float_values: None,
            exact_i64: true,
        };
        let value = Column {
            group: "nab".to_string(),
            source: source.clone(),
            name: format!("value_x{scale}"),
            mode: format!("value_scaled_x{scale}_verified"),
            values: values.into_iter().map(Some).collect(),
            float_values: None,
            exact_i64: true,
        };
        out.push(make_source(
            "structured",
            "nab",
            &source,
            timestamp,
            vec![value],
        )?);
    }
    Ok(out)
}

fn predicate_nab_sources(
    max_rows: usize,
    per_category: usize,
) -> Result<Vec<PredicateSource>, Box<dyn Error>> {
    let root = study_data_path("eval/data/nab");
    let mut output = Vec::new();
    for category in [
        "realAWSCloudwatch",
        "realAdExchange",
        "realKnownCause",
        "realTraffic",
        "realTweets",
    ] {
        let directory = root.join(category);
        if !directory.exists() {
            continue;
        }
        let mut paths = csv_paths(&directory)?;
        paths.truncate(per_category);
        for path in paths {
            let stem = path
                .file_stem()
                .and_then(|value| value.to_str())
                .ok_or("non-UTF8 NAB filename")?;
            let source = format!("{category}/{stem}");
            let (timestamps, parsed) = read_nab_csv(path.to_str().ok_or("non-UTF8 NAB path")?)?;
            let NabValues::Exact { values, scale } = parsed else {
                continue;
            };
            let selector = integer_column("nab", &source, "timestamp", timestamps, max_rows);
            let value =
                integer_column("nab", &source, &format!("value_x{scale}"), values, max_rows);
            push_predicate_source(
                &mut output,
                "structured",
                "timestamp",
                "nab",
                &source,
                selector,
                vec![value],
            )?;
        }
    }
    Ok(output)
}

fn add_household_predicate_source(
    output: &mut Vec<PredicateSource>,
    max_rows: usize,
) -> Result<(), Box<dyn Error>> {
    let mut columns = household_power_columns_limited(Some(max_rows))?;
    let Some(selector_index) = columns.iter().position(|column| column.name == "timestamp") else {
        return Ok(());
    };
    let selector = columns.remove(selector_index);
    let values = columns
        .into_iter()
        .filter(|column| column.exact_i64)
        .take(2)
        .collect();
    push_predicate_source(
        output,
        "structured",
        "timestamp",
        "household",
        "uci_household_power",
        selector,
        values,
    )
}

fn add_directory_predicate_sources(
    output: &mut Vec<PredicateSource>,
    max_rows: usize,
) -> Result<(), Box<dyn Error>> {
    add_file_source(
        output,
        "analytical",
        "timestamp",
        "clickbench",
        "hits_timestamp",
        "eval/data/clickbench",
        "hits__c0_timestamp.csv",
        &[
            "hits__c2_RegionID.csv",
            "hits__c3_ResolutionWidth.csv",
            "hits__c8_SendTiming.csv",
        ],
        max_rows,
    )?;
    add_file_source(
        output,
        "analytical",
        "timestamp",
        "tpch",
        "lineitem_timestamp",
        "eval/data/tpch",
        "lineitem__c8_timestamp.csv",
        &[
            "lineitem__c3_l_linenumber.csv",
            "lineitem__c4_l_quantity.csv",
        ],
        max_rows,
    )?;
    add_file_source(
        output,
        "analytical",
        "identifier",
        "tpch",
        "lineitem_orderkey",
        "eval/data/tpch",
        "lineitem__c0_l_orderkey.csv",
        &[
            "lineitem__c3_l_linenumber.csv",
            "lineitem__c4_l_quantity.csv",
        ],
        max_rows,
    )?;

    let directory = study_data_path("eval/data/publicbi");
    for table in ["Arade", "Bimbo", "CMSprovider", "Euro2016", "Generico"] {
        let mut paths = csv_paths(&directory)?
            .into_iter()
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&format!("{table}__")))
            })
            .collect::<Vec<_>>();
        if paths.len() < 2 {
            continue;
        }
        paths.sort();
        let selector_path = paths.remove(0);
        let selector = load_bounded_column("publicbi", &selector_path, max_rows)?;
        let values = paths
            .into_iter()
            .map(|path| load_bounded_column("publicbi", &path, max_rows))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|column| column.exact_i64)
            .take(2)
            .collect();
        push_predicate_source(
            output,
            "transactional",
            "identifier",
            "publicbi",
            table,
            selector,
            values,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn add_file_source(
    output: &mut Vec<PredicateSource>,
    class: &'static str,
    selector_kind: &'static str,
    group: &str,
    source: &str,
    directory: &str,
    selector_file: &str,
    value_files: &[&str],
    max_rows: usize,
) -> Result<(), Box<dyn Error>> {
    let directory = study_data_path(directory);
    let selector_path = directory.join(selector_file);
    if !selector_path.exists() {
        return Ok(());
    }
    let selector = load_bounded_column(group, &selector_path, max_rows)?;
    let values = value_files
        .iter()
        .map(|file| load_bounded_column(group, &directory.join(file), max_rows))
        .collect::<Result<Vec<_>, _>>()?;
    push_predicate_source(
        output,
        class,
        selector_kind,
        group,
        source,
        selector,
        values,
    )
}

fn add_taxi_predicate_source(
    output: &mut Vec<PredicateSource>,
    max_rows: usize,
) -> Result<(), Box<dyn Error>> {
    let mut columns = yellow_columns(Some(max_rows))?;
    let Some(selector_index) = columns
        .iter()
        .position(|column| column.name == "tpep_pickup_datetime")
    else {
        return Ok(());
    };
    let selector = columns.remove(selector_index);
    let values = columns
        .into_iter()
        .filter(|column| {
            column.exact_i64
                && column.name != "tpep_dropoff_datetime"
                && column.len() == selector.len()
        })
        .take(2)
        .collect();
    push_predicate_source(
        output,
        "transactional",
        "timestamp",
        "taxi",
        "yellow_tripdata_2024-01",
        selector,
        values,
    )
}

fn push_predicate_source(
    output: &mut Vec<PredicateSource>,
    class: &'static str,
    selector_kind: &'static str,
    group: &str,
    source: &str,
    mut selector: Column,
    mut values: Vec<Column>,
) -> Result<(), Box<dyn Error>> {
    values.retain(|value| value.exact_i64 && value.len() == selector.len());
    if !selector.exact_i64 || selector.len() < 1_024 || values.is_empty() {
        return Ok(());
    }
    selector.group = group.to_string();
    selector.source = source.to_string();
    for value in &mut values {
        value.group = group.to_string();
        value.source = source.to_string();
    }
    output.push(PredicateSource {
        class,
        selector_kind,
        group: group.to_string(),
        source: source.to_string(),
        selector,
        values,
    });
    Ok(())
}

fn integer_column(
    group: &str,
    source: &str,
    name: &str,
    values: Vec<i64>,
    max_rows: usize,
) -> Column {
    Column {
        group: group.to_string(),
        source: source.to_string(),
        name: name.to_string(),
        mode: "exact_i64".to_string(),
        values: values.into_iter().take(max_rows).map(Some).collect(),
        float_values: None,
        exact_i64: true,
    }
}

fn load_bounded_column(
    group: &str,
    path: &Path,
    max_rows: usize,
) -> Result<Column, Box<dyn Error>> {
    single_column_file(
        group,
        path.to_str().ok_or("non-UTF8 column path")?,
        max_rows,
    )
}

fn csv_paths(directory: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut paths = fs::read_dir(directory)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("csv"))
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

fn directory_study_source(
    class: &'static str,
    group: &str,
    source: &str,
    timestamp_file: &str,
    directory: &str,
    max_rows: usize,
) -> Result<Option<StudySource>, Box<dyn Error>> {
    let directory = study_data_path(directory);
    let timestamp_path = directory.join(timestamp_file);
    if !timestamp_path.exists() {
        return Ok(None);
    }
    let timestamp = single_column_file(
        group,
        timestamp_path.to_str().ok_or("non-UTF8 timestamp path")?,
        max_rows,
    )?;
    let mut paths: Vec<PathBuf> = fs::read_dir(&directory)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("csv"))
        .filter(|path| path != &timestamp_path)
        .collect();
    paths.sort();
    let mut values = Vec::new();
    for path in paths {
        let value =
            single_column_file(group, path.to_str().ok_or("non-UTF8 value path")?, max_rows)?;
        if value.exact_i64 && value.len() == timestamp.len() {
            values.push(value);
        }
    }
    Ok(Some(make_source(class, group, source, timestamp, values)?))
}

fn yellow_taxi_study_source(max_rows: usize) -> Result<Option<StudySource>, Box<dyn Error>> {
    let columns = yellow_columns(Some(max_rows))?;
    let Some(timestamp) = columns
        .iter()
        .find(|column| column.name == "tpep_pickup_datetime")
        .cloned()
    else {
        return Ok(None);
    };
    let values = columns
        .into_iter()
        .filter(|column| {
            column.exact_i64
                && column.name != "tpep_pickup_datetime"
                && column.name != "tpep_dropoff_datetime"
        })
        .collect();
    Ok(Some(make_source(
        "transactional",
        "taxi",
        "yellow_tripdata_2024-01",
        timestamp,
        values,
    )?))
}

fn study_data_path(path: &str) -> PathBuf {
    let direct = PathBuf::from(path);
    if direct.exists() {
        direct
    } else {
        Path::new("experiments").join(path)
    }
}

fn single_column_file(group: &str, path: &str, max_rows: usize) -> Result<Column, Box<dyn Error>> {
    let path = Path::new(path);
    let stem = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("column");
    let (source, name) = stem.split_once("__").unwrap_or(("table", stem));
    let tokens: Vec<Option<String>> = fs::read_to_string(path)?
        .lines()
        .take(max_rows)
        .map(|line| {
            let token = line.trim();
            (!token.is_empty()).then(|| token.to_string())
        })
        .collect();
    Ok(column_from_tokens(group, source, name, &tokens))
}

/// RQ3 breadth corpora (distinct real domains beyond NAB + Taxi):
/// household-power (UCI residential power, real 3-decimal sensor floats +
/// integer sub-metering) and UCR ECG5000 (z-normalized ML time series —
/// expected to abstain, the honest "structure absent" data point).
fn breadth_columns() -> Result<Vec<Column>, Box<dyn Error>> {
    let mut out = Vec::new();
    out.extend(household_power_columns()?);
    out.extend(ucr_ecg5000_columns()?);
    out.extend(dir_columns("publicbi", "eval/data/publicbi")?);
    // Canonical Parquet-benchmark corpora (advisor request, 2026-07): TPC-H
    // SF1 lineitem and a documented 1M-row ClickBench hits subset, exported by
    // scripts/fetch_bench_data.py into the same single-column layout.
    out.extend(dir_columns("tpch", "eval/data/tpch")?);
    out.extend(dir_columns("clickbench", "eval/data/clickbench")?);
    Ok(out)
}

/// Directory corpus: one token-per-line numeric column per file, filenames
/// `<Table>__c<i>_<name>.csv` (Public BI layout; also used by the TPC-H and
/// ClickBench exports). Columns named `timestamp` carry pre-converted epoch
/// seconds and enter the timestamp path; everything else goes through the
/// verified-scaling rule (column_from_tokens) like every other corpus.
fn dir_columns(group: &str, dir: &str) -> Result<Vec<Column>, Box<dyn Error>> {
    let dir = study_data_path(dir);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut paths: Vec<_> = fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("csv"))
        .collect();
    paths.sort();
    let mut out = Vec::new();
    for path in paths {
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("col");
        let (table, col) = stem.split_once("__").unwrap_or(("table", stem));
        let toks: Vec<Option<String>> = fs::read_to_string(&path)?
            .lines()
            .map(|l| {
                let t = l.trim();
                if t.is_empty() {
                    None
                } else {
                    Some(t.to_string())
                }
            })
            .collect();
        if toks.len() < 128 {
            continue;
        }
        if col.ends_with("_timestamp") || col == "timestamp" {
            let values: Vec<Option<i64>> = toks
                .iter()
                .map(|t| t.as_ref().and_then(|s| s.parse::<i64>().ok()))
                .collect();
            out.push(Column {
                group: group.to_string(),
                source: table.to_string(),
                name: "timestamp".to_string(),
                mode: "timestamp_epoch_s".to_string(),
                values,
                float_values: None,
                exact_i64: true,
            });
        } else {
            out.push(column_from_tokens(group, table, col, &toks));
        }
    }
    Ok(out)
}

const HOUSEHOLD_PATH: &str = "eval/data/household_power_consumption.txt";
const ECG_TRAIN: &str = "eval/data/ucr_ecg5000/ECG5000_TRAIN.txt";
const ECG_TEST: &str = "eval/data/ucr_ecg5000/ECG5000_TEST.txt";

fn household_power_columns() -> Result<Vec<Column>, Box<dyn Error>> {
    household_power_columns_limited(None)
}

fn household_power_columns_limited(max_rows: Option<usize>) -> Result<Vec<Column>, Box<dyn Error>> {
    let path = study_data_path(HOUSEHOLD_PATH);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(path)?;
    let names = [
        "global_active_power",
        "global_reactive_power",
        "voltage",
        "global_intensity",
        "sub_metering_1",
        "sub_metering_2",
        "sub_metering_3",
    ];
    let mut cols: Vec<Vec<Option<String>>> = vec![Vec::new(); names.len()];
    let mut ts: Vec<i64> = Vec::new();
    for line in text.lines().skip(1) {
        if max_rows.is_some_and(|limit| ts.len() >= limit) {
            break;
        }
        let f: Vec<&str> = line.split(';').collect();
        if f.len() < 9 {
            continue;
        }
        // timestamp from Date(d/m/Y) + Time(H:M:S)
        if let Ok(t) = parse_household_ts(f[0], f[1]) {
            ts.push(t);
        } else {
            continue;
        }
        for (j, c) in cols.iter_mut().enumerate() {
            let tok = f[2 + j].trim();
            c.push(if tok == "?" || tok.is_empty() {
                None
            } else {
                Some(tok.to_string())
            });
        }
    }
    let mut out = vec![Column {
        group: "household".to_string(),
        source: "uci_household_power".to_string(),
        name: "timestamp".to_string(),
        mode: "timestamp_epoch_s".to_string(),
        values: ts.into_iter().map(Some).collect(),
        float_values: None,
        exact_i64: true,
    }];
    for (name, toks) in names.iter().zip(cols) {
        out.push(column_from_tokens(
            "household",
            "uci_household_power",
            name,
            &toks,
        ));
    }
    Ok(out)
}

fn parse_household_ts(date: &str, time: &str) -> Result<i64, Box<dyn Error>> {
    let dt = NaiveDateTime::parse_from_str(&format!("{date} {time}"), "%d/%m/%Y %H:%M:%S")?;
    Ok(Utc.from_utc_datetime(&dt).timestamp())
}

/// Build a column from decimal-text tokens (None = missing): verified
/// fixed-point if all present tokens are plain decimals with ≤ MAX_DECIMAL_SCALE
/// fractional digits, else float-native (abstain). the measurement protocol.
fn column_from_tokens(group: &str, source: &str, name: &str, toks: &[Option<String>]) -> Column {
    let mut scale = 0u32;
    let mut plain = true;
    for t in toks.iter().flatten() {
        match text_decimals(t) {
            Some(d) if d <= MAX_DECIMAL_SCALE => scale = scale.max(d),
            _ => {
                plain = false;
                break;
            }
        }
    }
    if plain {
        let factor = scale;
        let values: Vec<Option<i64>> = toks
            .iter()
            .map(|t| {
                t.as_ref()
                    .and_then(|s| parse_scaled_decimal(s, factor).ok())
            })
            .collect();
        let s = 10i64.pow(scale);
        Column {
            group: group.into(),
            source: source.into(),
            name: format!("{name}_x{s}"),
            mode: format!("value_scaled_x{s}_verified"),
            values,
            float_values: None,
            exact_i64: true,
        }
    } else {
        let floats: Vec<Option<f64>> = toks
            .iter()
            .map(|t| t.as_ref().and_then(|s| s.parse::<f64>().ok()))
            .collect();
        Column {
            group: group.into(),
            source: source.into(),
            name: name.into(),
            mode: format!("float_native_no_scale_le_1e{MAX_DECIMAL_SCALE}"),
            values: Vec::new(),
            float_values: Some(floats),
            exact_i64: false,
        }
    }
}

fn ucr_ecg5000_columns() -> Result<Vec<Column>, Box<dyn Error>> {
    if !Path::new(ECG_TRAIN).exists() {
        return Ok(Vec::new());
    }
    // Flatten the 140-point series across all instances (drop the class label
    // in column 0) into one numeric column — the columnar view of the dataset.
    let mut flat: Vec<Option<String>> = Vec::new();
    for path in [ECG_TRAIN, ECG_TEST] {
        if !Path::new(path).exists() {
            continue;
        }
        for line in fs::read_to_string(path)?.lines() {
            for tok in line.split_whitespace().skip(1) {
                flat.push(Some(tok.to_string()));
            }
        }
    }
    if flat.is_empty() {
        return Ok(Vec::new());
    }
    Ok(vec![column_from_tokens(
        "ucr",
        "ECG5000",
        "series_flat",
        &flat,
    )])
}

fn small_columns() -> Result<Vec<Column>, Box<dyn Error>> {
    let mut out = Vec::new();
    for (source, path) in [
        ("nyc_taxi.csv", "eval/data/realKnownCause__nyc_taxi.csv"),
        (
            "Twitter_volume_AAPL.csv",
            "eval/data/realTweets__Twitter_volume_AAPL.csv",
        ),
        (
            "Twitter_volume_GOOG.csv",
            "eval/data/realTweets__Twitter_volume_GOOG.csv",
        ),
        (
            "Twitter_volume_IBM.csv",
            "eval/data/realTweets__Twitter_volume_IBM.csv",
        ),
        (
            "Twitter_volume_AMZN.csv",
            "eval/data/realTweets__Twitter_volume_AMZN.csv",
        ),
        (
            "Twitter_volume_CRM.csv",
            "eval/data/realTweets__Twitter_volume_CRM.csv",
        ),
        (
            "ambient_temperature_system_failure.csv",
            "eval/data/realKnownCause__ambient_temperature_system_failure.csv",
        ),
        (
            "art_daily_small_noise.csv",
            "eval/data/artificialNoAnomaly__art_daily_small_noise.csv",
        ),
        (
            "ec2_cpu_utilization_5f5533.csv",
            "eval/data/realAWSCloudwatch__ec2_cpu_utilization_5f5533.csv",
        ),
    ] {
        if !Path::new(path).exists() {
            continue;
        }
        let (timestamps, values) = read_nab_csv(path)?;
        out.push(Column {
            group: "small".to_string(),
            source: source.to_string(),
            name: "timestamp".to_string(),
            mode: "timestamp_epoch_s".to_string(),
            values: timestamps.into_iter().map(Some).collect(),
            float_values: None,
            exact_i64: true,
        });
        out.push(match values {
            NabValues::Exact { values, scale } => Column {
                group: "small".to_string(),
                source: source.to_string(),
                name: format!("value_x{scale}"),
                mode: format!("value_scaled_x{scale}_verified"),
                values: values.into_iter().map(Some).collect(),
                float_values: None,
                exact_i64: true,
            },
            NabValues::Float(floats) => Column {
                group: "small".to_string(),
                source: source.to_string(),
                name: "value".to_string(),
                mode: format!("float_native_no_scale_le_1e{MAX_DECIMAL_SCALE}"),
                values: Vec::new(),
                float_values: Some(floats.into_iter().map(Some).collect()),
                exact_i64: false,
            },
        });
    }
    out.extend(synthetic_columns("small", 4096));
    out.extend(synthetic_columns("medium", 262_144));
    out.extend(synthetic_columns("large_synth", 1_048_576));
    Ok(out)
}

/// NAB values parsed exactly from the decimal text (no float multiply), so the
/// scaled integers are lossless by construction. Columns whose text needs more
/// than MAX_DECIMAL_SCALE fractional digits come back as floats instead
/// (the measurement protocol: never silently quantize).
fn read_nab_csv(path: &str) -> Result<(Vec<i64>, NabValues), Box<dyn Error>> {
    let mut timestamps = Vec::new();
    let mut tokens = Vec::new();
    for line in fs::read_to_string(path)?.lines().skip(1) {
        let Some((ts, value)) = line.split_once(',') else {
            continue;
        };
        timestamps.push(parse_ts(ts)?);
        tokens.push(value.trim().to_string());
    }

    let mut decimals = 0u32;
    let mut plain = true;
    for t in &tokens {
        match text_decimals(t) {
            Some(d) => decimals = decimals.max(d),
            None => {
                plain = false;
                break;
            }
        }
    }

    if plain && decimals <= MAX_DECIMAL_SCALE {
        let scale = 10i64.pow(decimals);
        let values = tokens
            .iter()
            .map(|t| parse_scaled_decimal(t, decimals))
            .collect::<Result<Vec<i64>, _>>()?;
        return Ok((timestamps, NabValues::Exact { values, scale }));
    }

    let floats = tokens
        .iter()
        .map(|t| t.parse::<f64>())
        .collect::<Result<Vec<f64>, _>>()?;
    Ok((timestamps, NabValues::Float(floats)))
}

enum NabValues {
    Exact { values: Vec<i64>, scale: i64 },
    Float(Vec<f64>),
}

const MAX_DECIMAL_SCALE: u32 = 4;

/// Fractional digits of a plain decimal token (trailing zeros dropped); None
/// for scientific notation or anything else unparseable as plain decimal.
fn text_decimals(t: &str) -> Option<u32> {
    if t.is_empty() || t.contains(['e', 'E']) {
        return None;
    }
    let body = t.strip_prefix('-').unwrap_or(t);
    match body.split_once('.') {
        None => body.chars().all(|c| c.is_ascii_digit()).then_some(0),
        Some((int, frac)) => (int.chars().all(|c| c.is_ascii_digit())
            && frac.chars().all(|c| c.is_ascii_digit()))
        .then(|| frac.trim_end_matches('0').len() as u32),
    }
}

fn parse_scaled_decimal(t: &str, decimals: u32) -> Result<i64, Box<dyn Error>> {
    let negative = t.starts_with('-');
    let body = t.strip_prefix('-').unwrap_or(t);
    let (int, frac) = body.split_once('.').unwrap_or((body, ""));
    let mut digits = String::with_capacity(int.len() + decimals as usize);
    digits.push_str(int);
    let frac = frac.trim_end_matches('0');
    digits.push_str(frac);
    for _ in frac.len() as u32..decimals {
        digits.push('0');
    }
    let magnitude: i64 = if digits.is_empty() {
        0
    } else {
        digits.parse()?
    };
    Ok(if negative { -magnitude } else { magnitude })
}

fn parse_ts(s: &str) -> Result<i64, Box<dyn Error>> {
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M:%S%.f"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Ok(Utc.from_utc_datetime(&dt).timestamp());
        }
    }
    Err(format!("unparseable timestamp: {s}").into())
}

fn synthetic_columns(group: &str, n: usize) -> Vec<Column> {
    let specs: Vec<(&str, Vec<Option<i64>>)> = vec![
        ("constant", vec![Some(42); n]),
        ("linear", (0..n).map(|i| Some(20 + 2 * i as i64)).collect()),
        (
            "quadratic",
            (0..n)
                .map(|i| Some(i as i64 * i as i64 + 2 * i as i64 + 4))
                .collect(),
        ),
        (
            "small_domain",
            (0..n).map(|i| Some([4, 9, 12, 15][i % 4])).collect(),
        ),
        (
            "sparse_zero",
            (0..n)
                .map(|i| Some(if i % 73 == 0 { 10_000 + i as i64 } else { 0 }))
                .collect(),
        ),
        (
            "nullable_fee",
            (0..n)
                .map(|i| {
                    if i % 17 == 0 {
                        None
                    } else if i % 23 == 0 {
                        Some(250)
                    } else {
                        Some(0)
                    }
                })
                .collect(),
        ),
        ("noisy_lcg", lcg_values(n)),
    ];

    specs
        .into_iter()
        .map(|(name, values)| Column {
            group: group.to_string(),
            source: "synthetic".to_string(),
            name: name.to_string(),
            mode: "synthetic_i64".to_string(),
            values,
            float_values: None,
            exact_i64: true,
        })
        .collect()
}

fn lcg_values(n: usize) -> Vec<Option<i64>> {
    let mut x = 42u64;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        out.push(Some((x % 1_000_000) as i64));
    }
    out
}

fn large_columns() -> Result<Vec<Column>, Box<dyn Error>> {
    yellow_columns(None)
}

fn yellow_columns(max_rows: Option<usize>) -> Result<Vec<Column>, Box<dyn Error>> {
    let path = study_data_path(YELLOW_PARQUET);
    if !path.exists() {
        return Ok(Vec::new());
    }

    let file = File::open(path)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    let schema = builder.schema().clone();
    let mut cols: Vec<Option<Column>> = schema
        .fields()
        .iter()
        .map(|field| {
            yellow_mode(field.data_type()).map(|mode| {
                let is_float = matches!(field.data_type(), DataType::Float64);
                Column {
                    group: "large".to_string(),
                    source: "yellow_tripdata_2024-01".to_string(),
                    name: field.name().to_string(),
                    mode,
                    values: Vec::new(),
                    float_values: is_float.then(Vec::new),
                    exact_i64: !is_float,
                }
            })
        })
        .collect();

    let reader = builder.with_batch_size(65_536).build()?;
    for batch in reader {
        let batch = batch?;
        for (idx, col) in cols.iter_mut().enumerate() {
            let Some(col) = col else {
                continue;
            };
            append_arrow_values(col, batch.column(idx).as_ref())?;
        }
        if max_rows.is_some_and(|limit| {
            cols.iter()
                .flatten()
                .next()
                .is_some_and(|column| column.len() >= limit)
        }) {
            break;
        }
    }

    let mut out: Vec<Column> = cols.into_iter().flatten().collect();
    for col in &mut out {
        if let Some(limit) = max_rows {
            col.values.truncate(limit);
            if let Some(values) = &mut col.float_values {
                values.truncate(limit);
            }
        }
        resolve_float_column(col);
    }
    Ok(out)
}

fn yellow_mode(dt: &DataType) -> Option<String> {
    match dt {
        DataType::Int32 | DataType::Int64 => Some(format!("{dt:?}")),
        DataType::Timestamp(_, _) => Some("timestamp".to_string()),
        DataType::Float64 => Some("float64_pending_scale".to_string()),
        _ => None,
    }
}

/// Verified decimal scaling (the measurement protocol): adopt the smallest scale 10^s,
/// s <= MAX_DECIMAL_SCALE, such that round(v*10^s) reconstructs every non-null
/// value exactly (`==` on f64). Otherwise the column stays float-native and the
/// the integer codecs abstain on it.
fn resolve_float_column(col: &mut Column) {
    let Some(floats) = &col.float_values else {
        return;
    };
    'scales: for s in 0..=MAX_DECIMAL_SCALE {
        let scale = 10f64.powi(s as i32);
        let mut ints = Vec::with_capacity(floats.len());
        for v in floats {
            match v {
                None => ints.push(None),
                Some(v) => {
                    let scaled = (v * scale).round();
                    if !v.is_finite()
                        || scaled < i64::MIN as f64
                        || scaled > i64::MAX as f64
                        || scaled / scale != *v
                    {
                        continue 'scales;
                    }
                    ints.push(Some(scaled as i64));
                }
            }
        }
        col.values = ints;
        col.float_values = None;
        col.exact_i64 = true;
        col.mode = format!("float_scaled_x{}_verified", 10i64.pow(s));
        return;
    }
    col.mode = format!("float_native_no_scale_le_1e{MAX_DECIMAL_SCALE}");
}

fn append_arrow_values(col: &mut Column, array: &dyn Array) -> Result<(), Box<dyn Error>> {
    match array.data_type() {
        DataType::Int32 => {
            let arr = array
                .as_any()
                .downcast_ref::<Int32Array>()
                .ok_or("bad Int32Array")?;
            for i in 0..arr.len() {
                col.values
                    .push((!arr.is_null(i)).then(|| arr.value(i) as i64));
            }
        }
        DataType::Int64 => {
            let arr = array
                .as_any()
                .downcast_ref::<Int64Array>()
                .ok_or("bad Int64Array")?;
            for i in 0..arr.len() {
                col.values.push((!arr.is_null(i)).then(|| arr.value(i)));
            }
        }
        DataType::Timestamp(TimeUnit::Microsecond, _) => {
            let arr = array
                .as_any()
                .downcast_ref::<TimestampMicrosecondArray>()
                .ok_or("bad TimestampMicrosecondArray")?;
            for i in 0..arr.len() {
                col.values.push((!arr.is_null(i)).then(|| arr.value(i)));
            }
        }
        DataType::Float64 => {
            let arr = array
                .as_any()
                .downcast_ref::<Float64Array>()
                .ok_or("bad Float64Array")?;
            let floats = col.float_values.as_mut().ok_or("float column expected")?;
            for i in 0..arr.len() {
                floats.push((!arr.is_null(i)).then(|| arr.value(i)));
            }
        }
        _ => {}
    }
    Ok(())
}
