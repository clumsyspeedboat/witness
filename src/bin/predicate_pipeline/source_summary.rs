use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::error::Error;

type Cell = (usize, String);

#[derive(Clone)]
struct Pair {
    class: String,
    selector_kind: String,
    group: String,
    source: String,
    non_decreasing: bool,
    rows: usize,
    direct_file_ratio: f64,
}

#[derive(Clone, Copy)]
struct Measurement {
    median_ns: f64,
    delivered_bytes: f64,
    selector_values_read: f64,
    boundary_order_used: bool,
}

#[derive(Default)]
struct SourceStats {
    pairs: BTreeSet<usize>,
    cells: usize,
    selectivity: Vec<f64>,
    known_ratio: Vec<f64>,
    filter_ratio: Vec<f64>,
    boundary_ratio: Vec<f64>,
    equal_page_boundary_ratio: Vec<f64>,
    direct_boundary_ratio: Vec<f64>,
    direct_equal_page_boundary_ratio: Vec<f64>,
    direct_storage_ratio: Vec<f64>,
    direct_file_ratio: Vec<f64>,
    boundary_byte_ratio: Vec<f64>,
    direct_boundary_byte_ratio: Vec<f64>,
    generated_selector_fraction: Vec<f64>,
    direct_selector_fraction: Vec<f64>,
    boundary_selector_fraction: Vec<f64>,
    boundary_wins: usize,
    direct_boundary_wins: usize,
    direct_equal_page_wins: usize,
    boundary_order_used: bool,
}

