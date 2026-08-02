//! Generate a neutral claim manifest from canonical result CSVs.
//!
//! Every reported number is defined here and traced to a CSV produced by a
//! study binary. Nothing is hand-entered. The
//! paired 16Ki-row run provides the scale-stability comparison; the live
//! directories provide the current study, certificate controls, and census.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::Write as _;
use std::fs;

const CURRENT: &str = "experiments/results/predicate_pipeline";
const PAIRED_16K: &str = "experiments/results/predicate_pipeline_rows16k";
const CENSUS: &str = "experiments/results/invariant_census";
const CERTIFICATES: &str = "experiments/results/certificate_study";
const REAL_ACCESS: &str = "experiments/results/real_access";
const ADDITIONAL: &str = "experiments/results/additional_plans.csv";
const OUTPUT: &str = "experiments/results/claim_manifest.csv";

#[derive(Default)]
struct ClaimManifest {
    entries: Vec<(String, String)>,
}

impl ClaimManifest {
    fn write(&self, path: &str) -> Result<(), Box<dyn Error>> {
        let mut writer = csv::Writer::from_path(path)?;
        writer.write_record(["claim", "value"])?;
        for (name, value) in &self.entries {
            writer.write_record([name, value])?;
        }
        writer.flush()?;
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    if std::env::args().any(|argument| argument == "--measure-additional") {
        let pairs = read_csv(&format!("{CURRENT}/pairs.csv"))?;
        let mut measured = ClaimManifest::default();
        evaluate_dictionary_translation(&mut measured, &pairs)?;
        evaluate_run_length_counting(&mut measured)?;
        measured.write(ADDITIONAL)?;
        println!("wrote {ADDITIONAL}");
    }

    let mut out = ClaimManifest::default();

    let summary = key_value_csv(&format!("{CURRENT}/summary.csv"))?;
    let summary_16k = key_value_csv(&format!("{PAIRED_16K}/summary.csv"))?;
    require_matching_fingerprints(CURRENT, PAIRED_16K)?;
    let census = key_value_csv(&format!("{CENSUS}/summary.csv"))?;
    let additional = key_value_csv(ADDITIONAL)?;

    // Study shape.
    let pairs = read_csv(&format!("{CURRENT}/pairs.csv"))?;
    let columns = read_csv(&format!("{CURRENT}/columns.csv"))?;
    let source_summary = read_csv(&format!("{CURRENT}/source_summary.csv"))?;
    let rows_per_column = pairs.field(&pairs[0], "rows")?;
    define(&mut out, "WitPairs", &count(&pairs).to_string());
    define(&mut out, "WitColumns", &count(&columns).to_string());
    define(&mut out, "WitSources", &count(&source_summary).to_string());
    define(
        &mut out,
        "WitRowsPerColumn",
        &group_digits(&rows_per_column),
    );
    define_metric(&mut out, "WitCells", &summary, "predicate_cells", 0)?;

    // Source-level intervals keep correlated predicates from one source from
    // masquerading as independent evidence.
    for (name, column) in [
        (
            "DirectBoundary",
            "complete_direct_gen_over_parquet_boundary_median",
        ),
        (
            "DirectStorage",
            "complete_direct_gen_over_storage_gen_median",
        ),
    ] {
        let values = source_summary
            .iter()
            .map(|row| {
                source_summary
                    .field(row, column)?
                    .parse::<f64>()
                    .map_err(Into::into)
            })
            .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
        let (low, high) = bootstrap_median_ci(&values, 2_000);
        define(
            &mut out,
            &format!("WitSource{name}Median"),
            &format!("{:.2}", median(&values)),
        );
        define(
            &mut out,
            &format!("WitSource{name}CiLow"),
            &format!("{low:.2}"),
        );
        define(
            &mut out,
            &format!("WitSource{name}CiHigh"),
            &format!("{high:.2}"),
        );
    }

    // Robustness of the source-level interval to the resampling unit. NAB
    // contributes several columns per archive family (realTweets, realTraffic,
    // realAWSCloudwatch), and columns inside one family measure the same
    // phenomenon, so treating each as independent overstates how many
    // independent draws the bootstrap has.
    //
    // Two corrections are reported. The two-stage cluster bootstrap below is
    // the principled one: it resamples families and then sources within each
    // drawn family, so within-family correlation is respected while cluster
    // sizes are preserved. Collapsing each family to a single median, computed
    // afterwards, is cruder and more pessimistic here, because it weights a
    // one-source family equally with a ten-source family and the singleton
    // families are the worst cases. Reporting both shows how much the headline
    // depends on that choice rather than asserting one is correct.
    let family_of = |source: &str| -> String {
        source
            .split('/')
            .next()
            .unwrap_or(source)
            .trim_matches('"')
            .to_string()
    };
    for (name, column) in [
        (
            "DirectBoundary",
            "complete_direct_gen_over_parquet_boundary_median",
        ),
        (
            "DirectStorage",
            "complete_direct_gen_over_storage_gen_median",
        ),
    ] {
        let mut families: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        for row in &source_summary {
            let value = source_summary.field(row, column)?.parse::<f64>()?;
            families
                .entry(family_of(&source_summary.field(row, "source")?))
                .or_default()
                .push(value);
        }
        let clusters: Vec<Vec<f64>> = families.values().cloned().collect();
        let pooled: Vec<f64> = clusters.iter().flatten().copied().collect();
        let (cluster_low, cluster_high) = cluster_bootstrap_median_ci(&clusters, 2_000);
        define(
            &mut out,
            &format!("WitCluster{name}Median"),
            &format!("{:.2}", median(&pooled)),
        );
        define(
            &mut out,
            &format!("WitCluster{name}CiLow"),
            &format!("{cluster_low:.2}"),
        );
        define(
            &mut out,
            &format!("WitCluster{name}CiHigh"),
            &format!("{cluster_high:.2}"),
        );

        let collapsed: Vec<f64> = families.values().map(|group| median(group)).collect();
        let (low, high) = bootstrap_median_ci(&collapsed, 2_000);
        if name == "DirectBoundary" {
            define(&mut out, "WitFamilies", &collapsed.len().to_string());
            // Distribution-free corroboration: how many families favour the
            // access-ready plan at all, ignoring by how much. Immune to the
            // near-parity cluster that makes cell-level win counts unstable.
            let wins = collapsed.iter().filter(|ratio| **ratio < 1.0).count();
            define(&mut out, "WitFamilyWins", &wins.to_string());
            define(
                &mut out,
                "WitFamilySignP",
                &format!("{:.3}", sign_test_p(wins, collapsed.len())),
            );
            // Reported in preference to the sign test: same distribution-free
            // question, but it uses the magnitudes the sign test discards, so
            // the two-sided value is stronger than the sign test's one-sided.
            define(
                &mut out,
                "WitFamilyWilcoxonP",
                &format!("{:.3}", wilcoxon_signed_rank_p(&collapsed)?),
            );
        }
        define(
            &mut out,
            &format!("WitFamily{name}Median"),
            &format!("{:.2}", median(&collapsed)),
        );
        define(
            &mut out,
            &format!("WitFamily{name}CiLow"),
            &format!("{low:.2}"),
        );
        define(
            &mut out,
            &format!("WitFamily{name}CiHigh"),
            &format!("{high:.2}"),
        );
    }

    // Census shape and incidence.
    define_metric(&mut out, "WitCensusColumns", &census, "columns", 0)?;
    define_metric(&mut out, "WitCensusSources", &census, "sources", 0)?;
    let census_rows = lookup(&census, "rows")?;
    define(
        &mut out,
        "WitCensusRowsMillions",
        &format!("{:.1}", census_rows.parse::<f64>()? / 1e6),
    );
    define_percent(
        &mut out,
        "WitCensusMonotoneColumns",
        &census,
        "global_monotone_column_fraction",
    )?;
    define_percent(
        &mut out,
        "WitCensusStructuralAccessReady",
        &census,
        "structural_monotone_access_ready_columns_fraction",
    )?;
    define_percent(
        &mut out,
        "WitCensusCheckedAccessReady",
        &census,
        "checked_monotone_access_ready_columns_fraction",
    )?;
    define_percent(
        &mut out,
        "WitCensusPiecewiseAccessReady",
        &census,
        "structural_piecewise_access_ready_columns_fraction",
    )?;
    define_percent(
        &mut out,
        "WitCensusMonotoneSourceWeighted",
        &census,
        "global_monotone_source_weighted_fraction",
    )?;
    define_percent(
        &mut out,
        "WitCensusMonotonePages",
        &census,
        "page_1024_monotone_fraction",
    )?;
    define_percent(
        &mut out,
        "WitCensusSmallDomain",
        &census,
        "small_domain_column_fraction",
    )?;
    define_percent(
        &mut out,
        "WitCensusOrderMapping",
        &census,
        "order_mapping_access_ready_columns_fraction",
    )?;
    // The census table prints this beside the structural premium, so it is
    // generated at the same precision rather than transcribed.
    define_metric(
        &mut out,
        "WitCensusOrderMappingPremium",
        &census,
        "order_mapping_access_ready_premium_median",
        3,
    )?;
    define_metric(
        &mut out,
        "WitCensusStructuralPremiumMedian",
        &census,
        "structural_monotone_premium_median",
        3,
    )?;
    define_percent(
        &mut out,
        "WitCensusSegmentFraction",
        &census,
        "monotone_segment_rows_ge_128_fraction",
    )?;
    // Every column-encoding pair the census builds has its derived order and
    // mapping facts re-checked against the decoded values; the count is the
    // sample size behind the soundness claim in Section 3.3.
    define_metric(
        &mut out,
        "WitCensusCandidates",
        &census,
        "candidate_rows",
        0,
    )?;

    // Effect decomposition (current study).
    define_metric(
        &mut out,
        "WitDiscoveryShare",
        &summary,
        "predicate_discovery_share_median",
        2,
    )?;

    // Discovery share by target selectivity: the single median above
    // compresses a wide, selectivity-dependent range into one number.
    let diagnostics = read_csv(&format!("{CURRENT}/diagnostics.csv"))?;
    let mut discovery_by_selectivity: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
    for row in &diagnostics {
        let selectivity = diagnostics
            .field(row, "actual_selectivity")?
            .parse::<f64>()?;
        let share = diagnostics.field(row, "discovery_share")?.parse::<f64>()?;
        let bucket = if selectivity <= 0.002 {
            "Tenth"
        } else if selectivity <= 0.02 {
            "OnePct"
        } else if selectivity <= 0.2 {
            "TenPct"
        } else {
            "FiftyPct"
        };
        discovery_by_selectivity
            .entry(bucket)
            .or_default()
            .push(share);
    }
    for bucket in ["Tenth", "OnePct", "TenPct", "FiftyPct"] {
        let values = discovery_by_selectivity
            .get(bucket)
            .ok_or_else(|| format!("no diagnostics cells at selectivity bucket {bucket}"))?;
        define(
            &mut out,
            &format!("WitDiscoveryShare{bucket}"),
            &format!("{:.2}", median(values)),
        );
    }
    define_metric(
        &mut out,
        "WitKnownOverParquet",
        &summary,
        "known_gen_over_parquet_median",
        2,
    )?;
    define_metric(
        &mut out,
        "WitKnownDirectOverParquet",
        &summary,
        "known_direct_gen_over_parquet_median",
        2,
    )?;
    define_metric(
        &mut out,
        "WitFilterBeatsScan",
        &summary,
        "compiled_filter_beats_scan",
        0,
    )?;
    define_metric(
        &mut out,
        "WitCompleteOverScan",
        &summary,
        "complete_gen_over_scan_median",
        2,
    )?;

    // Frontier (current study).
    define_metric(
        &mut out,
        "WitDirectOverStorage",
        &summary,
        "complete_direct_gen_over_storage_gen_median",
        2,
    )?;
    define_metric(
        &mut out,
        "WitDirectOverBoundary",
        &summary,
        "complete_direct_gen_over_parquet_boundary_median",
        2,
    )?;
    define_metric(
        &mut out,
        "WitDirectOverBoundaryLargePage",
        &summary,
        "complete_direct_gen_over_parquet_boundary_p16384_median",
        2,
    )?;
    define_metric(
        &mut out,
        "WitDirectBeatsBoundary",
        &summary,
        "complete_direct_generated_beats_parquet_boundary",
        0,
    )?;
    define_metric(
        &mut out,
        "WitStorageBeatsBoundary",
        &summary,
        "complete_generated_beats_parquet_boundary",
        0,
    )?;
    define_metric(
        &mut out,
        "WitStorageOverBoundary",
        &summary,
        "complete_gen_over_parquet_boundary_median",
        2,
    )?;

    // Access-byte premium of the access-ready layout, from physical files.
    let premium = median(
        &columns
            .iter()
            .map(|row| {
                Ok(columns.field(row, "direct_bytes")?.parse::<f64>()?
                    / columns.field(row, "bytes")?.parse::<f64>()?)
            })
            .collect::<Result<Vec<_>, Box<dyn Error>>>()?,
    );
    define(&mut out, "WitAccessPremium", &format!("{premium:.2}"));

    // Scale stability: paired 16Ki run against the current 128Ki run.
    for (name, key, digits) in [
        (
            "DirectOverBoundary",
            "complete_direct_gen_over_parquet_boundary_median",
            2,
        ),
        (
            "DirectOverStorage",
            "complete_direct_gen_over_storage_gen_median",
            2,
        ),
        ("DiscoveryShare", "predicate_discovery_share_median", 2),
        (
            "DirectBeatsBoundary",
            "complete_direct_generated_beats_parquet_boundary",
            0,
        ),
    ] {
        define_metric(
            &mut out,
            &format!("WitSixteenK{name}"),
            &summary_16k,
            key,
            digits,
        )?;
    }

    // Probabilistic and auxiliary-index controls, summarized over independent
    // source medians rather than correlated query cells.
    let certificate_rows = read_csv(&format!("{CERTIFICATES}/summary.csv"))?;
    for (suffix, plan, query) in [
        ("BloomAbsent", "bloom", "eq_absent"),
        ("BloomRare", "bloom", "eq_rare"),
        ("BloomFrequent", "bloom", "eq_frequent"),
        ("BloomIn", "bloom", "in_mixed"),
        ("FenceRare", "sparse_fence", "eq_rare"),
    ] {
        let row = find_row(&certificate_rows, |row| row[0] == plan && row[1] == query)?;
        define(&mut out, &format!("Wit{suffix}Cells"), &row[2]);
        define(&mut out, &format!("Wit{suffix}Sources"), &row[3]);
        define(
            &mut out,
            &format!("Wit{suffix}Candidate"),
            &format!("{:.3}", row[5].parse::<f64>()?),
        );
        define(
            &mut out,
            &format!("Wit{suffix}ModeledBytes"),
            &format!("{:.3}", row[8].parse::<f64>()?),
        );
        define(
            &mut out,
            &format!("Wit{suffix}Latency"),
            &format!("{:.3}", row[11].parse::<f64>()?),
        );
        define(
            &mut out,
            &format!("Wit{suffix}CiLow"),
            &format!("{:.3}", row[13].parse::<f64>()?),
        );
        define(
            &mut out,
            &format!("Wit{suffix}CiHigh"),
            &format!("{:.3}", row[14].parse::<f64>()?),
        );
    }

    // Cold XFS random-order schedule: same logical answer and closure, different
    // read schedule. These are mechanism diagnostics, not device-general claims.
    let storage = read_csv(&format!("{REAL_ACCESS}/storage_scan.csv"))?;
    let storage_row = |policy: &str| {
        find_row(&storage, |row| {
            row[0] == "workspace_mount"
                && row[1] == "xfs"
                && row[2] == "random"
                && row[3] == "cold"
                && row[4] == policy
        })
    };
    let per_page = storage_row("per_page")?;
    let sorted = storage_row("sorted_closure")?;
    let coalesced = storage_row("coalesce_4k")?;
    let full = storage_row("full_file")?;
    let mb = |value: &str| -> Result<f64, Box<dyn Error>> {
        Ok(value.parse::<f64>()? / (1024.0 * 1024.0))
    };
    define(
        &mut out,
        "WitColdRequiredMb",
        &format!("{:.2}", mb(&per_page[9])?),
    );
    define(
        &mut out,
        "WitColdFileMb",
        &format!("{:.2}", mb(&per_page[8])?),
    );
    define(&mut out, "WitColdPageCalls", &per_page[11]);
    define(&mut out, "WitColdCoalescedCalls", &coalesced[11]);
    define(&mut out, "WitColdFullCalls", &full[11]);
    define(
        &mut out,
        "WitColdCoalescedMb",
        &format!("{:.2}", mb(&coalesced[10])?),
    );
    let full_ns = full[14].parse::<f64>()?;
    for (name, row) in [
        ("Page", per_page),
        ("Sorted", sorted),
        ("Coalesced", coalesced),
    ] {
        define(
            &mut out,
            &format!("WitCold{name}OverFull"),
            &format!("{:.2}", row[14].parse::<f64>()? / full_ns),
        );
    }

    // Certificate classes (current study).
    let classes = read_csv(&format!("{CURRENT}/certificate_summary.csv"))?;
    for row in &classes {
        let class = if classes.field(row, "non_decreasing")? == "true" {
            "Monotone"
        } else {
            "NonMonotone"
        };
        define(
            &mut out,
            &format!("Wit{class}Sources"),
            &classes.field(row, "sources")?,
        );
        define(
            &mut out,
            &format!("Wit{class}Cells"),
            &classes.field(row, "predicate_cells")?,
        );
        define(
            &mut out,
            &format!("Wit{class}DirectOverBoundary"),
            &format!(
                "{:.2}",
                classes
                    .field(row, "source_median_direct_gen_over_parquet_boundary")?
                    .parse::<f64>()?
            ),
        );
        define(
            &mut out,
            &format!("Wit{class}DirectOverEqualPage"),
            &format!(
                "{:.2}",
                classes
                    .field(
                        row,
                        "source_median_direct_gen_over_equal_page_parquet_boundary"
                    )?
                    .parse::<f64>()?
            ),
        );
        define(
            &mut out,
            &format!("Wit{class}DirectWins"),
            &classes.field(row, "direct_sources_beating_parquet_boundary")?,
        );
    }

    // Frontier scatter: byte premium (x) against latency ratio (y) per source.
    //
    // Split by archive family as well as pooled. Three NAB families supply 23 of
    // the sources, so an undifferentiated scatter shows a tight cluster as one
    // mark and hides which points are independent columns. Separating them lets
    // the figure carry the same caveat the interval discussion states: the
    // single-column sources are both the least correlated and, at the parity
    // end, the least favourable.
    let mut frontier = String::new();
    let mut by_family: BTreeMap<String, Vec<(f64, f64)>> = BTreeMap::new();
    for row in &source_summary {
        let premium = source_summary
            .field(row, "direct_gen_over_storage_gen_file_bytes_median")?
            .parse::<f64>()?;
        let ratio = source_summary
            .field(row, "complete_direct_gen_over_storage_gen_median")?
            .parse::<f64>()?;
        write!(frontier, "({premium:.3},{ratio:.3})")?;
        by_family
            .entry(family_of(&source_summary.field(row, "source")?))
            .or_default()
            .push((premium, ratio));
    }
    define(&mut out, "WitFrontierPlot", &frontier);

    let series = |points: &[(f64, f64)]| {
        let mut text = String::new();
        for (premium, ratio) in points {
            let _ = write!(text, "({premium:.3},{ratio:.3})");
        }
        text
    };
    let mut singletons: Vec<(f64, f64)> = Vec::new();
    for (family, points) in &by_family {
        match family.as_str() {
            "realTweets" => define(&mut out, "WitFrontierTweetsPlot", &series(points)),
            "realTraffic" => define(&mut out, "WitFrontierTrafficPlot", &series(points)),
            "realAWSCloudwatch" => define(&mut out, "WitFrontierCloudwatchPlot", &series(points)),
            _ => singletons.extend(points.iter().copied()),
        }
    }
    singletons.sort_by(|left, right| left.partial_cmp(right).unwrap());
    define(&mut out, "WitFrontierSingletonPlot", &series(&singletons));
    define(
        &mut out,
        "WitFrontierSingletons",
        &singletons.len().to_string(),
    );

    // Per-cell effect data: ratio of the access-ready plan over the
    // sortedness-aware Parquet reference, keyed by certificate class and by
    // the cell's first target selectivity.
    let monotone_pair: BTreeMap<String, bool> = pairs
        .iter()
        .map(|row| {
            (
                row[0].clone(),
                row.get(8).map(String::as_str) == Some("true"),
            )
        })
        .collect();
    let mut cell_medians: BTreeMap<(String, String), BTreeMap<String, f64>> = BTreeMap::new();
    let complete_query = read_csv(&format!("{CURRENT}/complete_query.csv"))?;
    for row in &complete_query {
        let key = (row[0].clone(), row[1].clone());
        cell_medians.entry(key).or_default().insert(
            row[2].clone(),
            complete_query.field(row, "median_ns")?.parse()?,
        );
    }
    let mut curves: BTreeMap<(bool, u32), Vec<f64>> = BTreeMap::new();
    let mut ecdf: BTreeMap<bool, Vec<f64>> = BTreeMap::new();
    for ((pair, predicate), arms) in &cell_medians {
        let monotone = monotone_pair[pair];
        let target = predicate
            .split("pct")
            .next()
            .and_then(|prefix| prefix.parse::<f64>().ok())
            .ok_or_else(|| format!("unparsable predicate label {predicate}"))?;
        let ratio = arms["generated_direct_selective"] / arms["parquet_boundary_search"];
        curves
            .entry((monotone, (target * 10.0).round() as u32))
            .or_default()
            .push(ratio);
        ecdf.entry(monotone).or_default().push(ratio);
    }
    for (class, monotone) in [("Monotone", true), ("NoFact", false)] {
        let mut coordinates = String::new();
        for ((_, target), ratios) in curves.iter().filter(|((m, _), _)| *m == monotone) {
            write!(
                coordinates,
                "({:.1},{:.4})",
                f64::from(*target) / 10.0,
                median(ratios)
            )?;
        }
        define(&mut out, &format!("WitCurve{class}Plot"), &coordinates);
        let mut ratios = ecdf[&monotone].clone();
        ratios.sort_by(f64::total_cmp);
        let mut coordinates = String::new();
        for (position, ratio) in ratios.iter().enumerate() {
            write!(
                coordinates,
                "({ratio:.4},{:.4})",
                (position + 1) as f64 / ratios.len() as f64
            )?;
        }
        define(&mut out, &format!("WitEcdf{class}Plot"), &coordinates);
    }

    // Census incidence by corpus group.
    let census_sources = read_csv(&format!("{CENSUS}/sources.csv"))?;
    let mut group_rows: BTreeMap<String, (f64, f64, f64, f64, f64)> = BTreeMap::new();
    for row in &census_sources {
        let entry = group_rows.entry(row[0].clone()).or_default();
        entry.0 += census_sources.field(row, "columns")?.parse::<f64>()?;
        entry.1 += census_sources.field(row, "rows")?.parse::<f64>()?;
        entry.2 += census_sources
            .field(row, "global_monotone_columns")?
            .parse::<f64>()?;
        entry.3 += census_sources
            .field(row, "page_1024_total")?
            .parse::<f64>()?;
        entry.4 += census_sources
            .field(row, "page_1024_monotone")?
            .parse::<f64>()?;
    }
    let mut table = String::new();
    for (group, (columns, rows, monotone, pages, monotone_pages)) in &group_rows {
        writeln!(
            table,
            "{} & {} & {:.2} & {:.1} & {:.1} \\\\",
            escape_display_label(group),
            columns,
            rows / 1e6,
            100.0 * monotone / columns,
            100.0 * monotone_pages / pages,
        )?;
    }
    define(&mut out, "WitCensusGroupRows", table.trim_end());

    // Physical size by corpus group: bits per value for both layouts.
    type SizeSamples = (Vec<f64>, Vec<f64>, Vec<f64>);
    let mut size_rows: BTreeMap<String, SizeSamples> = BTreeMap::new();
    for row in &columns {
        let rows_in_column = columns.field(row, "rows")?.parse::<f64>()?;
        let minimal = columns.field(row, "bytes")?.parse::<f64>()?;
        let ready = columns.field(row, "direct_bytes")?.parse::<f64>()?;
        let entry = size_rows.entry(row[1].clone()).or_default();
        entry.0.push(minimal * 8.0 / rows_in_column);
        entry.1.push(ready * 8.0 / rows_in_column);
        entry.2.push(ready / minimal);
    }
    let mut table = String::new();
    for (group, (minimal, ready, premium)) in &size_rows {
        writeln!(
            table,
            "{} & {} & {:.2} & {:.2} & {:.2}$\\times$ \\\\",
            escape_display_label(group),
            minimal.len(),
            median(minimal),
            median(ready),
            median(premium),
        )?;
    }
    define(&mut out, "WitSizeGroupRows", table.trim_end());

    // Range of the access-ready byte premium across corpus groups: the claim
    // that it is not a constant header charge, without printing every group.
    let group_premiums: Vec<f64> = size_rows
        .values()
        .map(|(_, _, premium)| median(premium))
        .collect();
    define(
        &mut out,
        "WitPremiumGroupLow",
        &format!(
            "{:.2}",
            group_premiums.iter().copied().fold(f64::MAX, f64::min)
        ),
    );
    define(
        &mut out,
        "WitPremiumGroupHigh",
        &format!(
            "{:.2}",
            group_premiums.iter().copied().fold(f64::MIN, f64::max)
        ),
    );
    define(
        &mut out,
        "WitPremiumGroups",
        &group_premiums.len().to_string(),
    );

    // Additional authorized plans (Sec. 3.5): real measured evidence for
    // dictionary range translation and run-length counting, over columns and
    // pairs already in the canonical study, not a synthetic illustration.
    for name in [
        "WitDictPairs",
        "WitDictCells",
        "WitDictEntries",
        "WitDictProbes",
        "WitDictReduction",
        "WitDictLatency",
        "WitRleColumns",
        "WitRleReductionMedian",
        "WitRleLatency",
    ] {
        define(&mut out, name, lookup(&additional, name)?);
    }

    // Artifact identity: crate version and the frozen exact source
    // fingerprint, which together pin the exact semantics behind every
    // reported number.
    define(&mut out, "WitCrateVersion", env!("CARGO_PKG_VERSION"));
    let freeze = read_csv(&format!("{CURRENT}/freeze.csv"))?;
    define(
        &mut out,
        "WitRuleFingerprint",
        &freeze.field(&freeze[0], "rule_fingerprint")?,
    );

    // The fixed per-page certificate cost (Table 2's "checked" byte premium):
    // a version-3 descriptor adds a 4-byte flag word, 4 bytes of alignment
    // padding, and an 8-byte checksum over the legacy (version-2, uncertified)
    // header, regardless of column size. This is a serialization constant, not
    // a measurement; `certificate_header_premium_matches_serialized_pages` in
    // tests/certificate_plans.rs binds it to real encoded bytes so it cannot
    // drift away from the format. It is stated here rather than imported
    // because the exact source fingerprint hashes the source text of
    // format.rs, and widening its visibility would silently refreeze every
    // generated kernel.
    define(&mut out, "WitCertificateHeaderBytes", "16");

    out.write(OUTPUT)?;
    println!("wrote {OUTPUT}");
    Ok(())
}

/// Real, measured evidence for `TranslateDictionaryRange`: the pairs whose
/// selector column the study's own recipe search chose to dictionary-encode
/// (`selector_recipe == "Frame(Dictionary(BitPack))"`), so their
/// already-recorded `complete_gen_over_scan` reflects the compiled,
/// generated-kernel discovery path (`FILTER_SESSION_FNS`), not an
/// illustration. Dictionary size is read back from the same deterministic
/// corpus construction the study used, so it cannot drift from what was
/// actually encoded.
fn evaluate_dictionary_translation(
    out: &mut ClaimManifest,
    pairs: &Table,
) -> Result<(), Box<dyn Error>> {
    use witness::access_compiler::DecoderNode;
    use witness::experiment::access_real::predicate_access_corpus;

    let dictionary_pair_ids: Vec<usize> = pairs
        .iter()
        .filter(|row| {
            pairs.field(row, "selector_recipe").ok().as_deref()
                == Some("Frame(Dictionary(BitPack))")
        })
        .map(|row| {
            pairs
                .field(row, "pair")
                .and_then(|value| Ok(value.parse::<usize>()?))
        })
        .collect::<Result<_, Box<dyn Error>>>()?;
    if dictionary_pair_ids.is_empty() {
        return Err("no dictionary-encoded selector pairs found in the current study".into());
    }

    let diagnostics = read_csv(&format!("{CURRENT}/diagnostics.csv"))?;
    let mut ratios = Vec::new();
    for row in &diagnostics {
        let pair_id: usize = diagnostics.field(row, "pair")?.parse()?;
        if dictionary_pair_ids.contains(&pair_id) {
            ratios.push(
                diagnostics
                    .field(row, "complete_gen_over_scan")?
                    .parse::<f64>()?,
            );
        }
    }

    let (columns, corpus_pairs) = predicate_access_corpus()?;
    let mut max_entries = 0_usize;
    let mut rows = 0_usize;
    for &pair_id in &dictionary_pair_ids {
        let selector = &columns[corpus_pairs[pair_id].selector];
        match selector
            .size_selected
            .decoder
            .node(selector.size_selected.decoder.root())?
        {
            DecoderNode::Dictionary { entries, .. } => max_entries = max_entries.max(*entries),
            other => return Err(format!("expected a dictionary root, found {other:?}").into()),
        }
        rows = selector.size_selected.truth.len();
    }
    let probes = 2 * (usize::BITS - max_entries.max(1).leading_zeros()) as usize;

    define(out, "WitDictPairs", &dictionary_pair_ids.len().to_string());
    define(out, "WitDictCells", &ratios.len().to_string());
    define(
        out,
        "WitDictEntries",
        &group_digits(&max_entries.to_string()),
    );
    define(out, "WitDictProbes", &probes.to_string());
    define(
        out,
        "WitDictReduction",
        &format!("{:.0}", rows as f64 / probes as f64),
    );
    define(out, "WitDictLatency", &format!("{:.2}", median(&ratios)));
    Ok(())
}

/// Real, measured evidence for `CountRuns`, over real columns whose natural
/// row order already has run structure. Selection is a threshold on measured
/// run/row reduction, not a hand-picked list: every exact-integer census
/// column with at least a 4x reduction enters, so the reported median is not
/// a best-case cherry pick.
fn evaluate_run_length_counting(out: &mut ClaimManifest) -> Result<(), Box<dyn Error>> {
    use witness::access_compiler::{
        ClosureMode, InputColumn, PlanOp, Predicate, Query, Recipe, compile, encode,
        execute_count_runs, execute_interpreted,
    };
    use witness::experiment::datasets::invariant_census_columns;

    const MIN_REDUCTION: f64 = 4.0;
    const REPS: u32 = 7;

    fn natural_runs(values: &[i64]) -> usize {
        let mut runs = 1_usize;
        for window in values.windows(2) {
            if window[0] != window[1] {
                runs += 1;
            }
        }
        runs
    }

    let mut candidates = Vec::new();
    for column in invariant_census_columns(131_072)? {
        if !column.exact_i64 || column.values.iter().any(Option::is_none) {
            continue;
        }
        let values: Vec<i64> = column.values.into_iter().map(|v| v.unwrap()).collect();
        if values.len() < 1_024 || values.iter().any(|value| *value < 0) {
            continue;
        }
        let runs = natural_runs(&values);
        let reduction = values.len() as f64 / runs as f64;
        if reduction >= MIN_REDUCTION {
            candidates.push((values, reduction));
        }
    }
    if candidates.is_empty() {
        return Err("no census columns cleared the run-length reduction threshold".into());
    }

    let mut reductions = Vec::new();
    let mut ratios = Vec::new();
    for (values, reduction) in &candidates {
        reductions.push(*reduction);
        let target = values[0];
        // Fast path: the run index authorizes CountRuns.
        let run_length = encode(
            &Recipe::Rle {
                index_interval: 64,
                values: Box::new(Recipe::BitPack),
            },
            InputColumn::dense(values.clone()),
        )?;
        // Fallback: the same values with no run structure, where the compiler
        // emits CountExact. This is the comparison the calculus actually
        // decides between. It is deliberately NOT a per-row `get` walk of the
        // run-length column: that path re-enters the sparse run index once per
        // row, so timing against it would flatter CountRuns with an
        // access-pattern artifact rather than the plan.
        let flat = encode(&Recipe::BitPack, InputColumn::dense(values.clone()))?;
        let query = Query::Count {
            predicate: Predicate::Equals { value: target },
        };
        let fast_plan = compile(&run_length, query.clone())?;
        if !matches!(
            fast_plan.nodes.last().map(|node| &node.op),
            Some(PlanOp::CountRuns { .. })
        ) {
            return Err("a qualifying column did not authorize CountRuns".into());
        }
        let fallback_plan = compile(&flat, query.clone())?;
        if !matches!(
            fallback_plan.nodes.last().map(|node| &node.op),
            Some(PlanOp::CountExact)
        ) {
            return Err("the non-run encoding did not fall back to CountExact".into());
        }
        let fast = execute_count_runs(&run_length, target, ClosureMode::FullPage)?;
        let fallback = execute_interpreted(&flat, &query, ClosureMode::FullPage)?;
        if fast.answer != fallback.answer {
            return Err("CountRuns disagreed with the decode-and-compare fallback".into());
        }
        let fast_ns = time_median(REPS, || {
            execute_count_runs(&run_length, target, ClosureMode::FullPage).unwrap()
        });
        let fallback_ns = time_median(REPS, || {
            execute_interpreted(&flat, &query, ClosureMode::FullPage).unwrap()
        });
        ratios.push(fast_ns / fallback_ns);
    }

    define(out, "WitRleColumns", &candidates.len().to_string());
    define(
        out,
        "WitRleReductionMedian",
        &format!("{:.0}", median(&reductions)),
    );
    define(out, "WitRleLatency", &format!("{:.3}", median(&ratios)));
    Ok(())
}

/// Median wall-clock nanoseconds per call, timing only the closure and
/// discarding results via `black_box` so the optimizer cannot remove the
/// work being measured.
fn time_median<T>(reps: u32, mut call: impl FnMut() -> T) -> f64 {
    let mut samples = Vec::with_capacity(reps as usize);
    for _ in 0..reps {
        let start = std::time::Instant::now();
        let result = call();
        let elapsed = start.elapsed().as_nanos() as f64;
        std::hint::black_box(result);
        samples.push(elapsed);
    }
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

fn require_matching_fingerprints(left: &str, right: &str) -> Result<(), Box<dyn Error>> {
    let left = read_csv(&format!("{left}/freeze.csv"))?;
    let right = read_csv(&format!("{right}/freeze.csv"))?;
    if left.field(&left[0], "rule_fingerprint")? != right.field(&right[0], "rule_fingerprint")? {
        return Err("scale runs use different rule fingerprints".into());
    }
    Ok(())
}

fn find_row(
    rows: &[Vec<String>],
    predicate: impl Fn(&[String]) -> bool,
) -> Result<&Vec<String>, Box<dyn Error>> {
    rows.iter()
        .find(|row| predicate(row))
        .ok_or_else(|| "required result row is absent".into())
}

fn define(out: &mut ClaimManifest, name: &str, value: &str) {
    out.entries.push((name.to_string(), value.to_string()));
}

fn escape_display_label(value: &str) -> String {
    value.replace('_', "\\_")
}

fn define_metric(
    out: &mut ClaimManifest,
    name: &str,
    table: &BTreeMap<String, String>,
    key: &str,
    digits: usize,
) -> Result<(), Box<dyn Error>> {
    let raw = lookup(table, key)?;
    let formatted = if digits == 0 {
        raw.parse::<f64>()?.round().to_string()
    } else {
        format!("{:.digits$}", raw.parse::<f64>()?)
    };
    define(out, name, &formatted);
    Ok(())
}

fn define_percent(
    out: &mut ClaimManifest,
    name: &str,
    table: &BTreeMap<String, String>,
    key: &str,
) -> Result<(), Box<dyn Error>> {
    let value = lookup(table, key)?.parse::<f64>()? * 100.0;
    define(out, name, &format!("{value:.1}"));
    Ok(())
}

fn lookup<'t>(table: &'t BTreeMap<String, String>, key: &str) -> Result<&'t String, String> {
    table
        .get(key)
        .ok_or_else(|| format!("metric {key} is absent"))
}

