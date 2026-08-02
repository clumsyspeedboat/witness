use std::collections::{BTreeMap, HashMap};
use std::error::Error;

type Key = (usize, String);
type Timings = HashMap<String, f64>;

pub fn write(result_dir: &str) -> Result<(), Box<dyn Error>> {
    let predicates = read_predicates(&format!("{result_dir}/predicates.csv"))?;
    let known = read_timings(&format!("{result_dir}/known_selection.csv"))?;
    let complete = read_timings(&format!("{result_dir}/complete_query.csv"))?;
    let mut diagnostics = csv::Writer::from_path(format!("{result_dir}/diagnostics.csv"))?;
    diagnostics.write_record([
        "pair",
        "predicate",
        "actual_selectivity",
        "known_gen_ns",
        "known_direct_gen_ns",
        "known_fused_ns",
        "known_parquet_ns",
        "complete_gen_ns",
        "complete_direct_gen_ns",
        "complete_fused_ns",
        "complete_scan_ns",
        "parquet_full_ns",
        "parquet_filter_ns",
        "parquet_boundary_ns",
        "parquet_boundary_p4096_ns",
        "parquet_boundary_p16384_ns",
        "known_gen_over_parquet",
        "known_direct_gen_over_parquet",
        "complete_gen_over_scan",
        "complete_gen_over_parquet_full",
        "complete_gen_over_parquet_filter",
        "complete_gen_over_parquet_boundary",
        "complete_gen_over_parquet_boundary_p4096",
        "complete_gen_over_parquet_boundary_p16384",
        "complete_direct_gen_over_parquet_boundary",
        "complete_direct_gen_over_parquet_boundary_p16384",
        "complete_direct_gen_over_storage_gen",
        "discovery_multiplier",
        "discovery_share",
        "direct_discovery_share",
    ])?;

    let mut metrics = Metrics::default();
    for (key, actual) in &predicates {
        let known = known.get(key).ok_or("missing known-selection result")?;
        let complete = complete.get(key).ok_or("missing complete-query result")?;
        let kg = value(known, "generated_selective")?;
        let kdg = value(known, "generated_direct_selective")?;
        let kf = value(known, "generated_fused")?;
        let kp = value(known, "parquet_row_selection")?;
        let cg = value(complete, "generated_selective")?;
        let cdg = value(complete, "generated_direct_selective")?;
        let cf = value(complete, "generated_fused")?;
        let cs = value(complete, "generated_scan_selective")?;
        let pf = value(complete, "parquet_full")?;
        let pr = value(complete, "parquet_row_filter")?;
        let pb = value(complete, "parquet_boundary_search")?;
        let pb4 = value(complete, "parquet_boundary_search_p4096")?;
        let pb16 = value(complete, "parquet_boundary_search_p16384")?;
        metrics.observe(kg, kdg, kf, kp, cg, cdg, cf, cs, pf, pr, pb, pb4, pb16);
        diagnostics.write_record([
            key.0.to_string(),
            key.1.clone(),
            format!("{actual:.8}"),
            format!("{kg:.1}"),
            format!("{kdg:.1}"),
            format!("{kf:.1}"),
            format!("{kp:.1}"),
            format!("{cg:.1}"),
            format!("{cdg:.1}"),
            format!("{cf:.1}"),
            format!("{cs:.1}"),
            format!("{pf:.1}"),
            format!("{pr:.1}"),
            format!("{pb:.1}"),
            format!("{pb4:.1}"),
            format!("{pb16:.1}"),
            format!("{:.4}", kg / kp),
            format!("{:.4}", kdg / kp),
            format!("{:.4}", cg / cs),
            format!("{:.4}", cg / pf),
            format!("{:.4}", cg / pr),
            format!("{:.4}", cg / pb),
            format!("{:.4}", cg / pb4),
            format!("{:.4}", cg / pb16),
            format!("{:.4}", cdg / pb),
            format!("{:.4}", cdg / pb16),
            format!("{:.4}", cdg / cg),
            format!("{:.4}", cg / kg),
            format!("{:.4}", 1.0 - kg / cg),
            format!("{:.4}", 1.0 - kdg / cdg),
        ])?;
    }
    diagnostics.flush()?;
    metrics.write(result_dir, predicates.len())
}