pub fn write(result_dir: &str) -> Result<(), Box<dyn Error>> {
    let pairs = read_pairs(&format!("{result_dir}/pairs.csv"))?;
    let predicates = read_predicates(&format!("{result_dir}/predicates.csv"))?;
    let known = read_measurements(&format!("{result_dir}/known_selection.csv"))?;
    let complete = read_measurements(&format!("{result_dir}/complete_query.csv"))?;
    let mut sources: BTreeMap<(String, String, String), SourceStats> = BTreeMap::new();
    for (cell, selectivity) in predicates {
        let pair = pairs
            .get(&cell.0)
            .ok_or("predicate references unknown pair")?;
        let known_cell = known.get(&cell).ok_or("missing known-selection cell")?;
        let complete_cell = complete.get(&cell).ok_or("missing complete-query cell")?;
        let generated_known = measurement(known_cell, "generated_selective")?;
        let parquet_known = measurement(known_cell, "parquet_row_selection")?;
        let generated = measurement(complete_cell, "generated_selective")?;
        let generated_direct = measurement(complete_cell, "generated_direct_selective")?;
        let parquet_filter = measurement(complete_cell, "parquet_row_filter")?;
        let parquet_boundary = measurement(complete_cell, "parquet_boundary_search")?;
        let parquet_equal = measurement(complete_cell, "parquet_boundary_search_p16384")?;
        let stats = sources
            .entry((pair.class.clone(), pair.group.clone(), pair.source.clone()))
            .or_default();
        stats.pairs.insert(cell.0);
        stats.cells += 1;
        stats.selectivity.push(selectivity);
        stats
            .known_ratio
            .push(generated_known.median_ns / parquet_known.median_ns);
        stats
            .filter_ratio
            .push(generated.median_ns / parquet_filter.median_ns);
        stats
            .boundary_ratio
            .push(generated.median_ns / parquet_boundary.median_ns);
        stats
            .equal_page_boundary_ratio
            .push(generated.median_ns / parquet_equal.median_ns);
        stats
            .direct_boundary_ratio
            .push(generated_direct.median_ns / parquet_boundary.median_ns);
        stats
            .direct_equal_page_boundary_ratio
            .push(generated_direct.median_ns / parquet_equal.median_ns);
        stats
            .direct_storage_ratio
            .push(generated_direct.median_ns / generated.median_ns);
        stats.direct_file_ratio.push(pair.direct_file_ratio);
        stats
            .boundary_byte_ratio
            .push(generated.delivered_bytes / parquet_boundary.delivered_bytes);
        stats
            .direct_boundary_byte_ratio
            .push(generated_direct.delivered_bytes / parquet_boundary.delivered_bytes);
        stats
            .generated_selector_fraction
            .push(generated.selector_values_read / pair.rows as f64);
        stats
            .direct_selector_fraction
            .push(generated_direct.selector_values_read / pair.rows as f64);
        stats
            .boundary_selector_fraction
            .push(parquet_boundary.selector_values_read / pair.rows as f64);
        stats.boundary_wins += usize::from(generated.median_ns < parquet_boundary.median_ns);
        stats.direct_boundary_wins +=
            usize::from(generated_direct.median_ns < parquet_boundary.median_ns);
        stats.direct_equal_page_wins +=
            usize::from(generated_direct.median_ns < parquet_equal.median_ns);
        stats.boundary_order_used |= parquet_boundary.boundary_order_used;
    }

    let mut output = csv::Writer::from_path(format!("{result_dir}/source_summary.csv"))?;
    output.write_record([
        "class",
        "selector_kind",
        "group",
        "source",
        "non_decreasing",
        "pairs",
        "predicate_cells",
        "selectivity_min",
        "selectivity_median",
        "selectivity_max",
        "known_gen_over_parquet_median",
        "complete_gen_over_parquet_filter_median",
        "complete_gen_over_parquet_boundary_median",
        "complete_gen_over_equal_page_parquet_boundary_median",
        "complete_direct_gen_over_parquet_boundary_median",
        "complete_direct_gen_over_equal_page_parquet_boundary_median",
        "complete_direct_gen_over_storage_gen_median",
        "direct_gen_over_storage_gen_file_bytes_median",
        "gen_delivered_over_parquet_boundary_median",
        "direct_gen_delivered_over_parquet_boundary_median",
        "gen_selector_primitive_values_fraction_median",
        "direct_gen_selector_primitive_values_fraction_median",
        "parquet_boundary_selector_values_fraction_median",
        "gen_boundary_wins",
        "direct_gen_boundary_wins",
        "direct_gen_equal_page_boundary_wins",
        "boundary_order_used",
    ])?;
    for ((class, group, source), stats) in &sources {
        let pair = pairs
            .get(stats.pairs.first().ok_or("source has no pairs")?)
            .ok_or("source pair missing")?;
        output.write_record([
            class,
            &pair.selector_kind,
            group,
            source,
            &pair.non_decreasing.to_string(),
            &stats.pairs.len().to_string(),
            &stats.cells.to_string(),
            &format!("{:.8}", minimum(&stats.selectivity)),
            &format!("{:.8}", median(&stats.selectivity)),
            &format!("{:.8}", maximum(&stats.selectivity)),
            &format!("{:.4}", median(&stats.known_ratio)),
            &format!("{:.4}", median(&stats.filter_ratio)),
            &format!("{:.4}", median(&stats.boundary_ratio)),
            &format!("{:.4}", median(&stats.equal_page_boundary_ratio)),
            &format!("{:.4}", median(&stats.direct_boundary_ratio)),
            &format!("{:.4}", median(&stats.direct_equal_page_boundary_ratio)),
            &format!("{:.4}", median(&stats.direct_storage_ratio)),
            &format!("{:.4}", median(&stats.direct_file_ratio)),
            &format!("{:.4}", median(&stats.boundary_byte_ratio)),
            &format!("{:.4}", median(&stats.direct_boundary_byte_ratio)),
            &format!("{:.4}", median(&stats.generated_selector_fraction)),
            &format!("{:.4}", median(&stats.direct_selector_fraction)),
            &format!("{:.4}", median(&stats.boundary_selector_fraction)),
            &stats.boundary_wins.to_string(),
            &stats.direct_boundary_wins.to_string(),
            &stats.direct_equal_page_wins.to_string(),
            &stats.boundary_order_used.to_string(),
        ])?;
    }
    output.flush()?;
    write_certificate_summary(result_dir, &pairs, &sources)
}