fn key_value_csv(path: &str) -> Result<BTreeMap<String, String>, Box<dyn Error>> {
    Ok(read_csv(path)?
        .into_iter()
        .map(|row| (row[0].clone(), row[1].clone()))
        .collect())
}

fn count(rows: &[Vec<String>]) -> usize {
    rows.len()
}

/// A parsed CSV together with its header row.
///
/// Every claim in the paper is read out of a file some other binary wrote, so
/// the header is kept rather than skipped: fields are resolved by name, and a
/// producer that inserts, drops, or reorders a column fails the run instead of
/// silently redefining whichever claims read past it.
struct Table {
    header: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Table {
    fn field(&self, row: &[String], name: &str) -> Result<String, Box<dyn Error>> {
        let index = self
            .header
            .iter()
            .position(|column| column == name)
            .ok_or_else(|| format!("column {name} is absent from the header"))?;
        row.get(index)
            .cloned()
            .ok_or_else(|| format!("row is short of column {name}").into())
    }
}

impl std::ops::Deref for Table {
    type Target = Vec<Vec<String>>;

    fn deref(&self) -> &Self::Target {
        &self.rows
    }
}

impl<'a> IntoIterator for &'a Table {
    type Item = &'a Vec<String>;
    type IntoIter = std::slice::Iter<'a, Vec<String>>;