#[derive(Default)]
struct Metrics {
    known_fused: Vec<f64>,
    known_parquet: Vec<f64>,
    known_direct_parquet: Vec<f64>,
    complete_fused: Vec<f64>,
    complete_scan: Vec<f64>,
    complete_full: Vec<f64>,
    complete_filter: Vec<f64>,
    complete_boundary: Vec<f64>,
    complete_boundary_p4096: Vec<f64>,
    complete_boundary_p16384: Vec<f64>,
    complete_direct_boundary: Vec<f64>,
    complete_direct_boundary_p16384: Vec<f64>,
    complete_direct_storage: Vec<f64>,
    discovery_share: Vec<f64>,
    direct_discovery_share: Vec<f64>,
}

impl Metrics {
    #[allow(clippy::too_many_arguments)]
    fn observe(
        &mut self,
        kg: f64,
        kdg: f64,
        kf: f64,
        kp: f64,
        cg: f64,
        cdg: f64,
        cf: f64,
        cs: f64,
        pf: f64,
        pr: f64,
        pb: f64,
        pb4: f64,
        pb16: f64,
    ) {
        self.known_fused.push(kg / kf);
        self.known_parquet.push(kg / kp);
        self.known_direct_parquet.push(kdg / kp);
        self.complete_fused.push(cg / cf);
        self.complete_scan.push(cg / cs);
        self.complete_full.push(cg / pf);
        self.complete_filter.push(cg / pr);
        self.complete_boundary.push(cg / pb);
        self.complete_boundary_p4096.push(cg / pb4);
        self.complete_boundary_p16384.push(cg / pb16);
        self.complete_direct_boundary.push(cdg / pb);
        self.complete_direct_boundary_p16384.push(cdg / pb16);
        self.complete_direct_storage.push(cdg / cg);
        self.discovery_share.push(1.0 - kg / cg);
        self.direct_discovery_share.push(1.0 - kdg / cdg);
    }

    fn write(&self, result_dir: &str, cells: usize) -> Result<(), Box<dyn Error>> {
        let mut output = csv::Writer::from_path(format!("{result_dir}/summary.csv"))?;
        output.write_record(["metric", "value"])?;
        row(&mut output, "predicate_cells", cells)?;
        wins(
            &mut output,
            "known_generated_beats_fused",
            &self.known_fused,
        )?;
        wins(
            &mut output,
            "known_generated_beats_parquet",
            &self.known_parquet,
        )?;
        wins(
            &mut output,
            "known_direct_generated_beats_parquet",
            &self.known_direct_parquet,
        )?;
        wins(
            &mut output,
            "complete_generated_beats_fused",
            &self.complete_fused,
        )?;
        wins(
            &mut output,
            "compiled_filter_beats_scan",
            &self.complete_scan,
        )?;
        wins(
            &mut output,
            "complete_generated_beats_parquet_full",
            &self.complete_full,
        )?;
        wins(
            &mut output,
            "complete_generated_beats_parquet_filter",
            &self.complete_filter,
        )?;
        wins(
            &mut output,
            "complete_generated_beats_parquet_boundary",
            &self.complete_boundary,
        )?;
        wins(
            &mut output,
            "complete_generated_beats_parquet_boundary_p4096",
            &self.complete_boundary_p4096,
        )?;
        wins(
            &mut output,
            "complete_generated_beats_parquet_boundary_p16384",
            &self.complete_boundary_p16384,
        )?;
        wins(
            &mut output,
            "complete_direct_generated_beats_parquet_boundary",
            &self.complete_direct_boundary,
        )?;
        wins(
            &mut output,
            "complete_direct_generated_beats_parquet_boundary_p16384",
            &self.complete_direct_boundary_p16384,
        )?;
        distribution(&mut output, "known_gen_over_parquet", &self.known_parquet)?;
        distribution(
            &mut output,
            "known_direct_gen_over_parquet",
            &self.known_direct_parquet,
        )?;
        distribution(&mut output, "complete_gen_over_scan", &self.complete_scan)?;
        distribution(
            &mut output,
            "complete_gen_over_parquet_full",
            &self.complete_full,
        )?;
        distribution(
            &mut output,
            "complete_gen_over_parquet_filter",
            &self.complete_filter,
        )?;
        distribution(
            &mut output,
            "complete_gen_over_parquet_boundary",
            &self.complete_boundary,
        )?;
        distribution(
            &mut output,
            "complete_gen_over_parquet_boundary_p4096",
            &self.complete_boundary_p4096,
        )?;
        distribution(
            &mut output,
            "complete_gen_over_parquet_boundary_p16384",
            &self.complete_boundary_p16384,
        )?;
        distribution(
            &mut output,
            "complete_direct_gen_over_parquet_boundary",
            &self.complete_direct_boundary,
        )?;
        distribution(
            &mut output,
            "complete_direct_gen_over_parquet_boundary_p16384",
            &self.complete_direct_boundary_p16384,
        )?;
        distribution(
            &mut output,
            "complete_direct_gen_over_storage_gen",
            &self.complete_direct_storage,
        )?;
        distribution(
            &mut output,
            "predicate_discovery_share",
            &self.discovery_share,
        )?;
        distribution(
            &mut output,
            "direct_predicate_discovery_share",
            &self.direct_discovery_share,
        )?;
        output.flush()?;
        Ok(())
    }
}