fn write_certificate_summary(
    result_dir: &str,
    pairs: &HashMap<usize, Pair>,
    sources: &BTreeMap<(String, String, String), SourceStats>,
) -> Result<(), Box<dyn Error>> {
    let mut groups: BTreeMap<bool, Vec<&SourceStats>> = BTreeMap::new();
    for stats in sources.values() {
        let pair = pairs.get(stats.pairs.first().unwrap()).unwrap();
        groups.entry(pair.non_decreasing).or_default().push(stats);
    }
    let mut output = csv::Writer::from_path(format!("{result_dir}/certificate_summary.csv"))?;
    output.write_record([
        "non_decreasing",
        "sources",
        "pairs",
        "predicate_cells",
        "source_median_gen_over_parquet_filter",
        "source_median_gen_over_parquet_boundary",
        "source_median_gen_over_equal_page_parquet_boundary",
        "source_median_direct_gen_over_parquet_boundary",
        "source_median_direct_gen_over_equal_page_parquet_boundary",
        "source_median_gen_delivered_over_parquet_boundary",
        "sources_beating_parquet_boundary",
        "direct_sources_beating_parquet_boundary",
        "direct_sources_beating_equal_page_parquet_boundary",
    ])?;
    for (certificate, group) in groups {
        let filter = group
            .iter()
            .map(|stats| median(&stats.filter_ratio))
            .collect::<Vec<_>>();
        let boundary = group
            .iter()
            .map(|stats| median(&stats.boundary_ratio))
            .collect::<Vec<_>>();
        let bytes = group
            .iter()
            .map(|stats| median(&stats.boundary_byte_ratio))
            .collect::<Vec<_>>();
        let equal_page = group
            .iter()
            .map(|stats| median(&stats.equal_page_boundary_ratio))
            .collect::<Vec<_>>();
        let direct = group
            .iter()
            .map(|stats| median(&stats.direct_boundary_ratio))
            .collect::<Vec<_>>();
        let direct_equal = group
            .iter()
            .map(|stats| median(&stats.direct_equal_page_boundary_ratio))
            .collect::<Vec<_>>();
        output.write_record([
            certificate.to_string(),
            group.len().to_string(),
            group
                .iter()
                .map(|stats| stats.pairs.len())
                .sum::<usize>()
                .to_string(),
            group
                .iter()
                .map(|stats| stats.cells)
                .sum::<usize>()
                .to_string(),
            format!("{:.4}", median(&filter)),
            format!("{:.4}", median(&boundary)),
            format!("{:.4}", median(&equal_page)),
            format!("{:.4}", median(&direct)),
            format!("{:.4}", median(&direct_equal)),
            format!("{:.4}", median(&bytes)),
            group
                .iter()
                .filter(|stats| median(&stats.boundary_ratio) < 1.0)
                .count()
                .to_string(),
            group
                .iter()
                .filter(|stats| median(&stats.direct_boundary_ratio) < 1.0)
                .count()
                .to_string(),
            group
                .iter()
                .filter(|stats| median(&stats.direct_equal_page_boundary_ratio) < 1.0)
                .count()
                .to_string(),
        ])?;
    }
    output.flush()?;
    Ok(())
}

fn read_pairs(path: &str) -> Result<HashMap<usize, Pair>, Box<dyn Error>> {
    let mut reader = csv::Reader::from_path(path)?;
    let headers = reader.headers()?.clone();
    let mut output = HashMap::new();
    for record in reader.records() {
        let record = record?;
        output.insert(
            field(&headers, &record, "pair")?.parse()?,
            Pair {
                class: field(&headers, &record, "class")?.into(),
                selector_kind: field(&headers, &record, "selector_kind")?.into(),
                group: field(&headers, &record, "group")?.into(),
                source: field(&headers, &record, "source")?.into(),
                non_decreasing: field(&headers, &record, "selector_non_decreasing")?.parse()?,
                rows: field(&headers, &record, "rows")?.parse()?,
                direct_file_ratio: field(&headers, &record, "direct_gen_bytes")?.parse::<f64>()?
                    / field(&headers, &record, "gen_bytes")?.parse::<f64>()?,
            },
        );
    }
    Ok(output)
}