    fn into_iter(self) -> Self::IntoIter {
        self.rows.iter()
    }
}

fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(|left, right| left.total_cmp(right));
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted[mid]
    } else {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    }
}

/// One xorshift64 step. Shared by both bootstraps so their draw sequences are
/// generated identically and reproducibly.
fn xorshift64(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// Two-stage cluster bootstrap over a nested sample: draw `clusters.len()`
/// families with replacement, then draw that family's own number of sources
/// with replacement from it, and take the median of the pooled draw. This
/// respects within-family correlation without discarding cluster sizes, which
/// is what collapsing each family to a single value would do.
fn cluster_bootstrap_median_ci(clusters: &[Vec<f64>], repetitions: usize) -> (f64, f64) {
    assert!(!clusters.is_empty() && clusters.iter().all(|group| !group.is_empty()));
    let mut state = 0x6a09_e667_f3bc_c909_u64;
    let mut estimates = Vec::with_capacity(repetitions);
    let mut sample = Vec::new();
    for _ in 0..repetitions {
        sample.clear();
        for _ in 0..clusters.len() {
            let cluster = &clusters[xorshift64(&mut state) as usize % clusters.len()];
            for _ in 0..cluster.len() {
                sample.push(cluster[xorshift64(&mut state) as usize % cluster.len()]);
            }
        }
        estimates.push(median(&sample));
    }
    estimates.sort_by(f64::total_cmp);
    let low = estimates[(repetitions * 25 / 1_000).min(repetitions - 1)];
    let high = estimates[(repetitions * 975 / 1_000).min(repetitions - 1)];
    (low, high)
}

/// Exact two-sided Wilcoxon signed-rank test over log ratios.
///
/// The sign test below answers the same distribution-free question but
/// discards magnitude, scoring a family at 0.04 exactly like one at 0.96.
/// Signed rank keeps the ordering of |log ratio|, which is the information the
/// sign test throws away, so it is the better-matched test for ratios that are
/// already summarized by medians everywhere else in this study. Logs make the
/// symmetry the test assumes plausible: a halving and a doubling become equal
/// and opposite.
///
/// The null distribution is enumerated exactly by rank sum rather than
/// approximated. Ties in |log ratio| and exact parity would both invalidate
/// that enumeration, so they are rejected rather than silently averaged.
fn wilcoxon_signed_rank_p(ratios: &[f64]) -> Result<f64, Box<dyn Error>> {
    let mut deviations = Vec::with_capacity(ratios.len());
    for &ratio in ratios {
        if ratio <= 0.0 {
            return Err("signed-rank test needs positive ratios".into());
        }
        let deviation = ratio.ln();
        if deviation == 0.0 {
            return Err("signed-rank test needs no family exactly at parity".into());
        }
        deviations.push(deviation);
    }
    deviations.sort_by(|left, right| left.abs().total_cmp(&right.abs()));
    if deviations
        .windows(2)
        .any(|pair| pair[0].abs() == pair[1].abs())
    {
        return Err("signed-rank test needs distinct |log ratio| values".into());
    }

    let count = deviations.len();
    let total = count * (count + 1) / 2;
    let positive_rank_sum: usize = deviations
        .iter()
        .enumerate()
        .filter(|(_, deviation)| **deviation > 0.0)
        .map(|(index, _)| index + 1)
        .sum();

    // Sign assignments reaching each rank sum: each rank is either added or
    // not, so this is a 0/1 subset-sum count over the ranks 1..=count.
    let mut assignments = vec![0.0_f64; total + 1];
    assignments[0] = 1.0;
    for rank in 1..=count {
        for sum in (rank..=total).rev() {
            assignments[sum] += assignments[sum - rank];
        }
    }
    let at_or_below: f64 = assignments[..=positive_rank_sum].iter().sum();
    let at_or_above: f64 = assignments[positive_rank_sum..].iter().sum();
    Ok((2.0 * at_or_below.min(at_or_above) / 2.0_f64.powi(count as i32)).min(1.0))
}

/// One-sided exact binomial tail P(X >= wins) under a fair coin: the
/// probability that at least this many families would favour the plan by
/// chance alone.
fn sign_test_p(wins: usize, trials: usize) -> f64 {
    let mut tail = 0.0_f64;
    for k in wins..=trials {
        let mut term = 1.0_f64;
        for i in 0..k {
            term *= (trials - i) as f64 / (i + 1) as f64;
        }
        tail += term;
    }
    tail / 2.0_f64.powi(trials as i32)
}

fn bootstrap_median_ci(values: &[f64], repetitions: usize) -> (f64, f64) {
    assert!(!values.is_empty());
    let mut state = 0x6a09_e667_f3bc_c909_u64;
    let mut estimates = Vec::with_capacity(repetitions);
    let mut sample = Vec::with_capacity(values.len());
    for _ in 0..repetitions {
        sample.clear();
        for _ in 0..values.len() {
            sample.push(values[xorshift64(&mut state) as usize % values.len()]);
        }
        estimates.push(median(&sample));
    }
    estimates.sort_by(f64::total_cmp);
    let low = estimates[(repetitions * 25 / 1_000).min(repetitions - 1)];
    let high = estimates[(repetitions * 975 / 1_000).min(repetitions - 1)];
    (low, high)
}

fn group_digits(value: &str) -> String {
    let digits: Vec<char> = value.chars().collect();
    let mut grouped = String::new();
    for (position, digit) in digits.iter().enumerate() {
        if position > 0 && (digits.len() - position).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(*digit);
    }
    grouped
}

/// Minimal CSV reader: quoted fields may contain commas. The header is
/// retained so `Table::field` can resolve columns by name.
fn read_csv(path: &str) -> Result<Table, Box<dyn Error>> {
    let content = fs::read_to_string(path).map_err(|error| format!("{path}: {error}"))?;
    let split = |line: &str| {
        let mut fields = Vec::new();
        let mut current = String::new();
        let mut quoted = false;
        for character in line.chars() {
            match character {
                '"' => quoted = !quoted,
                ',' if !quoted => fields.push(std::mem::take(&mut current)),
                other => current.push(other),
            }
        }
        fields.push(current);
        fields
    };
    let mut lines = content.lines();
    let header = lines
        .next()
        .map(split)
        .ok_or_else(|| format!("{path}: file has no header row"))?;
    let rows = lines.filter(|line| !line.is_empty()).map(split).collect();
    Ok(Table { header, rows })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(header: &[&str], rows: &[&[&str]]) -> Table {
        Table {
            header: header.iter().map(|name| (*name).to_string()).collect(),
            rows: rows
                .iter()
                .map(|row| row.iter().map(|cell| (*cell).to_string()).collect())
                .collect(),
        }
    }

    /// The point of resolving by name: a producer may reorder its columns
    /// without silently redefining the claims that read them.
    #[test]
    fn field_follows_the_name_not_the_position() {
        let original = table(&["source", "ratio"], &[&["nab", "0.25"]]);
        let reordered = table(&["ratio", "source"], &[&["0.25", "nab"]]);
        for shape in [&original, &reordered] {
            assert_eq!(shape.field(&shape.rows[0], "source").unwrap(), "nab");
            assert_eq!(shape.field(&shape.rows[0], "ratio").unwrap(), "0.25");
        }
    }

    #[test]
    fn field_rejects_a_column_the_producer_no_longer_writes() {
        let shape = table(&["source"], &[&["nab"]]);
        let error = shape
            .field(&shape.rows[0], "ratio")
            .unwrap_err()
            .to_string();
        assert!(error.contains("ratio"), "unhelpful error: {error}");
    }

    #[test]
    fn read_csv_keeps_quoted_commas_inside_one_field() {
        let path = std::env::temp_dir().join("witness_claim_manifest_quoted.csv");
        fs::write(&path, "claim,value\nWitRows,\"131,072\"\n").unwrap();
        let parsed = read_csv(path.to_str().unwrap()).unwrap();
        assert_eq!(parsed.rows.len(), 1);
        assert_eq!(parsed.field(&parsed.rows[0], "value").unwrap(), "131,072");
        fs::remove_file(&path).unwrap();
    }

    /// An even-length median is the mean of the two central values, not the
    /// upper one. Reporting the upper order statistic biases every ratio up.
    #[test]
    fn median_averages_the_two_central_values() {
        assert_eq!(median(&[1.0, 2.0, 3.0, 4.0]), 2.5);
        assert_eq!(median(&[1.0, 2.0, 3.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), 2.5);
    }

    /// The exact one-sided binomial tail behind the sign test the paper
    /// reports. 9 of 12 families is 299/4096; the two-sided value is twice
    /// this, which is why the paper names the tail it uses.
    #[test]
    fn sign_test_is_the_exact_one_sided_binomial_tail() {
        assert!((sign_test_p(9, 12) - 299.0 / 4096.0).abs() < 1e-12);
        assert!((sign_test_p(12, 12) - 1.0 / 4096.0).abs() < 1e-12);
        assert!((sign_test_p(0, 12) - 1.0).abs() < 1e-12);
        // A bare majority is not evidence.
        assert!(sign_test_p(7, 12) > 0.15);
    }

    /// The twelve family medians the paper reports. W+ is 14 of a possible 78,
    /// whose exact two-sided tail is 0.0522 -- stronger than the sign test's
    /// one-sided 0.073 on the same data, because magnitude is retained.
    #[test]
    fn signed_rank_is_the_exact_two_sided_tail() {
        let families = [
            0.042, 0.074, 0.097, 0.162, 0.414, 0.486, 0.488, 0.523, 0.964, 1.504, 1.962, 3.211,
        ];
        let p = wilcoxon_signed_rank_p(&families).unwrap();
        assert!((p - 0.0522).abs() < 5e-4, "expected 0.0522, got {p}");
        assert!(
            p < sign_test_p(9, 12),
            "signed rank should beat the sign test"
        );
    }

    /// A sample with no directional tendency is the null case. The magnitudes
    /// are staggered because equal and opposite log ratios would tie.
    #[test]
    fn signed_rank_finds_nothing_in_a_balanced_sample() {
        let balanced = [0.5, 2.5, 0.2, 3.0];
        assert!((wilcoxon_signed_rank_p(&balanced).unwrap() - 1.0).abs() < 1e-12);
    }

    /// Every observation on one side is the most extreme outcome: 2/2^n.
    #[test]
    fn signed_rank_saturates_when_every_family_agrees() {
        let unanimous = [0.1, 0.2, 0.3, 0.4, 0.5];
        let p = wilcoxon_signed_rank_p(&unanimous).unwrap();
        assert!((p - 2.0 / 32.0).abs() < 1e-12, "got {p}");
    }

    /// Ties and exact parity break the enumerated null, so they must fail
    /// loudly rather than be averaged into an approximate rank.
    #[test]
    fn signed_rank_rejects_samples_its_null_cannot_describe() {
        assert!(wilcoxon_signed_rank_p(&[0.5, 2.0, 1.0]).is_err(), "parity");
        assert!(wilcoxon_signed_rank_p(&[0.5, 0.5, 0.3]).is_err(), "tie");
        assert!(wilcoxon_signed_rank_p(&[0.5, -1.0]).is_err(), "nonpositive");
    }

    #[test]
    fn cluster_bootstrap_brackets_the_median_and_is_deterministic() {
        let clusters = vec![
            vec![0.10, 0.12, 0.11],
            vec![0.90],
            vec![0.50, 0.55],
            vec![0.30, 0.31, 0.29, 0.32],
        ];
        let (low, high) = cluster_bootstrap_median_ci(&clusters, 2_000);
        assert!(low <= high, "inverted interval [{low}, {high}]");
        assert!(low >= 0.10 && high <= 0.90, "interval escapes the data");
        assert_eq!(
            (low, high),
            cluster_bootstrap_median_ci(&clusters, 2_000),
            "seeded bootstrap must be reproducible"
        );
    }

    /// With no spread within or between clusters there is nothing to resample,
    /// so the interval must collapse rather than wander.
    #[test]
    fn cluster_bootstrap_collapses_on_a_degenerate_sample() {
        let clusters = vec![vec![0.42, 0.42], vec![0.42], vec![0.42, 0.42, 0.42]];
        assert_eq!(cluster_bootstrap_median_ci(&clusters, 500), (0.42, 0.42));
    }
}