fn read_predicates(path: &str) -> Result<BTreeMap<Key, f64>, Box<dyn Error>> {
    let mut output = BTreeMap::new();
    for record in csv::Reader::from_path(path)?.records() {
        let record = record?;
        output.insert((record[0].parse()?, record[1].into()), record[6].parse()?);
    }
    Ok(output)
}

fn read_timings(path: &str) -> Result<HashMap<Key, Timings>, Box<dyn Error>> {
    let mut output: HashMap<Key, Timings> = HashMap::new();
    for record in csv::Reader::from_path(path)?.records() {
        let record = record?;
        output
            .entry((record[0].parse()?, record[1].into()))
            .or_default()
            .insert(record[2].into(), record[4].parse()?);
    }
    Ok(output)
}

fn value(results: &Timings, name: &str) -> Result<f64, Box<dyn Error>> {
    results
        .get(name)
        .copied()
        .ok_or_else(|| format!("missing {name}").into())
}

fn wins(output: &mut csv::Writer<std::fs::File>, name: &str, ratios: &[f64]) -> csv::Result<()> {
    row(
        output,
        name,
        ratios.iter().filter(|ratio| **ratio < 1.0).count(),
    )
}

fn distribution(
    output: &mut csv::Writer<std::fs::File>,
    name: &str,
    values: &[f64],
) -> csv::Result<()> {
    let mut values = values.to_vec();
    values.sort_by(f64::total_cmp);
    row(output, &format!("{name}_min"), format!("{:.4}", values[0]))?;
    row(
        output,
        &format!("{name}_median"),
        format!("{:.4}", median_sorted(&values)),
    )?;
    row(
        output,
        &format!("{name}_max"),
        format!("{:.4}", values[values.len() - 1]),
    )
}

fn row(
    output: &mut csv::Writer<std::fs::File>,
    name: &str,
    value: impl ToString,
) -> csv::Result<()> {
    output.write_record([name, &value.to_string()])
}

/// Standard median of an already-sorted sample: the mean of the two central
/// order statistics when the length is even. Taking a single central element
/// instead biases the estimate high, which matters here because every source
/// in the study contributes an even number of cells.
fn median_sorted(values: &[f64]) -> f64 {
    let middle = values.len() / 2;
    if values.len() % 2 == 1 {
        values[middle]
    } else {
        (values[middle - 1] + values[middle]) / 2.0
    }
}

#[cfg(test)]
mod tests {
    use super::median_sorted;

    #[test]
    fn median_averages_the_two_central_values_when_even() {
        assert_eq!(median_sorted(&[1.0, 2.0, 3.0, 4.0]), 2.5);
        assert_eq!(median_sorted(&[0.1, 0.3]), 0.2);
    }

    #[test]
    fn median_takes_the_centre_when_odd() {
        assert_eq!(median_sorted(&[1.0, 2.0, 3.0]), 2.0);
        assert_eq!(median_sorted(&[7.0]), 7.0);
    }

    #[test]
    fn median_handles_duplicates_and_ties() {
        assert_eq!(median_sorted(&[2.0, 2.0, 2.0, 2.0]), 2.0);
        assert_eq!(median_sorted(&[1.0, 1.0, 3.0, 3.0]), 2.0);
    }

    /// The specific defect this guards: for an even sample the upper central
    /// element is not the median, and on a lower-is-better ratio it biases the
    /// reported figure against the system under test.
    #[test]
    fn median_is_not_the_upper_central_order_statistic() {
        let sample = [0.20, 0.40, 0.60, 1.00];
        assert_eq!(median_sorted(&sample), 0.5);
        assert_ne!(median_sorted(&sample), sample[sample.len() / 2]);
    }
}