fn read_predicates(path: &str) -> Result<BTreeMap<Cell, f64>, Box<dyn Error>> {
    let mut reader = csv::Reader::from_path(path)?;
    let headers = reader.headers()?.clone();
    let mut output = BTreeMap::new();
    for record in reader.records() {
        let record = record?;
        output.insert(
            (
                field(&headers, &record, "pair")?.parse()?,
                field(&headers, &record, "predicate")?.into(),
            ),
            field(&headers, &record, "actual_selectivity")?.parse()?,
        );
    }
    Ok(output)
}

fn read_measurements(
    path: &str,
) -> Result<HashMap<Cell, HashMap<String, Measurement>>, Box<dyn Error>> {
    let mut reader = csv::Reader::from_path(path)?;
    let headers = reader.headers()?.clone();
    let mut output: HashMap<Cell, HashMap<String, Measurement>> = HashMap::new();
    for record in reader.records() {
        let record = record?;
        output
            .entry((
                field(&headers, &record, "pair")?.parse()?,
                field(&headers, &record, "predicate")?.into(),
            ))
            .or_default()
            .insert(
                field(&headers, &record, "baseline")?.into(),
                Measurement {
                    median_ns: field(&headers, &record, "median_ns")?.parse()?,
                    delivered_bytes: field(&headers, &record, "delivered_bytes")?.parse()?,
                    selector_values_read: field(
                        &headers,
                        &record,
                        "selector_primitive_values_read",
                    )?
                    .parse()?,
                    boundary_order_used: field(&headers, &record, "boundary_order_used")?
                        .parse()?,
                },
            );
    }
    Ok(output)
}

fn measurement(
    values: &HashMap<String, Measurement>,
    baseline: &str,
) -> Result<Measurement, Box<dyn Error>> {
    values
        .get(baseline)
        .copied()
        .ok_or_else(|| format!("missing {baseline}").into())
}

fn field<'a>(
    headers: &csv::StringRecord,
    record: &'a csv::StringRecord,
    name: &str,
) -> Result<&'a str, Box<dyn Error>> {
    let index = headers
        .iter()
        .position(|header| header == name)
        .ok_or_else(|| format!("missing CSV column {name}"))?;
    record
        .get(index)
        .ok_or_else(|| format!("missing value for {name}").into())
}

/// Standard median: the mean of the two central order statistics when the
/// sample size is even. Every source in the study has an even cell count, so
/// taking a single central element would bias every per-source estimate.
fn median(values: &[f64]) -> f64 {
    let mut values = values.to_vec();
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len() % 2 == 1 {
        values[middle]
    } else {
        (values[middle - 1] + values[middle]) / 2.0
    }
}

fn minimum(values: &[f64]) -> f64 {
    values.iter().copied().min_by(f64::total_cmp).unwrap()
}

fn maximum(values: &[f64]) -> f64 {
    values.iter().copied().max_by(f64::total_cmp).unwrap()
}

#[cfg(test)]
mod tests {
    use super::median;

    #[test]
    fn median_averages_the_two_central_values_when_even() {
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), 2.5);
    }

    #[test]
    fn median_takes_the_centre_when_odd() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
    }

    #[test]
    fn median_handles_duplicates() {
        assert_eq!(median(&[3.0, 1.0, 1.0, 3.0]), 2.0);
    }

    /// Guards the defect directly: the upper central element is not the median.
    #[test]
    fn median_is_not_the_upper_central_order_statistic() {
        let sample = [1.00, 0.60, 0.20, 0.40];
        assert_eq!(median(&sample), 0.5);
        let mut sorted = sample;
        sorted.sort_by(f64::total_cmp);
        assert_ne!(median(&sample), sorted[sorted.len() / 2]);
    }
}
