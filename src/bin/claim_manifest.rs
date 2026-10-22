//! Generate a neutral claim manifest from canonical result CSVs.
//!
//! Every reported number is defined here and traced to a CSV produced by a
//! study binary. Nothing is hand-entered. The
//! paired 16Ki-row run provides the scale-stability comparison; the live
//! directories provide the current study, certificate controls, and census.

use std::collections::{BTreeMap, BTreeSet};
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
    // Constant columns and pages are trivially non-decreasing, so order that
    // can matter to a query is reported over non-constant data.
    for (name, key) in [
        ("WitCensusConstantColumns", "constant_column_fraction"),
        (
            "WitCensusNonconstantMonotone",
            "nonconstant_monotone_column_fraction",
        ),
        (
            "WitCensusNonconstantMonotoneSourceWeighted",
            "nonconstant_monotone_source_weighted_fraction",
        ),
        ("WitCensusConstantPages", "page_1024_constant_fraction"),
        (
            "WitCensusNonconstantMonotonePages",
            "page_1024_nonconstant_monotone_fraction",
        ),
    ] {
        define_percent(&mut out, name, &census, key)?;
    }
    define_metric(
        &mut out,
        "WitCensusChunkedPremium",
        &census,
        "chunked_premium_median",
        2,
    )?;
    define_metric(
        &mut out,
        "WitCensusAccessReadyPremium",
        &census,
        "access_ready_premium_median",
        2,
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

    paged_claims(&mut out)?;
    separation_claims(&mut out)?;
    conjunctive_claims(&mut out)?;
    parquet_facts_claims(&mut out)?;
    uncertainty_claims(&mut out)?;
    type_claims(&mut out)?;
    cold_claims(&mut out)?;
    ablation_claims(&mut out)?;
    fault_claims(&mut out)?;
    delete_claims(&mut out)?;
    page_fact_claims(&mut out)?;
    distribution_claims(&mut out)?;
    breakdown_claims(&mut out)?;

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

const PAGED: &str = "experiments/results/paged_study";
const PAGED_SCALE: &str = "experiments/results/paged_scale";

/// Claims from the paged studies: selective SUM over columns stored as
/// independently encoded pages, planned per page from encoding-derived facts,
/// against Parquet pruned by its page index; and the same on full-length
/// TPC-H SF10 lineitem across thread counts.
fn paged_claims(out: &mut ClaimManifest) -> Result<(), Box<dyn Error>> {
    let pairs = read_csv(&format!("{PAGED}/pairs.csv"))?;
    let queries = read_csv(&format!("{PAGED}/queries.csv"))?;
    let costs = read_csv(&format!("{PAGED}/costs.csv"))?;
    let freeze = read_csv(&format!("{PAGED}/freeze.csv"))?;

    let tables = pairs
        .iter()
        .map(|row| Ok((pairs.field(row, "group")?, pairs.field(row, "source")?)))
        .collect::<Result<BTreeSet<_>, Box<dyn Error>>>()?;
    define(out, "WitPagedPairs", &pairs.len().to_string());
    define(out, "WitPagedTables", &tables.len().to_string());
    define(
        out,
        "WitPagedPageRows",
        &group_digits(&freeze.field(&freeze[0], "page_rows")?),
    );
    let pages = pairs
        .iter()
        .map(|row| Ok(pairs.field(row, "pages")?.parse::<f64>()?))
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    define(
        out,
        "WitPagedPagesMedian",
        &format!("{:.0}", median(&pages)),
    );

    // Latency per (pair, predicate, arm), and the page decisions of the
    // Witness arm with facts.
    let mut latency = BTreeMap::<(String, String), BTreeMap<String, f64>>::new();
    // pages, skipped, accepted, exact skips, exact accepts, dictionary skips
    let mut decisions = [0_usize; 6];
    // Skips and accepts of derived bounds alone, without the shortcuts.
    let mut bound_decisions = [0_usize; 2];
    // Queries, skips and dictionary skips of the zone map with a dictionary
    // filter.
    let mut filter_decisions = [0_usize; 3];
    for row in &queries {
        let cell = (
            queries.field(row, "pair")?,
            queries.field(row, "predicate")?,
        );
        let arm = queries.field(row, "arm")?;
        latency
            .entry(cell)
            .or_default()
            .insert(arm.clone(), queries.field(row, "median_ns")?.parse()?);
        if arm == "witness_direct_facts" {
            for (slot, name) in [
                "pages",
                "skipped",
                "accepted",
                "oracle_skipped",
                "oracle_accepted",
                "code_skipped",
            ]
            .into_iter()
            .enumerate()
            {
                decisions[slot] += queries.field(row, name)?.parse::<usize>()?;
            }
        }
        if arm == "witness_direct_factbounds" {
            for (slot, name) in ["skipped", "accepted"].into_iter().enumerate() {
                bound_decisions[slot] += queries.field(row, name)?.parse::<usize>()?;
            }
        }
        if arm == "witness_direct_zonemapdict" {
            filter_decisions[0] += 1;
            for (slot, name) in ["skipped", "code_skipped"].into_iter().enumerate() {
                filter_decisions[slot + 1] += queries.field(row, name)?.parse::<usize>()?;
            }
        }
    }
    define(
        out,
        "WitPagedCells",
        &group_digits(&latency.len().to_string()),
    );
    add_selectivity(&mut latency, PAGED)?;
    let ratios = |numerator: &str, denominator: &str, bucket: Option<&str>| {
        latency
            .iter()
            .filter(|(_, arms)| bucket.is_none_or(|bucket| in_bucket(arms, bucket)))
            .filter_map(|(_, arms)| Some(arms.get(numerator)? / arms.get(denominator)?))
            .collect::<Vec<_>>()
    };
    for (label, denominator) in [
        ("Plain", "parquet_plain_indexed"),
        ("Zstd", "parquet_zstd_indexed"),
    ] {
        let all = ratios("witness_direct_facts", denominator, None);
        define(
            out,
            &format!("WitPaged{label}Median"),
            &format!("{:.2}", median(&all)),
        );
        define(
            out,
            &format!("WitPaged{label}Wins"),
            &group_digits(&all.iter().filter(|ratio| **ratio < 1.0).count().to_string()),
        );
        for (bucket, suffix) in [
            ("0.1pct", "Tenth"),
            ("1pct", "OnePct"),
            ("10pct", "TenPct"),
            ("50pct", "FiftyPct"),
        ] {
            let selected = ratios("witness_direct_facts", denominator, Some(bucket));
            define(
                out,
                &format!("WitPaged{label}{suffix}"),
                &format!("{:.2}", median(&selected)),
            );
        }
    }
    let compact = ratios("witness_compact_facts", "parquet_zstd_indexed", None);
    define(
        out,
        "WitPagedCompactZstdMedian",
        &format!("{:.2}", median(&compact)),
    );
    define(
        out,
        "WitPagedCompactZstdWins",
        &group_digits(
            &compact
                .iter()
                .filter(|ratio| **ratio < 1.0)
                .count()
                .to_string(),
        ),
    );
    let scan_only = ratios("witness_direct_scan", "parquet_plain_indexed", None);
    define(
        out,
        "WitPagedScanOverPlainMedian",
        &format!("{:.2}", median(&scan_only)),
    );
    let ablation = ratios("witness_direct_facts", "witness_direct_scan", None);
    let geometric =
        (ablation.iter().map(|ratio| ratio.ln()).sum::<f64>() / ablation.len() as f64).exp();
    define(out, "WitPagedFactsOverScanGeo", &format!("{geometric:.2}"));
    let [
        pages_total,
        skipped,
        _,
        exact_skipped,
        exact_accepted,
        code_skipped,
    ] = decisions;
    let [bound_skipped, bound_accepted] = bound_decisions;
    let percent = |part: usize, whole: usize| format!("{:.1}", part as f64 * 100.0 / whole as f64);
    define(out, "WitPagedSkippedPages", &percent(skipped, pages_total));
    // What derived bounds alone recover of an exact zone map's decisions.
    define(
        out,
        "WitPagedBoundSkipRecovery",
        &percent(bound_skipped.min(exact_skipped), exact_skipped),
    );
    define(
        out,
        "WitPagedBoundAcceptRecovery",
        &percent(bound_accepted.min(exact_accepted), exact_accepted),
    );
    // Facts also skip pages whose dictionary holds no value in the range,
    // and pages outside an ordered column's range: more than an exact zone
    // map can.
    define(
        out,
        "WitPagedFactsSkips",
        &group_digits(&skipped.to_string()),
    );
    define(
        out,
        "WitPagedZonemapSkips",
        &group_digits(&exact_skipped.to_string()),
    );
    define(
        out,
        "WitPagedCodeSkips",
        &group_digits(&code_skipped.to_string()),
    );
    define(
        out,
        "WitPagedCodeSkipShare",
        &percent(code_skipped, skipped),
    );
    // A zone map with a dictionary filter skips pages whose dictionary
    // holds no value in the range by testing every entry.
    let [filter_queries, filter_skipped, filter_code_skipped] = filter_decisions;
    if filter_queries == 0 {
        return Err("paged study lacks witness_direct_zonemapdict".into());
    }
    define(
        out,
        "WitPagedZonemapDictSkips",
        &group_digits(&filter_skipped.to_string()),
    );
    define(
        out,
        "WitPagedZonemapDictCodeSkips",
        &group_digits(&filter_code_skipped.to_string()),
    );
    if skipped < exact_skipped {
        return Err("facts skip fewer pages than an exact zone map".into());
    }
    define(
        out,
        "WitPagedFactsExtraSkipPercent",
        &format!(
            "{:.0}",
            (skipped - exact_skipped) as f64 * 100.0 / exact_skipped as f64
        ),
    );
    // The queries in which some page was skipped for its dictionary, and
    // how facts fare against the zone map on them.
    let mut code_cells = BTreeSet::new();
    for row in &queries {
        if queries.field(row, "arm")? == "witness_direct_facts"
            && queries.field(row, "code_skipped")?.parse::<usize>()? > 0
        {
            code_cells.insert((
                queries.field(row, "pair")?,
                queries.field(row, "predicate")?,
            ));
        }
    }
    let code_ratios = latency
        .iter()
        .filter(|(cell, _)| code_cells.contains(*cell))
        .filter_map(|(_, arms)| {
            Some(arms.get("witness_direct_facts")? / arms.get("witness_direct_zonemap")?)
        })
        .collect::<Vec<_>>();
    define(
        out,
        "WitPagedCodeSkipQueries",
        &group_digits(&code_ratios.len().to_string()),
    );
    define(
        out,
        "WitPagedCodeSkipQueriesGeo",
        &format!("{:.2}", geometric_mean(&code_ratios)),
    );

    let per_page = |name: &str| {
        costs
            .iter()
            .map(|row| {
                Ok(costs.field(row, name)?.parse::<f64>()?
                    / costs.field(row, "pages")?.parse::<f64>()?)
            })
            .collect::<Result<Vec<_>, Box<dyn Error>>>()
    };
    define(
        out,
        "WitPagedDeriveNsPerPage",
        &format!("{:.0}", median(&per_page("derive_ns")?)),
    );
    define(
        out,
        "WitPagedPlanNsPerPage",
        &format!("{:.0}", median(&per_page("plan_ns")?)),
    );
    define(
        out,
        "WitPagedPlanBytesPerPage",
        &format!("{:.1}", median(&per_page("plan_logical_bytes")?)),
    );
    // Verifying a page's dictionaries when it is sealed, and checking each
    // plan: planning with the check minus planning without it.
    define(
        out,
        "WitPagedVerifyNsPerPage",
        &format!("{:.0}", median(&per_page("verify_ns")?)),
    );
    let check = costs
        .iter()
        .map(|row| {
            let pages = costs.field(row, "pages")?.parse::<f64>()?;
            Ok((costs.field(row, "plan_ns")?.parse::<f64>()?
                - costs.field(row, "plan_unchecked_ns")?.parse::<f64>()?)
                / pages)
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    define(
        out,
        "WitPagedCheckNsPerPage",
        &format!("{:.0}", median(&check).max(0.0)),
    );
    let check_share = costs
        .iter()
        .map(|row| {
            Ok((costs.field(row, "plan_ns")?.parse::<f64>()?
                - costs.field(row, "plan_unchecked_ns")?.parse::<f64>()?)
                / costs.field(row, "scan_ns")?.parse::<f64>()?)
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    define(
        out,
        "WitPagedCheckOverScanPercent",
        &format!("{:.2}", (median(&check_share) * 100.0).max(0.0)),
    );
    let plan_share = costs
        .iter()
        .map(|row| {
            Ok(costs.field(row, "plan_ns")?.parse::<f64>()?
                / costs.field(row, "scan_ns")?.parse::<f64>()?)
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    define(
        out,
        "WitPagedPlanOverScanPercent",
        &format!("{:.2}", median(&plan_share) * 100.0),
    );
    let bytes = pairs
        .iter()
        .map(|row| {
            Ok(pairs.field(row, "witness_direct_bytes")?.parse::<f64>()?
                / pairs.field(row, "parquet_plain_bytes")?.parse::<f64>()?)
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    define(
        out,
        "WitPagedDirectOverPlainBytes",
        &format!("{:.2}", median(&bytes)),
    );

    let scale = read_csv(&format!("{PAGED_SCALE}/scaling.csv"))?;
    let mut timing = BTreeMap::<(String, String, String, String), f64>::new();
    for row in &scale {
        timing.insert(
            (
                scale.field(row, "selector")?,
                scale.field(row, "predicate")?,
                scale.field(row, "arm")?,
                scale.field(row, "threads")?,
            ),
            scale.field(row, "median_ns")?.parse()?,
        );
    }
    define(
        out,
        "WitScaleRows",
        &group_digits(&scale.field(&scale[0], "rows")?),
    );
    define(
        out,
        "WitScalePages",
        &group_digits(&scale.field(&scale[0], "pages")?),
    );
    let mut one_thread = Vec::new();
    let mut speedup = Vec::new();
    for ((selector, predicate, arm, threads), nanos) in &timing {
        if arm != "witness_direct_facts" || threads != "1" {
            continue;
        }
        let parquet = timing[&(
            selector.clone(),
            predicate.clone(),
            "parquet_plain_indexed".into(),
            "1".into(),
        )];
        one_thread.push(nanos / parquet);
        speedup.push(
            nanos
                / timing[&(
                    selector.clone(),
                    predicate.clone(),
                    arm.clone(),
                    "64".into(),
                )],
        );
    }
    define(out, "WitScaleQueries", &one_thread.len().to_string());
    define(
        out,
        "WitScaleOneThreadOverParquet",
        &format!("{:.2}", median(&one_thread)),
    );
    define(
        out,
        "WitScaleOneThreadWins",
        &one_thread
            .iter()
            .filter(|ratio| **ratio < 1.0)
            .count()
            .to_string(),
    );
    define(
        out,
        "WitScaleSpeedupSixtyFour",
        &format!("{:.1}", median(&speedup)),
    );
    let mut curve = String::new();
    for threads in ["1", "2", "4", "8", "16", "32", "64"] {
        let speedups = timing
            .iter()
            .filter(|((_, _, arm, count), _)| arm == "witness_direct_facts" && count == threads)
            .map(|((selector, predicate, arm, _), nanos)| {
                timing[&(selector.clone(), predicate.clone(), arm.clone(), "1".into())] / nanos
            })
            .collect::<Vec<_>>();
        curve.push_str(&format!("({threads},{:.2})", median(&speedups)));
    }
    define(out, "WitScaleSpeedupPlot", &curve);
    // On as many threads as the machine has physical cores.
    let thirty_two = timing
        .iter()
        .filter(|((_, _, arm, count), _)| arm == "witness_direct_facts" && count == "32")
        .map(|((selector, predicate, arm, _), nanos)| {
            timing[&(selector.clone(), predicate.clone(), arm.clone(), "1".into())] / nanos
        })
        .collect::<Vec<_>>();
    define(
        out,
        "WitScaleSpeedupThirtyTwo",
        &format!("{:.1}", median(&thirty_two)),
    );
    // The same curve for the planner that scans every page: execution alone,
    // with no query kept on few threads because it skips most pages.
    let mut scan_curve = String::new();
    for threads in ["1", "2", "4", "8", "16", "32", "64"] {
        let speedups = timing
            .iter()
            .filter(|((_, _, arm, count), _)| arm == "witness_direct_scan" && count == threads)
            .map(|((selector, predicate, arm, _), nanos)| {
                timing[&(selector.clone(), predicate.clone(), arm.clone(), "1".into())] / nanos
            })
            .collect::<Vec<_>>();
        scan_curve.push_str(&format!("({threads},{:.2})", median(&speedups)));
        if threads == "32" || threads == "64" {
            define(
                out,
                &format!(
                    "WitScaleScanSpeedup{}",
                    if threads == "32" {
                        "ThirtyTwo"
                    } else {
                        "SixtyFour"
                    }
                ),
                &format!("{:.1}", median(&speedups)),
            );
        }
    }
    define(out, "WitScaleScanSpeedupPlot", &scan_curve);
    paged_tables(out)?;
    for (predicate, suffix) in [("0.1pct", "Tenth"), ("50pct", "FiftyPct")] {
        let key = |arm: &str| {
            (
                "c0_l_orderkey_x1".to_string(),
                predicate.to_string(),
                arm.to_string(),
                "1".to_string(),
            )
        };
        let witness = timing
            .get(&key("witness_direct_facts"))
            .ok_or("scale orderkey row is absent")?;
        let parquet = timing
            .get(&key("parquet_plain_indexed"))
            .ok_or("scale orderkey row is absent")?;
        define(
            out,
            &format!("WitScaleOrderkey{suffix}Ms"),
            &format!("{:.2}", witness / 1e6),
        );
        define(
            out,
            &format!("WitScaleOrderkey{suffix}ParquetMs"),
            &format!("{:.2}", parquet / 1e6),
        );
        // Parquet's time on this query moves between runs far more than
        // Witness's, so its range across runs is reported beside it.
        let mut runs = Vec::new();
        for dir in std::iter::once(PAGED_SCALE.to_string())
            .chain(REPEATS.iter().map(|dir| format!("{dir}/paged_scale")))
        {
            let scaling = read_csv(&format!("{dir}/scaling.csv"))?;
            for row in &scaling {
                if scaling.field(row, "selector")? == "c0_l_orderkey_x1"
                    && scaling.field(row, "predicate")? == predicate
                    && scaling.field(row, "arm")? == "parquet_plain_indexed"
                    && scaling.field(row, "threads")? == "1"
                {
                    runs.push(scaling.field(row, "median_ns")?.parse::<f64>()? / 1e6);
                }
            }
        }
        runs.sort_by(f64::total_cmp);
        let (Some(low), Some(high)) = (runs.first(), runs.last()) else {
            return Err("scale orderkey rows are absent from a run".into());
        };
        define(
            out,
            &format!("WitScaleOrderkey{suffix}ParquetMsRunLow"),
            &format!("{low:.2}"),
        );
        define(
            out,
            &format!("WitScaleOrderkey{suffix}ParquetMsRunHigh"),
            &format!("{high:.2}"),
        );
    }
    Ok(())
}

const DUCKDB: &str = "experiments/results/duckdb";

/// Page-size sensitivity runs: their page size, as the study names its
/// directory, and the suffix of their claims.
const PAGE_SIZES: [(&str, &str); 2] = [("4096", "FourK"), ("65536", "SixtyFourK")];

/// Target-selectivity buckets of the paged workload and their claim suffixes.
const BUCKETS: [(&str, &str); 4] = [
    ("0.1pct", "Tenth"),
    ("1pct", "OnePct"),
    ("10pct", "TenPct"),
    ("50pct", "FiftyPct"),
];

/// Median latency per (pair, predicate) and arm.
type Latency = BTreeMap<(String, String), BTreeMap<String, f64>>;

fn paged_latency(dir: &str) -> Result<Latency, Box<dyn Error>> {
    let queries = read_csv(&format!("{dir}/queries.csv"))?;
    let mut latency = Latency::new();
    for row in &queries {
        latency
            .entry((
                queries.field(row, "pair")?,
                queries.field(row, "predicate")?,
            ))
            .or_default()
            .insert(
                queries.field(row, "arm")?,
                queries.field(row, "median_ns")?.parse()?,
            );
    }
    add_selectivity(&mut latency, dir)?;
    Ok(latency)
}

/// Key under which a latency cell records the fraction of its pair's rows
/// the query matched. It is not an arm; no code iterates over arm names.
const SELECTIVITY: &str = "selectivity";

/// Records in every cell of `latency` the fraction of rows its query
/// matched, from the study's `queries.csv` and `pairs.csv` in `dir`.
fn add_selectivity(latency: &mut Latency, dir: &str) -> Result<(), Box<dyn Error>> {
    let pairs = read_csv(&format!("{dir}/pairs.csv"))?;
    let rows = pairs
        .iter()
        .map(|row| {
            Ok((
                pairs.field(row, "pair")?,
                pairs.field(row, "rows")?.parse::<f64>()?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>, Box<dyn Error>>>()?;
    let queries = read_csv(&format!("{dir}/queries.csv"))?;
    for row in &queries {
        let cell = (
            queries.field(row, "pair")?,
            queries.field(row, "predicate")?,
        );
        let total = rows.get(&cell.0).ok_or("query names an unknown pair")?;
        let matched = queries.field(row, "matched_rows")?.parse::<f64>()?;
        if let Some(arms) = latency.get_mut(&cell) {
            arms.insert(SELECTIVITY.into(), matched / total);
        }
    }
    Ok(())
}

/// The selectivity bucket a query belongs to by the fraction of rows it
/// matched, not by the window it asked for: on a column with many equal
/// values the quantile windows coincide, and a "0.1%" window can match most
/// of the column. The boundaries are the geometric midpoints between the
/// targets 0.1%, 1%, 10% and 50%.
fn selectivity_bucket(selectivity: f64) -> &'static str {
    if selectivity < (0.001_f64 * 0.01).sqrt() {
        "0.1pct"
    } else if selectivity < (0.01_f64 * 0.1).sqrt() {
        "1pct"
    } else if selectivity < (0.1_f64 * 0.5).sqrt() {
        "10pct"
    } else {
        "50pct"
    }
}

/// Whether a latency cell's measured selectivity falls in `bucket`.
fn in_bucket(arms: &BTreeMap<String, f64>, bucket: &str) -> bool {
    arms.get(SELECTIVITY)
        .is_some_and(|selectivity| selectivity_bucket(*selectivity) == bucket)
}

/// `numerator / denominator` for every cell that measured both arms,
/// optionally in one selectivity bucket.
fn arm_ratios(
    latency: &Latency,
    numerator: &str,
    denominator: &str,
    bucket: Option<&str>,
) -> Vec<f64> {
    latency
        .iter()
        .filter(|(_, arms)| bucket.is_none_or(|bucket| in_bucket(arms, bucket)))
        .filter_map(|(_, arms)| Some(arms.get(numerator)? / arms.get(denominator)?))
        .collect()
}

/// `arm_ratios` over every cell, failing when no cell measured both arms, as
/// when a study's results predate one of them.
fn measured_ratios(
    latency: &Latency,
    numerator: &str,
    denominator: &str,
) -> Result<Vec<f64>, Box<dyn Error>> {
    let ratios = arm_ratios(latency, numerator, denominator, None);
    if ratios.is_empty() {
        return Err(format!("no query measured both {numerator} and {denominator}").into());
    }
    Ok(ratios)
}

fn geometric_mean(values: &[f64]) -> f64 {
    (values.iter().map(|value| value.ln()).sum::<f64>() / values.len() as f64).exp()
}

/// Claims that separate what derived facts buy from what the executor buys
/// and from what stored statistics buy: the three planners on one executor,
/// Parquet without its page index, other page sizes, the slack of derived
/// bounds, fault injection against the checker, and DuckDB on the same
/// queries.
fn separation_claims(out: &mut ClaimManifest) -> Result<(), Box<dyn Error>> {
    let latency = paged_latency(PAGED)?;
    let facts_over_zonemap = arm_ratios(
        &latency,
        "witness_direct_facts",
        "witness_direct_zonemap",
        None,
    );
    define(
        out,
        "WitPagedFactsOverZonemapGeo",
        &format!("{:.2}", geometric_mean(&facts_over_zonemap)),
    );
    define(
        out,
        "WitPagedFactsOverZonemapMedian",
        &format!("{:.2}", median(&facts_over_zonemap)),
    );
    for (name, keep) in [
        (
            "WitPagedFactsBeatZonemap",
            (|ratio: f64| ratio < 0.95) as fn(f64) -> bool,
        ),
        ("WitPagedFactsTrailZonemap", |ratio: f64| ratio > 1.05),
    ] {
        let count = facts_over_zonemap
            .iter()
            .filter(|ratio| keep(**ratio))
            .count();
        define(out, name, &group_digits(&count.to_string()));
    }
    for (bucket, suffix) in BUCKETS {
        let selected = arm_ratios(
            &latency,
            "witness_direct_facts",
            "witness_direct_zonemap",
            Some(bucket),
        );
        define(
            out,
            &format!("WitPagedFactsOverZonemap{suffix}"),
            &format!("{:.2}", median(&selected)),
        );
    }
    // Against a zone map with a dictionary filter, which skips without
    // reading ids every page whose dictionary holds no value in the range,
    // as facts do; a 95% interval resamples pairs.
    let facts_over_filter = measured_ratios(
        &latency,
        "witness_direct_facts",
        "witness_direct_zonemapdict",
    )?;
    define(
        out,
        "WitPagedFactsOverZonemapDictGeo",
        &format!("{:.2}", geometric_mean(&facts_over_filter)),
    );
    define(
        out,
        "WitPagedFactsOverZonemapDictMedian",
        &format!("{:.2}", median(&facts_over_filter)),
    );
    let clusters = pair_ratios(
        &latency,
        "witness_direct_facts",
        "witness_direct_zonemapdict",
    );
    let (low, high) = pair_bootstrap(&clusters, geometric_mean, 2_000);
    define(
        out,
        "WitPagedFactsOverZonemapDictGeoCiLow",
        &format!("{low:.2}"),
    );
    define(
        out,
        "WitPagedFactsOverZonemapDictGeoCiHigh",
        &format!("{high:.2}"),
    );
    let filter_over_zonemap = measured_ratios(
        &latency,
        "witness_direct_zonemapdict",
        "witness_direct_zonemap",
    )?;
    define(
        out,
        "WitPagedZonemapDictOverZonemapGeo",
        &format!("{:.2}", geometric_mean(&filter_over_zonemap)),
    );
    let zonemap_over_scan = arm_ratios(
        &latency,
        "witness_direct_zonemap",
        "witness_direct_scan",
        None,
    );
    define(
        out,
        "WitPagedZonemapOverScanGeo",
        &format!("{:.2}", geometric_mean(&zonemap_over_scan)),
    );
    let zonemap_over_plain = arm_ratios(
        &latency,
        "witness_direct_zonemap",
        "parquet_plain_indexed",
        None,
    );
    define(
        out,
        "WitPagedZonemapOverPlainMedian",
        &format!("{:.2}", median(&zonemap_over_plain)),
    );

    // Against whichever Parquet read is faster for each query: with or
    // without the page index.
    let best = latency
        .values()
        .filter_map(|arms| {
            let parquet = arms
                .get("parquet_plain_indexed")?
                .min(*arms.get("parquet_plain_full")?);
            Some(arms.get("witness_direct_facts")? / parquet)
        })
        .collect::<Vec<_>>();
    define(
        out,
        "WitPagedFactsOverBestPlainMedian",
        &format!("{:.2}", median(&best)),
    );
    define(
        out,
        "WitPagedFactsOverBestPlainWins",
        &group_digits(
            &best
                .iter()
                .filter(|ratio| **ratio < 1.0)
                .count()
                .to_string(),
        ),
    );

    for (bucket, suffix) in BUCKETS {
        let selected = latency
            .iter()
            .filter(|(_, arms)| in_bucket(arms, bucket))
            .filter_map(|(_, arms)| {
                let parquet = arms
                    .get("parquet_plain_indexed")?
                    .min(*arms.get("parquet_plain_full")?);
                Some(arms.get("witness_direct_facts")? / parquet)
            })
            .collect::<Vec<_>>();
        define(
            out,
            &format!("WitPagedFactsOverBestPlain{suffix}"),
            &format!("{:.2}", median(&selected)),
        );
    }

    // Parquet without its page index: what a missing zone map costs.
    let full_over_indexed = arm_ratios(
        &latency,
        "parquet_plain_full",
        "parquet_plain_indexed",
        None,
    );
    define(
        out,
        "WitPagedNoIndexOverIndexedMedian",
        &format!("{:.2}", median(&full_over_indexed)),
    );
    let tenth = arm_ratios(
        &latency,
        "parquet_plain_full",
        "parquet_plain_indexed",
        Some("0.1pct"),
    );
    define(
        out,
        "WitPagedNoIndexOverIndexedTenth",
        &format!("{:.2}", median(&tenth)),
    );
    let facts_over_full = arm_ratios(&latency, "witness_direct_facts", "parquet_plain_full", None);
    define(
        out,
        "WitPagedFactsOverNoIndexMedian",
        &format!("{:.2}", median(&facts_over_full)),
    );

    // Page size: the same corpus and queries at 4 Ki and 64 Ki rows a page,
    // and a table row per page size including the main run's.
    let size_row = |dir: &str, latency: &Latency| -> Result<String, Box<dyn Error>> {
        let plain = arm_ratios(
            latency,
            "witness_direct_facts",
            "parquet_plain_indexed",
            None,
        );
        let zonemap = arm_ratios(
            latency,
            "witness_direct_facts",
            "witness_direct_zonemap",
            None,
        );
        let pairs = read_csv(&format!("{dir}/pairs.csv"))?;
        let bytes = pairs
            .iter()
            .map(|row| {
                Ok(pairs.field(row, "witness_direct_bytes")?.parse::<f64>()?
                    / pairs.field(row, "parquet_plain_bytes")?.parse::<f64>()?)
            })
            .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
        let costs = read_csv(&format!("{dir}/costs.csv"))?;
        let plan_share = costs
            .iter()
            .map(|row| {
                Ok(costs.field(row, "plan_ns")?.parse::<f64>()?
                    / costs.field(row, "scan_ns")?.parse::<f64>()?)
            })
            .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
        Ok(format!(
            "{:.2} & {:.2} & {:.2} & {:.2}",
            median(&plain),
            geometric_mean(&zonemap),
            median(&bytes),
            median(&plan_share) * 100.0
        ))
    };
    let mut size_rows = BTreeMap::<usize, String>::new();
    let main_freeze = read_csv(&format!("{PAGED}/freeze.csv"))?;
    size_rows.insert(
        main_freeze.field(&main_freeze[0], "page_rows")?.parse()?,
        size_row(PAGED, &latency)?,
    );
    for (rows, suffix) in PAGE_SIZES {
        let dir = format!("{PAGED}_rows{rows}");
        let freeze = read_csv(&format!("{dir}/freeze.csv"))?;
        if freeze.field(&freeze[0], "page_rows")? != rows {
            return Err(format!("{dir} was measured at another page size").into());
        }
        let latency = paged_latency(&dir)?;
        let plain = arm_ratios(
            &latency,
            "witness_direct_facts",
            "parquet_plain_indexed",
            None,
        );
        define(
            out,
            &format!("WitPagedRows{suffix}PlainMedian"),
            &format!("{:.2}", median(&plain)),
        );
        let zonemap = arm_ratios(
            &latency,
            "witness_direct_facts",
            "witness_direct_zonemap",
            None,
        );
        define(
            out,
            &format!("WitPagedRows{suffix}FactsOverZonemapGeo"),
            &format!("{:.2}", geometric_mean(&zonemap)),
        );
        let pairs = read_csv(&format!("{dir}/pairs.csv"))?;
        let bytes = pairs
            .iter()
            .map(|row| {
                Ok(pairs.field(row, "witness_direct_bytes")?.parse::<f64>()?
                    / pairs.field(row, "parquet_plain_bytes")?.parse::<f64>()?)
            })
            .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
        define(
            out,
            &format!("WitPagedRows{suffix}DirectOverPlainBytes"),
            &format!("{:.2}", median(&bytes)),
        );
        size_rows.insert(rows.parse()?, size_row(&dir, &latency)?);
    }
    let mut table = String::new();
    for (rows, cells) in &size_rows {
        writeln!(table, "{} & {cells} \\\\", group_digits(&rows.to_string()))?;
    }
    define(out, "WitPagedSizeRows", table.trim_end());
    // The most planning costs against a scan, at any page size.
    let mut plan_shares = Vec::new();
    for dir in std::iter::once(PAGED.to_string()).chain(
        PAGE_SIZES
            .iter()
            .map(|(rows, _)| format!("{PAGED}_rows{rows}")),
    ) {
        let costs = read_csv(&format!("{dir}/costs.csv"))?;
        let shares = costs
            .iter()
            .map(|row| {
                Ok(costs.field(row, "plan_ns")?.parse::<f64>()?
                    / costs.field(row, "scan_ns")?.parse::<f64>()?)
            })
            .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
        plan_shares.push(median(&shares) * 100.0);
    }
    define(
        out,
        "WitPagedSizePlanMaxPercent",
        &format!("{:.2}", plan_shares.iter().copied().fold(0.0, f64::max)),
    );

    // Slack of derived bounds against exact statistics, over pages that
    // hold a value.
    let bounds = read_csv(&format!("{PAGED}/bounds.csv"))?;
    let (mut valued, mut bounded, mut exact) = (0_usize, 0_usize, 0_usize);
    let mut slack = Vec::new();
    for row in &bounds {
        let stat_low = bounds.field(row, "stat_low")?;
        if stat_low.is_empty() {
            continue;
        }
        valued += 1;
        let bound_low = bounds.field(row, "bound_low")?;
        if bound_low.is_empty() {
            continue;
        }
        bounded += 1;
        let (stat_low, stat_high) = (
            stat_low.parse::<f64>()?,
            bounds.field(row, "stat_high")?.parse::<f64>()?,
        );
        let (bound_low, bound_high) = (
            bound_low.parse::<f64>()?,
            bounds.field(row, "bound_high")?.parse::<f64>()?,
        );
        if (bound_low, bound_high) == (stat_low, stat_high) {
            exact += 1;
        } else {
            slack.push((bound_high - bound_low + 1.0) / (stat_high - stat_low + 1.0));
        }
    }
    define(out, "WitBoundsPages", &group_digits(&valued.to_string()));
    define(
        out,
        "WitBoundsBoundedPercent",
        &format!("{:.1}", bounded as f64 * 100.0 / valued as f64),
    );
    define(
        out,
        "WitBoundsExactPercent",
        &format!("{:.1}", exact as f64 * 100.0 / bounded as f64),
    );
    define(
        out,
        "WitBoundsSlackMedian",
        &format!("{:.2}", median(&slack)),
    );

    // Fault injection against the checker.
    let checker = read_csv(&format!("{PAGED}/checker.csv"))?;
    let mut tally = BTreeMap::<(String, String), usize>::new();
    for row in &checker {
        *tally
            .entry((
                checker.field(row, "checker")?,
                checker.field(row, "outcome")?,
            ))
            .or_default() += checker.field(row, "mutants")?.parse::<usize>()?;
    }
    let total = |verdict: Option<&str>, harmful: Option<bool>| {
        tally
            .iter()
            .filter(|((checked, _), _)| verdict.is_none_or(|verdict| checked == verdict))
            .filter(|((_, outcome), _)| {
                harmful.is_none_or(|harmful| (outcome != "correct") == harmful)
            })
            .map(|(_, count)| count)
            .sum::<usize>()
    };
    // Per family: mutants, harmful, harmful rejected, harmless rejected,
    // accepted. Every mutant is harmful, harmless and rejected, or accepted.
    let mut families = BTreeMap::<String, [usize; 5]>::new();
    for row in &checker {
        let counts = families.entry(checker.field(row, "family")?).or_default();
        let mutants = checker.field(row, "mutants")?.parse::<usize>()?;
        let accepted = checker.field(row, "checker")? == "accepted";
        let harmful = checker.field(row, "outcome")? != "correct";
        counts[0] += mutants;
        counts[1] += if harmful { mutants } else { 0 };
        counts[2] += if harmful && !accepted { mutants } else { 0 };
        counts[3] += if !harmful && !accepted { mutants } else { 0 };
        counts[4] += if accepted { mutants } else { 0 };
    }
    let mut table = String::new();
    for (family, label) in [
        ("kind", "another action kind"),
        ("evidence", "a moved bound"),
        ("foreign_page", "a neighbouring page's action"),
        ("stale_plan", "another predicate's action"),
        ("forged_statistics", "forged statistics"),
    ] {
        let [mutants, harmful, rejected, harmless_rejected, accepted] = families
            .get(family)
            .copied()
            .ok_or("checker study lacks a mutation family")?;
        if mutants != rejected + harmless_rejected + accepted || harmful != rejected {
            return Err(format!("checker family {family} does not add up").into());
        }
        writeln!(
            table,
            "{label} & {} & {} & {} & {} & {} \\\\",
            group_digits(&mutants.to_string()),
            group_digits(&harmful.to_string()),
            group_digits(&rejected.to_string()),
            group_digits(&harmless_rejected.to_string()),
            group_digits(&accepted.to_string()),
        )?;
    }
    define(out, "WitCheckerFamilyRows", table.trim_end());
    if total(Some("accepted"), Some(true)) != 0 {
        return Err("the checker accepted a mutant that answers wrongly".into());
    }
    for (name, value) in [
        ("WitCheckerMutants", total(None, None)),
        ("WitCheckerAccepted", total(Some("accepted"), None)),
        ("WitCheckerHarmful", total(None, Some(true))),
        (
            "WitCheckerHarmfulRejected",
            total(Some("rejected"), Some(true)),
        ),
        (
            "WitCheckerHarmlessRejected",
            total(Some("rejected"), Some(false)),
        ),
    ] {
        define(out, name, &group_digits(&value.to_string()));
    }

    // DuckDB on the same queries, one thread, against the fact planner.
    let duck = read_csv(&format!("{DUCKDB}/paged.csv"))?;
    let mut client = BTreeMap::<&str, Vec<f64>>::new();
    let mut buckets = BTreeMap::<String, Vec<f64>>::new();
    let mut scanned = Vec::new();
    for row in &duck {
        let pair = duck
            .field(row, "table")?
            .strip_prefix("pair_")
            .ok_or("DuckDB paged table is not a pair")?
            .to_string();
        let predicate = duck.field(row, "predicate")?;
        let cell = latency
            .get(&(pair, predicate.clone()))
            .ok_or("DuckDB measured a query the paged study did not")?;
        let witness = cell
            .get("witness_direct_facts")
            .ok_or("DuckDB measured a query the paged study did not")?;
        let engine = duck.field(row, "engine")?;
        let ratio = witness / duck.field(row, "median_ns")?.parse::<f64>()?;
        if engine == "duckdb_native" {
            scanned.push(
                duck.field(row, "rows_scanned")?.parse::<f64>()?
                    / duck.field(row, "rows")?.parse::<f64>()?,
            );
            let selectivity = cell
                .get(SELECTIVITY)
                .ok_or("paged query lacks its selectivity")?;
            let bucket = selectivity_bucket(*selectivity).to_string();
            buckets.entry(bucket).or_default().push(ratio);
            client.entry("WitDuckNative").or_default().push(ratio);
        } else {
            client.entry("WitDuckParquet").or_default().push(ratio);
        }
    }
    for (name, ratios) in &client {
        define(
            out,
            &format!("{name}Median"),
            &format!("{:.2}", median(ratios)),
        );
        define(
            out,
            &format!("{name}Wins"),
            &group_digits(
                &ratios
                    .iter()
                    .filter(|ratio| **ratio < 1.0)
                    .count()
                    .to_string(),
            ),
        );
        define(
            out,
            &format!("{name}Queries"),
            &group_digits(&ratios.len().to_string()),
        );
    }
    define(
        out,
        "WitDuckScannedPercent",
        &format!("{:.0}", median(&scanned) * 100.0),
    );
    for (bucket, suffix) in BUCKETS {
        let ratios = buckets
            .get(bucket)
            .ok_or("DuckDB measured no query in a selectivity bucket")?;
        define(
            out,
            &format!("WitDuckNative{suffix}"),
            &format!("{:.2}", median(ratios)),
        );
    }

    // DuckDB on the 60 M-row columns, 1 and 64 threads.
    let scale = read_csv(&format!("{PAGED_SCALE}/scaling.csv"))?;
    let mut witness = BTreeMap::<(String, String, String), f64>::new();
    for row in &scale {
        if scale.field(row, "arm")? == "witness_direct_facts" {
            witness.insert(
                (
                    scale.field(row, "selector")?,
                    scale.field(row, "predicate")?,
                    scale.field(row, "threads")?,
                ),
                scale.field(row, "median_ns")?.parse()?,
            );
        }
    }
    let duck = read_csv(&format!("{DUCKDB}/scale.csv"))?;
    let mut by_threads = BTreeMap::<String, Vec<f64>>::new();
    for row in &duck {
        if duck.field(row, "engine")? != "duckdb_native" {
            continue;
        }
        let key = (
            duck.field(row, "table")?,
            duck.field(row, "predicate")?,
            duck.field(row, "threads")?,
        );
        let nanos = duck.field(row, "median_ns")?.parse::<f64>()?;
        let ours = witness
            .get(&key)
            .ok_or("DuckDB measured a scale query the scale study did not")?;
        by_threads
            .entry(key.2.clone())
            .or_default()
            .push(ours / nanos);
        if key.0 == "c0_l_orderkey_x1" && key.2 == "1" {
            for (predicate, suffix) in [("0.1pct", "Tenth"), ("50pct", "FiftyPct")] {
                if key.1 == predicate {
                    define(
                        out,
                        &format!("WitDuckScaleOrderkey{suffix}Ms"),
                        &format!("{:.2}", nanos / 1e6),
                    );
                }
            }
        }
    }
    for (threads, suffix) in [("1", "One"), ("64", "SixtyFour")] {
        let ratios = by_threads
            .get(threads)
            .ok_or("DuckDB scale run lacks a thread count")?;
        define(
            out,
            &format!("WitDuckScale{suffix}Median"),
            &format!("{:.2}", median(ratios)),
        );
        define(
            out,
            &format!("WitDuckScale{suffix}Wins"),
            &ratios
                .iter()
                .filter(|ratio| **ratio < 1.0)
                .count()
                .to_string(),
        );
    }
    let version = read_csv(&format!("{DUCKDB}/freeze.csv"))?;
    define(
        out,
        "WitDuckVersion",
        &version.field(&version[0], "duckdb_version")?,
    );
    let floor = version
        .field(&version[0], "client_floor_ns")?
        .parse::<f64>()?;
    define(out, "WitDuckClientFloorUs", &format!("{:.0}", floor / 1e3));
    Ok(())
}

const CONJUNCTIVE: &str = "experiments/results/paged_conjunctive";

/// A summary of a sample of ratios.
type Statistic = fn(&[f64]) -> f64;

/// Median time per (query, arm, threads).
type QueryTimes = BTreeMap<(String, String, String), f64>;
const PARQUET_FACTS: &str = "experiments/results/parquet_facts";
/// Repeated runs of the timed paged studies, beside the canonical one.
const REPEATS: [&str; 2] = [
    "experiments/results/repeats/run2",
    "experiments/results/repeats/run3",
];

/// Median query time per (query, arm, threads) of the conjunctive study,
/// with DuckDB's native engine as arm `duckdb`.
fn conjunctive_times(dir: &str, duckdb: &str) -> Result<QueryTimes, Box<dyn Error>> {
    let queries = read_csv(&format!("{dir}/queries.csv"))?;
    let mut times = BTreeMap::new();
    for row in &queries {
        times.insert(
            (
                queries.field(row, "query")?,
                queries.field(row, "arm")?,
                queries.field(row, "threads")?,
            ),
            queries.field(row, "median_ns")?.parse::<f64>()?,
        );
    }
    let duck = read_csv(&format!("{duckdb}/conjunctive.csv"))?;
    for row in &duck {
        if duck.field(row, "engine")? == "duckdb_native" {
            times.insert(
                (
                    duck.field(row, "predicate")?,
                    "duckdb".to_string(),
                    duck.field(row, "threads")?,
                ),
                duck.field(row, "median_ns")?.parse::<f64>()?,
            );
        }
    }
    Ok(times)
}

/// Claims from the conjunctive study: TPC-H Q6 on SF10 lineitem and the
/// same query on a range of the sorted order key, under each planner and on
/// DuckDB, on 1 and 64 threads.
fn conjunctive_claims(out: &mut ClaimManifest) -> Result<(), Box<dyn Error>> {
    let times = conjunctive_times(CONJUNCTIVE, DUCKDB)?;
    let get = |query: &str, arm: &str, threads: &str| {
        times
            .get(&(query.to_string(), arm.to_string(), threads.to_string()))
            .copied()
            .ok_or_else(|| format!("conjunctive study lacks {query} {arm} x{threads}"))
    };
    let mut table = String::new();
    for (query, label) in [("q6", "Q6"), ("q6_orderkey", "Q6, key range")] {
        for threads in ["1", "64"] {
            let cells = [
                "witness_direct_facts",
                "witness_direct_zonemap",
                "witness_direct_scan",
                "duckdb",
            ]
            .iter()
            .map(|arm| Ok(format!("{:.1}", get(query, arm, threads)? / 1e6)))
            .collect::<Result<Vec<_>, String>>()?;
            writeln!(table, "{label} & {threads} & {} \\\\", cells.join(" & "))?;
        }
    }
    define(out, "WitConjunctiveRows", table.trim_end());
    for (query, prefix) in [("q6", "WitQSix"), ("q6_orderkey", "WitQSixKey")] {
        for (threads, suffix) in [("1", "One"), ("64", "SixtyFour")] {
            define(
                out,
                &format!("{prefix}FactsOverDuck{suffix}"),
                &format!(
                    "{:.2}",
                    get(query, "witness_direct_facts", threads)? / get(query, "duckdb", threads)?
                ),
            );
            define(
                out,
                &format!("{prefix}FactsOverZonemap{suffix}"),
                &format!(
                    "{:.2}",
                    get(query, "witness_direct_facts", threads)?
                        / get(query, "witness_direct_zonemap", threads)?
                ),
            );
        }
    }
    let queries = read_csv(&format!("{CONJUNCTIVE}/queries.csv"))?;
    for row in &queries {
        if queries.field(row, "query")? == "q6_orderkey"
            && queries.field(row, "arm")? == "witness_direct_facts"
            && queries.field(row, "threads")? == "1"
        {
            let skipped = queries.field(row, "skipped")?.parse::<f64>()?;
            let pages = queries.field(row, "pages")?.parse::<f64>()?;
            define(
                out,
                "WitQSixKeySkippedPercent",
                &format!("{:.1}", skipped * 100.0 / pages),
            );
        }
    }
    Ok(())
}

/// Claims from reading Parquet's delta encoding headers as facts.
fn parquet_facts_claims(out: &mut ClaimManifest) -> Result<(), Box<dyn Error>> {
    let queries = read_csv(&format!("{PARQUET_FACTS}/queries.csv"))?;
    let mut sums = [0_usize; 4];
    for row in &queries {
        for (slot, name) in [
            "exact_skipped",
            "exact_accepted",
            "header_skipped",
            "header_accepted",
        ]
        .into_iter()
        .enumerate()
        {
            sums[slot] += queries.field(row, name)?.parse::<usize>()?;
        }
    }
    let percent =
        |part: usize, whole: usize| format!("{:.1}", part as f64 * 100.0 / whole.max(1) as f64);
    define(
        out,
        "WitParquetHeaderSkipRecovery",
        &percent(sums[2], sums[0]),
    );
    define(
        out,
        "WitParquetHeaderAcceptRecovery",
        &percent(sums[3], sums[1]),
    );
    let pages = read_csv(&format!("{PARQUET_FACTS}/pages.csv"))?;
    let (mut valued, mut ordered, mut exact, mut header_bytes, mut value_bytes) = (0, 0, 0, 0, 0);
    for row in &pages {
        if pages.field(row, "stat_low")?.is_empty() {
            continue;
        }
        valued += 1;
        ordered += usize::from(pages.field(row, "non_decreasing")? == "true");
        exact += usize::from(
            pages.field(row, "header_low")? == pages.field(row, "stat_low")?
                && pages.field(row, "header_high")? == pages.field(row, "stat_high")?,
        );
        header_bytes += pages.field(row, "header_bytes")?.parse::<usize>()?;
        value_bytes += pages.field(row, "value_bytes")?.parse::<usize>()?;
    }
    define(out, "WitParquetPages", &group_digits(&valued.to_string()));
    define(
        out,
        "WitParquetNonDecreasingPercent",
        &percent(ordered, valued),
    );
    define(out, "WitParquetExactBoundsPercent", &percent(exact, valued));
    define(
        out,
        "WitParquetHeaderBytesPercent",
        &format!("{:.2}", header_bytes as f64 * 100.0 / value_bytes as f64),
    );
    Ok(())
}

/// Bootstrap over pairs: resample whole pairs with replacement, since the
/// queries of one pair share columns and are not independent.
fn pair_bootstrap(clusters: &[Vec<f64>], statistic: Statistic, repetitions: usize) -> (f64, f64) {
    assert!(!clusters.is_empty());
    let mut state = 0x6a09_e667_f3bc_c909_u64;
    let mut estimates = Vec::with_capacity(repetitions);
    let mut sample = Vec::new();
    for _ in 0..repetitions {
        sample.clear();
        for _ in 0..clusters.len() {
            sample.extend_from_slice(&clusters[xorshift64(&mut state) as usize % clusters.len()]);
        }
        estimates.push(statistic(&sample));
    }
    estimates.sort_by(f64::total_cmp);
    (
        estimates[(repetitions * 25 / 1_000).min(repetitions - 1)],
        estimates[(repetitions * 975 / 1_000).min(repetitions - 1)],
    )
}

/// `numerator / denominator` per pair, for every query that measured both.
fn pair_ratios(latency: &Latency, numerator: &str, denominator: &str) -> Vec<Vec<f64>> {
    let mut clusters = BTreeMap::<&str, Vec<f64>>::new();
    for ((pair, _), arms) in latency {
        if let (Some(top), Some(bottom)) = (arms.get(numerator), arms.get(denominator)) {
            clusters.entry(pair).or_default().push(top / bottom);
        }
    }
    clusters.into_values().collect()
}

/// `numerator / denominator` per source table, for every query that measured
/// both: the pairs of one table share its value column, so a table, not a
/// pair, is the independent unit to resample. Tables are read from the
/// study's `pairs.csv` in `dir`.
fn table_ratios(
    latency: &Latency,
    dir: &str,
    numerator: &str,
    denominator: &str,
) -> Result<Vec<Vec<f64>>, Box<dyn Error>> {
    let pairs = read_csv(&format!("{dir}/pairs.csv"))?;
    let table_of = pairs
        .iter()
        .map(|row| {
            Ok((
                pairs.field(row, "pair")?,
                (pairs.field(row, "group")?, pairs.field(row, "source")?),
            ))
        })
        .collect::<Result<BTreeMap<_, _>, Box<dyn Error>>>()?;
    let mut clusters = BTreeMap::<&(String, String), Vec<f64>>::new();
    for ((pair, _), arms) in latency {
        if let (Some(top), Some(bottom)) = (arms.get(numerator), arms.get(denominator)) {
            let table = table_of.get(pair).ok_or("query names an unknown pair")?;
            clusters.entry(table).or_default().push(top / bottom);
        }
    }
    Ok(clusters.into_values().collect())
}

/// Paged-study latency with DuckDB's native time as arm `duckdb`.
fn paged_latency_with_duckdb(dir: &str, duckdb: &str) -> Result<Latency, Box<dyn Error>> {
    let mut latency = paged_latency(dir)?;
    let duck = read_csv(&format!("{duckdb}/paged.csv"))?;
    for row in &duck {
        if duck.field(row, "engine")? != "duckdb_native" {
            continue;
        }
        let pair = duck
            .field(row, "table")?
            .strip_prefix("pair_")
            .ok_or("DuckDB paged table is not a pair")?
            .to_string();
        latency
            .entry((pair, duck.field(row, "predicate")?))
            .or_default()
            .insert("duckdb".into(), duck.field(row, "median_ns")?.parse()?);
    }
    Ok(latency)
}

/// Median speedup on 64 threads over one in the scale study.
fn scale_speedup(dir: &str) -> Result<f64, Box<dyn Error>> {
    let scale = read_csv(&format!("{dir}/scaling.csv"))?;
    let mut times = BTreeMap::<(String, String, String), f64>::new();
    for row in &scale {
        if scale.field(row, "arm")? == "witness_direct_facts" {
            times.insert(
                (
                    scale.field(row, "selector")?,
                    scale.field(row, "predicate")?,
                    scale.field(row, "threads")?,
                ),
                scale.field(row, "median_ns")?.parse()?,
            );
        }
    }
    let speedups = times
        .iter()
        .filter(|((_, _, threads), _)| threads == "1")
        .filter_map(|((selector, predicate, _), one)| {
            Some(one / times.get(&(selector.clone(), predicate.clone(), "64".to_string()))?)
        })
        .collect::<Vec<_>>();
    Ok(median(&speedups))
}

/// How much the headline numbers move: a 95% bootstrap interval over pairs
/// within the canonical run, and the range over the canonical run and its
/// repeats.
fn uncertainty_claims(out: &mut ClaimManifest) -> Result<(), Box<dyn Error>> {
    let geometric: Statistic = geometric_mean;
    let middle: Statistic = median;
    let headlines: [(&str, &str, &str, Statistic); 3] = [
        (
            "WitPagedFactsOverZonemapGeo",
            "witness_direct_facts",
            "witness_direct_zonemap",
            geometric,
        ),
        (
            "WitPagedPlainMedian",
            "witness_direct_facts",
            "parquet_plain_indexed",
            middle,
        ),
        (
            "WitDuckNativeMedian",
            "witness_direct_facts",
            "duckdb",
            middle,
        ),
    ];
    let runs = std::iter::once((
        PAGED.to_string(),
        PAGED_SCALE.to_string(),
        CONJUNCTIVE.to_string(),
        DUCKDB.to_string(),
    ))
    .chain(REPEATS.iter().map(|dir| {
        (
            format!("{dir}/paged_study"),
            format!("{dir}/paged_scale"),
            format!("{dir}/paged_conjunctive"),
            format!("{dir}/duckdb"),
        )
    }))
    .collect::<Vec<_>>();
    let canonical = paged_latency_with_duckdb(PAGED, DUCKDB)?;
    for (name, numerator, denominator, statistic) in headlines {
        let clusters = pair_ratios(&canonical, numerator, denominator);
        let (low, high) = pair_bootstrap(&clusters, statistic, 2_000);
        define(out, &format!("{name}CiLow"), &format!("{low:.2}"));
        define(out, &format!("{name}CiHigh"), &format!("{high:.2}"));
        let tables = table_ratios(&canonical, PAGED, numerator, denominator)?;
        let (low, high) = pair_bootstrap(&tables, statistic, 2_000);
        define(out, &format!("{name}TableCiLow"), &format!("{low:.2}"));
        define(out, &format!("{name}TableCiHigh"), &format!("{high:.2}"));
        let mut values = Vec::new();
        for (paged, _, _, duckdb) in &runs {
            let latency = paged_latency_with_duckdb(paged, duckdb)?;
            let ratios = pair_ratios(&latency, numerator, denominator).concat();
            values.push(statistic(&ratios));
        }
        values.sort_by(f64::total_cmp);
        define(out, &format!("{name}RunLow"), &format!("{:.2}", values[0]));
        define(
            out,
            &format!("{name}RunHigh"),
            &format!("{:.2}", values[values.len() - 1]),
        );
    }
    let mut speedups = Vec::new();
    let mut q6 = Vec::new();
    for (_, scale, conjunctive, duckdb) in &runs {
        speedups.push(scale_speedup(scale)?);
        let times = conjunctive_times(conjunctive, duckdb)?;
        let key =
            |arm: &str, threads: &str| ("q6".to_string(), arm.to_string(), threads.to_string());
        q6.push(
            times
                .get(&key("witness_direct_facts", "1"))
                .ok_or("Q6 run lacks facts")?
                / times
                    .get(&key("duckdb", "1"))
                    .ok_or("Q6 run lacks DuckDB")?,
        );
    }
    for (name, mut values) in [
        ("WitScaleSpeedupSixtyFour", speedups),
        ("WitQSixFactsOverDuckOne", q6),
    ] {
        values.sort_by(f64::total_cmp);
        let digits = if name.starts_with("WitScale") { 1 } else { 2 };
        define(
            out,
            &format!("{name}RunLow"),
            &format!("{:.*}", digits, values[0]),
        );
        define(
            out,
            &format!("{name}RunHigh"),
            &format!("{:.*}", digits, values[values.len() - 1]),
        );
    }
    define(out, "WitRuns", &runs.len().to_string());
    Ok(())
}

const FLOAT: &str = "experiments/results/paged_float";
const STRING: &str = "experiments/results/paged_string";
const COLD: &str = "experiments/results/paged_cold";

/// Median time per (pair, predicate) and arm of a pair study, with DuckDB's
/// native time from `duckdb_file` as arm `duckdb`.
fn pair_latency(dir: &str, duckdb_file: &str) -> Result<Latency, Box<dyn Error>> {
    let mut latency = paged_latency(dir)?;
    let duck = read_csv(duckdb_file)?;
    for row in &duck {
        if duck.field(row, "engine")? != "duckdb_native" {
            continue;
        }
        let pair = duck
            .field(row, "table")?
            .strip_prefix("pair_")
            .ok_or("DuckDB table is not a pair")?
            .to_string();
        latency
            .entry((pair, duck.field(row, "predicate")?))
            .or_default()
            .insert("duckdb".into(), duck.field(row, "median_ns")?.parse()?);
    }
    Ok(latency)
}

/// Claims for floating-point and string selectors, and a table row per
/// data type comparing the planners and DuckDB.
fn type_claims(out: &mut ClaimManifest) -> Result<(), Box<dyn Error>> {
    let integers = paged_latency_with_duckdb(PAGED, DUCKDB)?;
    let floats = pair_latency(FLOAT, &format!("{DUCKDB}/float.csv"))?;
    let strings = pair_latency(STRING, &format!("{DUCKDB}/string.csv"))?;
    let mut table = String::new();
    for (label, prefix, latency, pairs_dir) in [
        ("integers", "WitInt", &integers, PAGED),
        ("floats", "WitFloat", &floats, FLOAT),
        ("strings", "WitString", &strings, STRING),
    ] {
        let zonemap = arm_ratios(
            latency,
            "witness_direct_facts",
            "witness_direct_zonemap",
            None,
        );
        let scan = arm_ratios(latency, "witness_direct_facts", "witness_direct_scan", None);
        let duck = arm_ratios(latency, "witness_direct_facts", "duckdb", None);
        let pairs = read_csv(&format!("{pairs_dir}/pairs.csv"))?.len();
        define(
            out,
            &format!("{prefix}Pairs"),
            &group_digits(&pairs.to_string()),
        );
        define(
            out,
            &format!("{prefix}Queries"),
            &group_digits(&latency.len().to_string()),
        );
        define(
            out,
            &format!("{prefix}FactsOverZonemapGeo"),
            &format!("{:.2}", geometric_mean(&zonemap)),
        );
        // A 95% interval that resamples source tables.
        let tables = table_ratios(
            latency,
            pairs_dir,
            "witness_direct_facts",
            "witness_direct_zonemap",
        )?;
        let (low, high) = pair_bootstrap(&tables, geometric_mean, 2_000);
        define(
            out,
            &format!("{prefix}FactsOverZonemapGeoCiLow"),
            &format!("{low:.2}"),
        );
        define(
            out,
            &format!("{prefix}FactsOverZonemapGeoCiHigh"),
            &format!("{high:.2}"),
        );
        define(
            out,
            &format!("{prefix}FactsOverScanGeo"),
            &format!("{:.2}", geometric_mean(&scan)),
        );
        define(
            out,
            &format!("{prefix}DuckMedian"),
            &format!("{:.2}", median(&duck)),
        );
        define(
            out,
            &format!("{prefix}DuckWins"),
            &group_digits(
                &duck
                    .iter()
                    .filter(|ratio| **ratio < 1.0)
                    .count()
                    .to_string(),
            ),
        );
        writeln!(
            table,
            "{label} & {} & {} & {:.2} & {:.2} & {:.2} \\\\",
            group_digits(&pairs.to_string()),
            group_digits(&latency.len().to_string()),
            geometric_mean(&zonemap),
            geometric_mean(&scan),
            median(&duck),
        )?;
    }
    define(out, "WitTypeRows", table.trim_end());
    let zonemap = arm_ratios(
        &strings,
        "witness_direct_facts",
        "witness_direct_zonemap",
        None,
    );
    for (name, keep) in [
        (
            "WitStringFactsBeatZonemap",
            (|ratio: f64| ratio < 0.95) as fn(f64) -> bool,
        ),
        ("WitStringFactsTrailZonemap", |ratio: f64| ratio > 1.05),
    ] {
        let count = zonemap.iter().filter(|ratio| keep(**ratio)).count();
        define(out, name, &group_digits(&count.to_string()));
    }
    // The zone map with the encoding's shortcuts (its order-preserving
    // translation and the skips between entries) and with a dictionary
    // filter, which attribute what facts gain on strings.
    for (name, numerator, denominator) in [
        (
            "WitStringFactsOverZonemapPlusGeo",
            "witness_direct_facts",
            "witness_direct_zonemapplus",
        ),
        (
            "WitStringFactsOverZonemapDictGeo",
            "witness_direct_facts",
            "witness_direct_zonemapdict",
        ),
        (
            "WitStringZonemapPlusOverZonemapGeo",
            "witness_direct_zonemapplus",
            "witness_direct_zonemap",
        ),
        (
            "WitStringZonemapDictOverZonemapGeo",
            "witness_direct_zonemapdict",
            "witness_direct_zonemap",
        ),
    ] {
        let ratios = measured_ratios(&strings, numerator, denominator)?;
        define(out, name, &format!("{:.2}", geometric_mean(&ratios)));
    }
    // Dictionary entries each planner reads, and the pages it skips, over
    // all string queries.
    let queries = read_csv(&format!("{STRING}/queries.csv"))?;
    let mut probes = BTreeMap::<String, f64>::new();
    let mut skipped = BTreeMap::<String, usize>::new();
    for row in &queries {
        let arm = queries.field(row, "arm")?;
        *probes.entry(arm.clone()).or_default() += queries.field(row, "probes")?.parse::<f64>()?;
        *skipped.entry(arm).or_default() += queries.field(row, "skipped")?.parse::<usize>()?;
    }
    for (name, arm) in [
        ("WitStringFactsSkips", "witness_direct_facts"),
        ("WitStringZonemapSkips", "witness_direct_zonemap"),
        ("WitStringZonemapPlusSkips", "witness_direct_zonemapplus"),
        ("WitStringZonemapDictSkips", "witness_direct_zonemapdict"),
    ] {
        let pages = skipped
            .get(arm)
            .ok_or_else(|| format!("string study lacks {arm}"))?;
        define(out, name, &group_digits(&pages.to_string()));
    }
    let get = |arm: &str| probes.get(arm).copied().ok_or("string study lacks an arm");
    define(
        out,
        "WitStringProbesFactsMillions",
        &format!("{:.1}", get("witness_direct_facts")? / 1e6),
    );
    define(
        out,
        "WitStringProbesScanMillions",
        &format!("{:.0}", get("witness_direct_scan")? / 1e6),
    );
    define(
        out,
        "WitStringProbesRatio",
        &format!(
            "{:.0}",
            get("witness_direct_scan")? / get("witness_direct_facts")?
        ),
    );
    let groups = read_csv(&format!("{STRING}/pairs.csv"))?
        .iter()
        .map(|row| row[1].clone())
        .collect::<BTreeSet<_>>();
    define(out, "WitStringGroups", &groups.len().to_string());
    Ok(())
}

/// Claims from the cold-read study: each planner's median time and reads,
/// for the sorted key and for unclustered columns, and what auditing each
/// footer costs.
fn cold_claims(out: &mut ClaimManifest) -> Result<(), Box<dyn Error>> {
    let queries = read_csv(&format!("{COLD}/queries.csv"))?;
    // Median over repeats per (selector, predicate, arm).
    let mut runs = BTreeMap::<(String, String, String), Vec<(f64, f64)>>::new();
    for row in &queries {
        runs.entry((
            queries.field(row, "selector")?,
            queries.field(row, "predicate")?,
            queries.field(row, "arm")?,
        ))
        .or_default()
        .push((
            queries.field(row, "ns")?.parse::<f64>()?,
            queries.field(row, "io_operations")?.parse::<f64>()?,
        ));
    }
    let cell = |selector_is_key: bool, predicate: Option<&str>, arm: &str| {
        let times = runs
            .iter()
            .filter(|((selector, label, kind), _)| {
                kind == arm
                    && selector.contains("orderkey") == selector_is_key
                    && predicate.is_none_or(|predicate| label.split('+').next() == Some(predicate))
            })
            .map(|(_, values)| median(&values.iter().map(|value| value.0).collect::<Vec<_>>()))
            .collect::<Vec<_>>();
        let reads = runs
            .iter()
            .filter(|((selector, label, kind), _)| {
                kind == arm
                    && selector.contains("orderkey") == selector_is_key
                    && predicate.is_none_or(|predicate| label.split('+').next() == Some(predicate))
            })
            .map(|(_, values)| values[0].1)
            .collect::<Vec<_>>();
        (median(&times) / 1e6, median(&reads))
    };
    let arms = [
        "scan",
        "zonemap_footer",
        "facts_footer",
        "facts_pages",
        "facts_pages_once",
    ];
    let mut table = String::new();
    for (label, key, predicate) in [
        ("sorted key, 0.1\\%", true, Some("0.1pct")),
        ("sorted key, 50\\%", true, Some("50pct")),
        ("unclustered", false, None),
    ] {
        let cells = arms
            .iter()
            .map(|arm| {
                let (ms, reads) = cell(key, predicate, arm);
                format!("{ms:.1} ({})", group_digits(&format!("{reads:.0}")))
            })
            .collect::<Vec<_>>();
        writeln!(table, "{label} & {} \\\\", cells.join(" & "))?;
    }
    define(out, "WitColdRows", table.trim_end());
    let (scan, _) = cell(false, None, "scan");
    let (pages, _) = cell(false, None, "facts_pages");
    define(
        out,
        "WitColdPagesOverScanUnclustered",
        &format!("{:.1}", pages / scan),
    );
    // Planning from one read of each page, which execution reuses.
    let (once, _) = cell(false, None, "facts_pages_once");
    define(
        out,
        "WitColdOnceOverScanUnclustered",
        &format!("{:.2}", once / scan),
    );
    let (key_scan, _) = cell(true, Some("50pct"), "scan");
    let (key_once, _) = cell(true, Some("50pct"), "facts_pages_once");
    let (key_footer, _) = cell(true, Some("50pct"), "facts_footer");
    define(
        out,
        "WitColdOnceOverScanKeyFiftyPct",
        &format!("{:.2}", key_once / key_scan),
    );
    define(
        out,
        "WitColdOnceOverFooterKeyFiftyPct",
        &format!("{:.2}", key_once / key_footer),
    );
    let audits = read_csv(&format!("{COLD}/audit.csv"))?;
    for (summary, prefix) in [
        ("derived_bounds", "WitColdAuditBounds"),
        ("zone_map", "WitColdAuditZone"),
    ] {
        let mut times = Vec::new();
        let mut threaded = Vec::new();
        let mut bytes = Vec::new();
        for row in &audits {
            if audits.field(row, "summary")? != summary {
                continue;
            }
            let ms = audits.field(row, "ns")?.parse::<f64>()? / 1e6;
            if audits.field(row, "threads")? == "1" {
                times.push(ms);
                bytes.push(audits.field(row, "io_bytes")?.parse::<f64>()?);
            } else {
                threaded.push(ms);
            }
        }
        define(
            out,
            &format!("{prefix}Ms"),
            &format!("{:.0}", median(&times)),
        );
        define(
            out,
            &format!("{prefix}MsThreaded"),
            &format!("{:.0}", median(&threaded)),
        );
        define(
            out,
            &format!("{prefix}Kib"),
            &format!("{:.0}", median(&bytes) / 1024.0),
        );
        define(
            out,
            &format!("{prefix}Mib"),
            &format!("{:.0}", median(&bytes) / (1024.0 * 1024.0)),
        );
    }
    let threads = audits
        .iter()
        .map(|row| audits.field(row, "threads"))
        .collect::<Result<BTreeSet<_>, _>>()?;
    define(
        out,
        "WitColdAuditThreads",
        threads
            .iter()
            .rfind(|count| *count != "1")
            .ok_or("cold audit ran on one thread only")?,
    );
    let freeze = read_csv(&format!("{COLD}/freeze.csv"))?;
    define(out, "WitColdRepeats", &freeze.field(&freeze[0], "repeats")?);
    Ok(())
}

/// The planner ablation: where skips and accepts come from (stored
/// statistics or the encoding) crossed with whether the encoding's shortcuts
/// (search, code translation, the column certificate) are allowed, each
/// relative to the plain zone map; for the integer and float studies, with a
/// bootstrap interval over pairs. Also Q6 under every planner, and which
/// actions each encoding received.
fn ablation_claims(out: &mut ClaimManifest) -> Result<(), Box<dyn Error>> {
    let cells = [
        ("witness_direct_zonemap", "stored", "no"),
        ("witness_direct_factbounds", "derived", "no"),
        ("witness_direct_zonemapplus", "stored", "yes"),
        ("witness_direct_facts", "derived", "yes"),
    ];
    let geometric: Statistic = geometric_mean;
    let mut table = String::new();
    for (label, prefix, dir, latency) in [
        ("integers", "WitInt", PAGED, paged_latency(PAGED)?),
        ("floats", "WitFloat", FLOAT, paged_latency(FLOAT)?),
    ] {
        let mut row = Vec::new();
        for (arm, bounds, shortcuts) in cells {
            let clusters = table_ratios(&latency, dir, arm, "witness_direct_zonemap")?;
            let value = geometric_mean(&clusters.concat());
            let (low, high) = pair_bootstrap(&clusters, geometric, 2_000);
            let name = format!(
                "{prefix}{}{}OverZonemap",
                if bounds == "derived" {
                    "Derived"
                } else {
                    "Stored"
                },
                if shortcuts == "yes" { "Shortcuts" } else { "" }
            );
            define(out, &name, &format!("{value:.2}"));
            define(out, &format!("{name}CiLow"), &format!("{low:.2}"));
            define(out, &format!("{name}CiHigh"), &format!("{high:.2}"));
            row.push(format!("{value:.2} [{low:.2}, {high:.2}]"));
        }
        writeln!(table, "{label} & {} \\\\", row.join(" & "))?;
    }
    define(out, "WitAblationRows", table.trim_end());

    // Q6 and its key-range variant under every planner, 1 and 64 threads.
    let times = conjunctive_times(CONJUNCTIVE, DUCKDB)?;
    let get = |query: &str, arm: &str, threads: &str| {
        times
            .get(&(query.to_string(), arm.to_string(), threads.to_string()))
            .copied()
            .ok_or_else(|| format!("conjunctive study lacks {query} {arm} x{threads}"))
    };
    let mut table = String::new();
    for (query, label) in [("q6", "Q6"), ("q6_orderkey", "Q6, key range")] {
        for threads in ["1", "64"] {
            let cells = [
                "witness_direct_facts",
                "witness_direct_factbounds",
                "witness_direct_zonemapplus",
                "witness_direct_zonemap",
                "witness_direct_scan",
                "duckdb",
            ]
            .iter()
            .map(|arm| Ok(format!("{:.1}", get(query, arm, threads)? / 1e6)))
            .collect::<Result<Vec<_>, String>>()?;
            writeln!(table, "{label} & {threads} & {} \\\\", cells.join(" & "))?;
        }
    }
    define(out, "WitConjunctiveAblationRows", table.trim_end());
    define(
        out,
        "WitQSixFactsOverFactBoundsOne",
        &format!(
            "{:.2}",
            get("q6", "witness_direct_facts", "1")? / get("q6", "witness_direct_factbounds", "1")?
        ),
    );
    define(
        out,
        "WitQSixZonemapPlusOverZonemapOne",
        &format!(
            "{:.2}",
            get("q6", "witness_direct_zonemapplus", "1")?
                / get("q6", "witness_direct_zonemap", "1")?
        ),
    );
    let freeze = read_csv(&format!("{CONJUNCTIVE}/freeze.csv"))?;
    define(
        out,
        "WitConjunctiveRounds",
        &freeze.field(&freeze[0], "rounds")?,
    );

    // Actions the facts planner gave each encoding over all paged queries.
    let actions = read_csv(&format!("{PAGED}/actions.csv"))?;
    let mut by_recipe = BTreeMap::<String, BTreeMap<String, f64>>::new();
    for row in &actions {
        *by_recipe
            .entry(actions.field(row, "recipe")?)
            .or_default()
            .entry(actions.field(row, "action")?)
            .or_default() += actions.field(row, "pages")?.parse::<f64>()?;
    }
    let shortcut = ["search", "search_piecewise", "translate", "skip_codes"];
    let licensed = by_recipe
        .values()
        .filter(|actions| {
            actions
                .iter()
                .any(|(action, pages)| action != "scan" && *pages > 0.0)
        })
        .count();
    let with_shortcuts = by_recipe
        .values()
        .filter(|actions| {
            shortcut
                .iter()
                .any(|action| actions.get(*action).is_some_and(|pages| *pages > 0.0))
        })
        .count();
    define(out, "WitEncodingsDistinct", &by_recipe.len().to_string());
    define(out, "WitEncodingsLicensed", &licensed.to_string());
    define(
        out,
        "WitEncodingsWithShortcuts",
        &with_shortcuts.to_string(),
    );
    let mut ranked = by_recipe
        .iter()
        .map(|(recipe, actions)| (actions.values().sum::<f64>(), recipe, actions))
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| right.0.total_cmp(&left.0));
    let mut table = String::new();
    for (total, recipe, actions) in ranked.into_iter().take(6) {
        let share = |names: &[&str]| {
            names
                .iter()
                .map(|name| actions.get(*name).copied().unwrap_or(0.0))
                .sum::<f64>()
                * 100.0
                / total
        };
        writeln!(
            table,
            "\\texttt{{{}}} & {:.0} & {:.0} & {:.0} & {:.0} \\\\",
            recipe.replace('_', "\\_"),
            share(&["skip", "accept"]),
            share(&["search", "search_piecewise"]),
            share(&["translate"]),
            share(&["scan"]),
        )?;
    }
    define(out, "WitEncodingRows", table.trim_end());
    Ok(())
}

/// Display names for corpus groups, in table order.
const GROUPS: [(&str, &str); 7] = [
    ("publicbi", "Public BI"),
    ("tpcds", "TPC-DS"),
    ("tpch", "TPC-H"),
    ("clickbench", "ClickBench"),
    ("nab", "NAB"),
    ("household", "Household"),
    ("large", "NYC taxi"),
];

/// Table rows for the paper: page decisions by target selectivity, paged
/// results by corpus group, and census incidence by corpus group.
fn paged_tables(out: &mut ClaimManifest) -> Result<(), Box<dyn Error>> {
    let pairs = read_csv(&format!("{PAGED}/pairs.csv"))?;
    let queries = read_csv(&format!("{PAGED}/queries.csv"))?;
    let group_of = pairs
        .iter()
        .map(|row| Ok((pairs.field(row, "pair")?, pairs.field(row, "group")?)))
        .collect::<Result<BTreeMap<_, _>, Box<dyn Error>>>()?;

    let mut decisions = BTreeMap::<String, [usize; 7]>::new();
    let mut bucket_selectivity = BTreeMap::<String, Vec<f64>>::new();
    let latency = paged_latency(PAGED)?;
    let mut group_pages = BTreeMap::<String, [usize; 2]>::new();
    for row in &queries {
        let predicate = queries.field(row, "predicate")?;
        let pair = queries.field(row, "pair")?;
        let arm = queries.field(row, "arm")?;
        if arm != "witness_direct_facts" {
            continue;
        }
        let selectivity = latency
            .get(&(pair.clone(), predicate.clone()))
            .and_then(|arms| arms.get(SELECTIVITY))
            .ok_or("paged query lacks its selectivity")?;
        let bucket = selectivity_bucket(*selectivity).to_string();
        bucket_selectivity
            .entry(bucket.clone())
            .or_default()
            .push(*selectivity);
        let entry = decisions.entry(bucket).or_default();
        for (slot, name) in [
            "pages",
            "skipped",
            "accepted",
            "searched",
            "translated",
            "scanned",
            "code_skipped",
        ]
        .into_iter()
        .enumerate()
        {
            entry[slot] += queries.field(row, name)?.parse::<usize>()?;
        }
        let group = group_of.get(&pair).ok_or("query names an unknown pair")?;
        let pages = group_pages.entry(group.clone()).or_default();
        pages[0] += queries.field(row, "pages")?.parse::<usize>()?;
        pages[1] += queries.field(row, "skipped")?.parse::<usize>()?;
    }
    let share = |part: usize, whole: usize| format!("{:.1}", part as f64 * 100.0 / whole as f64);
    let mut table = String::new();
    for (bucket, label) in [
        ("0.1pct", "0.1"),
        ("1pct", "1"),
        ("10pct", "10"),
        ("50pct", "50"),
    ] {
        let [
            pages,
            skipped,
            accepted,
            searched,
            translated,
            scanned,
            code_skipped,
        ] = *decisions
            .get(bucket)
            .ok_or("selectivity bucket is absent")?;
        let selectivities = &bucket_selectivity[bucket];
        // Skips by bound (and by the column's order) apart from skips
        // because the page's dictionary holds no value in the range.
        writeln!(
            table,
            "{label}\\% & {} & {:.2} & {} & {} & {} & {} & {} & {} \\\\",
            group_digits(&selectivities.len().to_string()),
            median(selectivities) * 100.0,
            share(skipped - code_skipped, pages),
            share(code_skipped, pages),
            share(accepted, pages),
            share(searched, pages),
            share(translated, pages),
            share(scanned, pages)
        )?;
    }
    define(out, "WitPagedDecisionRows", table.trim_end());
    for (bucket, suffix) in BUCKETS {
        let selectivities = bucket_selectivity
            .get(bucket)
            .ok_or("selectivity bucket is absent")?;
        define(
            out,
            &format!("WitPagedBucketQueries{suffix}"),
            &group_digits(&selectivities.len().to_string()),
        );
        define(
            out,
            &format!("WitPagedBucketSelectivity{suffix}"),
            &format!("{:.2}", median(selectivities) * 100.0),
        );
    }
    let distinct = bucket_selectivity.values().map(Vec::len).sum::<usize>();
    let requested = 4 * pairs.len();
    define(
        out,
        "WitPagedRequestedWindows",
        &group_digits(&requested.to_string()),
    );
    define(
        out,
        "WitPagedMergedWindows",
        &group_digits(&(requested - distinct).to_string()),
    );

    let mut table = String::new();
    for (group, label) in GROUPS {
        let ratio = |denominator: &str| {
            latency
                .iter()
                .filter(|((pair, _), _)| group_of.get(pair).is_some_and(|name| name == group))
                .filter_map(|(_, arms)| {
                    Some(arms.get("witness_direct_facts")? / arms.get(denominator)?)
                })
                .collect::<Vec<_>>()
        };
        let plain = ratio("parquet_plain_indexed");
        if plain.is_empty() {
            continue;
        }
        let zstd = ratio("parquet_zstd_indexed");
        let pair_count = group_of.values().filter(|name| *name == group).count();
        let [pages, skipped] = group_pages[group];
        writeln!(
            table,
            "{label} & {pair_count} & {:.2} & {:.2} & {} \\\\",
            median(&plain),
            median(&zstd),
            share(skipped, pages)
        )?;
    }
    define(out, "WitPagedGroupRows", table.trim_end());

    let columns = read_csv(&format!("{CENSUS}/columns.csv"))?;
    let pages = read_csv(&format!("{CENSUS}/pages.csv"))?;
    let mut census = BTreeMap::<String, [usize; 6]>::new();
    let mut column_group = BTreeMap::<String, String>::new();
    for row in &columns {
        let group = columns.field(row, "group")?;
        column_group.insert(columns.field(row, "column")?, group.clone());
        let unique = columns.field(row, "unique_non_null")?.parse::<usize>()?;
        let entry = census.entry(group).or_default();
        entry[0] += 1;
        entry[1] +=
            usize::from(unique > 1 && columns.field(row, "global_monotone_non_null")? == "true");
    }
    for row in &pages {
        if pages.field(row, "page_size")? != "1024" {
            continue;
        }
        let group = column_group
            .get(&pages.field(row, "column")?)
            .ok_or("census page names an unknown column")?;
        let unique = pages.field(row, "unique_non_null")?.parse::<usize>()?;
        let entry = census.get_mut(group).ok_or("census group is absent")?;
        entry[2] += 1;
        entry[3] += usize::from(unique == 0);
        entry[4] += usize::from(unique == 1);
        entry[5] += usize::from(unique > 1 && pages.field(row, "monotone_non_null")? == "true");
    }
    let mut table = String::new();
    for (group, label) in GROUPS {
        let Some(
            &[
                columns,
                monotone,
                page_count,
                all_null,
                constant,
                monotone_pages,
            ],
        ) = census.get(group)
        else {
            continue;
        };
        writeln!(
            table,
            "{label} & {columns} & {} & {} & {} & {} \\\\",
            share(monotone, columns),
            share(all_null, page_count),
            share(constant, page_count),
            share(monotone_pages, page_count)
        )?;
    }
    define(out, "WitCensusGroupIncidenceRows", table.trim_end());

    // The same rows with one more column: of the 1,024-row pages that hold
    // more than one value, the share whose facts license an action a zone map
    // cannot, from the census's per-page facts.
    let facts = read_csv(&format!("{CENSUS}/page_facts.csv"))?;
    let mut beyond = BTreeMap::<String, [usize; 2]>::new();
    for row in &facts {
        if facts.field(row, "page_size")? != "1024" || facts.field(row, "state")? != "values" {
            continue;
        }
        let group = column_group
            .get(&facts.field(row, "column")?)
            .ok_or("page fact names an unknown column")?;
        let licensed = ["search", "piecewise", "translate"]
            .iter()
            .map(|name| facts.field(row, name))
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|value| value == "true");
        let entry = beyond.entry(group.clone()).or_default();
        entry[0] += 1;
        entry[1] += usize::from(licensed);
    }
    let mut table = String::new();
    for (group, label) in GROUPS {
        let Some(
            &[
                columns,
                monotone,
                page_count,
                all_null,
                constant,
                monotone_pages,
            ],
        ) = census.get(group)
        else {
            continue;
        };
        let [values, licensed] = beyond.get(group).copied().unwrap_or_default();
        writeln!(
            table,
            "{label} & {columns} & {} & {} & {} & {} & {} \\\\",
            share(monotone, columns),
            share(all_null, page_count),
            share(constant, page_count),
            share(monotone_pages, page_count),
            if values == 0 {
                "--".to_string()
            } else {
                share(licensed, values)
            }
        )?;
    }
    define(out, "WitCensusFactRows", table.trim_end());
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

/// Declared metadata that is wrong or missing: for each fault, how many paged
/// queries each planner answered wrongly or could not answer.
fn fault_claims(out: &mut ClaimManifest) -> Result<(), Box<dyn Error>> {
    let faults = read_csv(&format!("{PAGED}/faults.csv"))?;
    // queries, affected, correct, wrong, failed
    let mut cells = BTreeMap::<(String, String), [usize; 5]>::new();
    for row in &faults {
        let cell = cells
            .entry((faults.field(row, "fault")?, faults.field(row, "planner")?))
            .or_default();
        for (slot, name) in ["queries", "affected", "correct", "wrong", "failed"]
            .into_iter()
            .enumerate()
        {
            cell[slot] += faults.field(row, name)?.parse::<usize>()?;
        }
    }
    let get = |fault: &str, planner: &str| {
        cells
            .get(&(fault.to_string(), planner.to_string()))
            .copied()
            .ok_or_else(|| format!("fault study lacks {fault} under {planner}"))
    };
    let mut table = String::new();
    let mut facts_wrong = 0;
    for (fault, label, suffix) in [
        ("neighbour", "a neighbouring page's", "Neighbour"),
        ("truncated", "truncated", "Truncated"),
        ("absent", "none", "Absent"),
    ] {
        let zone_map = get(fault, "zonemap")?;
        let plus = get(fault, "zonemapplus")?;
        let footer = get(fault, "footer")?;
        let facts = get(fault, "facts")?;
        facts_wrong += facts[3] + facts[4];
        writeln!(
            table,
            "{label} & {} & {} & {} & {} & {} \\\\",
            group_digits(&zone_map[3].to_string()),
            group_digits(&plus[3].to_string()),
            group_digits(&footer[3].to_string()),
            group_digits(&footer[4].to_string()),
            group_digits(&(facts[3] + facts[4]).to_string()),
        )?;
        define(
            out,
            &format!("WitFaultZonemapWrong{suffix}"),
            &group_digits(&zone_map[3].to_string()),
        );
        define(
            out,
            &format!("WitFaultFooterFailed{suffix}"),
            &group_digits(&footer[4].to_string()),
        );
        define(
            out,
            &format!("WitFaultFooterWrong{suffix}"),
            &group_digits(&footer[3].to_string()),
        );
        define(
            out,
            &format!("WitFaultAffected{suffix}"),
            &group_digits(&zone_map[1].to_string()),
        );
    }
    if facts_wrong != 0 {
        return Err("faulty statistics changed an answer planned from facts".into());
    }
    define(out, "WitFaultRows", table.trim_end());
    define(
        out,
        "WitFaultQueries",
        &group_digits(&get("neighbour", "zonemap")?[0].to_string()),
    );
    Ok(())
}

/// Median query time per (pair, predicate), deletion, and planner in one
/// run of the deletion study, and the pages each plan skipped.
type DeletionTimes = BTreeMap<(String, String), BTreeMap<(String, String), (f64, usize)>>;

fn deletion_times(dir: &str) -> Result<DeletionTimes, Box<dyn Error>> {
    let deletes = read_csv(&format!("{dir}/deletes.csv"))?;
    let mut times = DeletionTimes::new();
    for row in &deletes {
        times
            .entry((
                deletes.field(row, "pair")?,
                deletes.field(row, "predicate")?,
            ))
            .or_default()
            .insert(
                (deletes.field(row, "deletion")?, deletes.field(row, "arm")?),
                (
                    deletes.field(row, "median_ns")?.parse::<f64>()?,
                    deletes.field(row, "skipped")?.parse::<usize>()?,
                ),
            );
    }
    Ok(times)
}

/// Per query, the time with `deletion` over the time with none, both under
/// `planner`.
fn deletion_ratios(times: &DeletionTimes, deletion: &str, planner: &str) -> Vec<f64> {
    times
        .values()
        .filter_map(|cells| {
            let with = cells.get(&(deletion.to_string(), planner.to_string()))?.0;
            let without = cells.get(&("none".to_string(), planner.to_string()))?.0;
            Some(with / without)
        })
        .collect()
}

/// Claims from the deletion study: what deleting a share of each table's
/// rows costs a query planned as though none were deleted, under facts and
/// under the zone map; and that no plan changed.
fn delete_claims(out: &mut ClaimManifest) -> Result<(), Box<dyn Error>> {
    let times = deletion_times(PAGED)?;
    // A plan must not depend on the deletions.
    for ((pair, predicate), cells) in &times {
        for planner in ["facts", "zonemap"] {
            let skips = cells
                .iter()
                .filter(|((_, arm), _)| arm == planner)
                .map(|(_, (_, skipped))| *skipped)
                .collect::<BTreeSet<_>>();
            if skips.len() > 1 {
                return Err(format!(
                    "deleting rows changed the {planner} plan of {pair} {predicate}"
                )
                .into());
            }
        }
    }
    define(
        out,
        "WitDeleteQueries",
        &group_digits(&times.len().to_string()),
    );
    let deletes = read_csv(&format!("{PAGED}/deletes.csv"))?;
    let pairs = read_csv(&format!("{PAGED}/pairs.csv"))?;
    let rows_of = pairs
        .iter()
        .map(|row| {
            Ok((
                pairs.field(row, "pair")?,
                pairs.field(row, "rows")?.parse::<f64>()?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>, Box<dyn Error>>>()?;
    for (deletion, suffix) in [
        ("random1", "RandomOne"),
        ("random10", "RandomTen"),
        ("block10", "BlockTen"),
    ] {
        // Share of each table's rows deleted, from one row per table.
        let mut seen = BTreeSet::new();
        let mut shares = Vec::new();
        for row in &deletes {
            let pair = deletes.field(row, "pair")?;
            if deletes.field(row, "deletion")? == deletion && seen.insert(pair.clone()) {
                let rows = rows_of.get(&pair).ok_or("deletion names an unknown pair")?;
                shares.push(deletes.field(row, "deleted_rows")?.parse::<f64>()? / rows);
            }
        }
        define(
            out,
            &format!("WitDelete{suffix}Percent"),
            &format!("{:.1}", median(&shares) * 100.0),
        );
        define(
            out,
            &format!("WitDelete{suffix}OverNone"),
            &format!("{:.2}", median(&deletion_ratios(&times, deletion, "facts"))),
        );
    }
    define(
        out,
        "WitDeleteZonemapRandomTenOverNone",
        &format!(
            "{:.2}",
            median(&deletion_ratios(&times, "random10", "zonemap"))
        ),
    );
    for (deletion, suffix) in [("none", "None"), ("random10", "RandomTen")] {
        let ratios = times
            .values()
            .filter_map(|cells| {
                let facts = cells.get(&(deletion.to_string(), "facts".to_string()))?.0;
                let zone_map = cells.get(&(deletion.to_string(), "zonemap".to_string()))?.0;
                Some(facts / zone_map)
            })
            .collect::<Vec<_>>();
        define(
            out,
            &format!("WitDeleteFactsOverZonemap{suffix}Geo"),
            &format!("{:.2}", geometric_mean(&ratios)),
        );
    }
    // The cost of deleting a tenth of the rows at random, in every run.
    let mut values = Vec::new();
    for dir in std::iter::once(PAGED.to_string())
        .chain(REPEATS.iter().map(|dir| format!("{dir}/paged_study")))
    {
        values.push(median(&deletion_ratios(
            &deletion_times(&dir)?,
            "random10",
            "facts",
        )));
    }
    values.sort_by(f64::total_cmp);
    define(
        out,
        "WitDeleteRandomTenOverNoneRunLow",
        &format!("{:.2}", values[0]),
    );
    define(
        out,
        "WitDeleteRandomTenOverNoneRunHigh",
        &format!("{:.2}", values[values.len() - 1]),
    );
    Ok(())
}

/// Facts per page under the paged writer, from the census: how many pages
/// with more than one value carry a fact that licenses an action an exact
/// zone map cannot (search, search per block, translation into codes),
/// pooled, per source and per corpus; and how often a page carries a
/// derived bound, how often it is exact, and how wide the rest are.
fn page_fact_claims(out: &mut ClaimManifest) -> Result<(), Box<dyn Error>> {
    let columns = read_csv(&format!("{CENSUS}/columns.csv"))?;
    let source_of = columns
        .iter()
        .map(|row| {
            Ok((
                columns.field(row, "column")?,
                (columns.field(row, "group")?, columns.field(row, "source")?),
            ))
        })
        .collect::<Result<BTreeMap<_, _>, Box<dyn Error>>>()?;
    let facts = read_csv(&format!("{CENSUS}/page_facts.csv"))?;
    // Per page size: per (group, source): values pages, any, search, translate, piecewise.
    let mut tally = BTreeMap::<(String, (String, String)), [usize; 5]>::new();
    let mut bounds = BTreeMap::<String, [usize; 3]>::new(); // pages with a value, bounded, exact
    let mut slack = BTreeMap::<String, Vec<f64>>::new();
    let truthy = |value: String| value == "true";
    for row in &facts {
        let size = facts.field(row, "page_size")?;
        let state = facts.field(row, "state")?;
        if state == "all_null" {
            continue;
        }
        let bound = bounds.entry(size.clone()).or_default();
        bound[0] += 1;
        bound[1] += usize::from(truthy(facts.field(row, "bounded")?));
        bound[2] += usize::from(truthy(facts.field(row, "exact")?));
        let ratio = facts.field(row, "width_ratio")?;
        if !ratio.is_empty() {
            slack.entry(size.clone()).or_default().push(ratio.parse()?);
        }
        if state != "values" {
            continue;
        }
        let source = source_of
            .get(&facts.field(row, "column")?)
            .ok_or("page fact names an unknown column")?
            .clone();
        let cell = tally.entry((size, source)).or_default();
        let (search, translate, piecewise) = (
            truthy(facts.field(row, "search")?),
            truthy(facts.field(row, "translate")?),
            truthy(facts.field(row, "piecewise")?),
        );
        cell[0] += 1;
        cell[1] += usize::from(search || translate || piecewise);
        cell[2] += usize::from(search);
        cell[3] += usize::from(translate);
        cell[4] += usize::from(piecewise);
    }
    let percent = |part: usize, whole: usize| format!("{:.1}", part as f64 * 100.0 / whole as f64);
    for (size, suffix) in [("1024", ""), ("16384", "Large")] {
        let mut pooled = [0_usize; 5];
        let mut shares = Vec::new();
        for ((page_size, _), cell) in &tally {
            if page_size != size {
                continue;
            }
            for slot in 0..5 {
                pooled[slot] += cell[slot];
            }
            if cell[0] > 0 {
                shares.push(cell[1] as f64 / cell[0] as f64);
            }
        }
        define(
            out,
            &format!("WitPageFactsPages{suffix}"),
            &group_digits(&pooled[0].to_string()),
        );
        define(
            out,
            &format!("WitPageFactsHOne{suffix}"),
            &percent(pooled[1], pooled[0]),
        );
        define(
            out,
            &format!("WitPageFactsSearch{suffix}"),
            &percent(pooled[2], pooled[0]),
        );
        define(
            out,
            &format!("WitPageFactsTranslate{suffix}"),
            &percent(pooled[3], pooled[0]),
        );
        define(
            out,
            &format!("WitPageFactsPiecewise{suffix}"),
            &percent(pooled[4], pooled[0]),
        );
        define(
            out,
            &format!("WitPageFactsHOneSourceMedian{suffix}"),
            &format!("{:.1}", median(&shares) * 100.0),
        );
        define(
            out,
            &format!("WitPageFactsHOneSourceMean{suffix}"),
            &format!(
                "{:.1}",
                shares.iter().sum::<f64>() / shares.len() as f64 * 100.0
            ),
        );
        let bound = bounds
            .get(size)
            .ok_or("census has no page facts at this size")?;
        define(
            out,
            &format!("WitPageFactsBounded{suffix}"),
            &percent(bound[1], bound[0]),
        );
        define(
            out,
            &format!("WitPageFactsExact{suffix}"),
            &percent(bound[2], bound[1]),
        );
        define(
            out,
            &format!("WitPageFactsSlackMedian{suffix}"),
            &format!(
                "{:.2}",
                median(slack.get(size).ok_or("census has no bound widths")?)
            ),
        );
    }
    let mut table = String::new();
    for (group, label) in GROUPS {
        let cell = tally
            .iter()
            .filter(|((size, (name, _)), _)| size == "1024" && name == group)
            .fold([0_usize; 5], |mut sum, (_, cell)| {
                for slot in 0..5 {
                    sum[slot] += cell[slot];
                }
                sum
            });
        if cell[0] == 0 {
            continue;
        }
        writeln!(
            table,
            "{label} & {} & {} & {} \\\\",
            percent(cell[1], cell[0]),
            percent(cell[2], cell[0]),
            percent(cell[3], cell[0]),
        )?;
    }
    define(out, "WitPageFactsGroupRows", table.trim_end());
    Ok(())
}

/// How the per-query ratios of the paged study are spread: quantiles of each
/// planner's time over the plain zone map's, for a distribution plot, and
/// how many queries lie within 5% of the zone map.
fn distribution_claims(out: &mut ClaimManifest) -> Result<(), Box<dyn Error>> {
    let latency = paged_latency(PAGED)?;
    for (arm, name) in [
        ("witness_direct_facts", "Facts"),
        ("witness_direct_factbounds", "Bounds"),
        ("witness_direct_zonemapplus", "ZonemapPlus"),
    ] {
        let mut ratios = arm_ratios(&latency, arm, "witness_direct_zonemap", None);
        ratios.sort_by(f64::total_cmp);
        let mut plot = String::new();
        for step in 0..=40 {
            let quantile = step as f64 / 40.0;
            let index = ((ratios.len() - 1) as f64 * quantile).round() as usize;
            plot.push_str(&format!("({:.4},{quantile:.3})", ratios[index]));
        }
        define(out, &format!("WitEcdf{name}OverZonemapPlot"), &plot);
        let within = ratios
            .iter()
            .filter(|ratio| (0.95..=1.05).contains(*ratio))
            .count();
        define(
            out,
            &format!("WitPaged{name}WithinFive"),
            &format!("{:.1}", within as f64 * 100.0 / ratios.len() as f64),
        );
    }
    Ok(())
}

/// Where the headline ratios come from: the paged queries split by the
/// shortcut the facts planner took, by corpus and by tail; the census's
/// beyond-a-zone-map share split by fact; the scale study against the zone
/// map; the actions by the evidence they rest on; what verifying
/// dictionaries, the checker and the cold audit cost or catch; DuckDB by
/// query time; and the stored bytes in total.
fn breakdown_claims(out: &mut ClaimManifest) -> Result<(), Box<dyn Error>> {
    paged_category_claims(out)?;
    paged_spread_claims(out)?;
    census_split_claims(out)?;
    scale_zonemap_claims(out)?;
    evidence_class_claims(out)?;
    verify_cost_claims(out)?;
    checker_mechanism_claims(out)?;
    cold_audit_ratio_claims(out)?;
    duck_strata_claims(out)?;
    byte_total_claims(out)?;
    Ok(())
}

/// The value at quantile `quantile` of `values`, by the nearest rank the
/// distribution plot uses.
fn quantile(values: &[f64], quantile: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[((sorted.len() - 1) as f64 * quantile).round() as usize]
}

/// The paged queries split by the shortcut the facts planner took, each
/// with facts over the zone map. The four categories partition the queries;
/// a query falls in the first that applies:
///
/// 1. `Gap`: some page was skipped because its sorted dictionary holds no
///    value in the range (`code_skipped > 0`);
/// 2. `Search`: some page was searched, or the column's order certificate
///    skipped pages that derived bounds alone did not. The counters fold
///    the certificate's skips into `skipped`, so they show as facts
///    skipping more pages, besides dictionary gaps, than the planner with
///    derived bounds only;
/// 3. `Translate`: some page's range was translated into codes;
/// 4. `None`: every page was skipped or accepted by a bound, or scanned.
fn paged_category_claims(out: &mut ClaimManifest) -> Result<(), Box<dyn Error>> {
    let queries = read_csv(&format!("{PAGED}/queries.csv"))?;
    // Counters of the facts and derived-bounds planners per query.
    let mut counters = BTreeMap::<(String, String), [usize; 5]>::new();
    for row in &queries {
        let arm = queries.field(row, "arm")?;
        let slots: &[(usize, &str)] = match arm.as_str() {
            "witness_direct_facts" => &[
                (0, "code_skipped"),
                (1, "searched"),
                (2, "translated"),
                (3, "skipped"),
            ],
            "witness_direct_factbounds" => &[(4, "skipped")],
            _ => continue,
        };
        let cell = counters
            .entry((
                queries.field(row, "pair")?,
                queries.field(row, "predicate")?,
            ))
            .or_default();
        for (slot, name) in slots {
            cell[*slot] = queries.field(row, name)?.parse()?;
        }
    }
    let latency = paged_latency(PAGED)?;
    let mut categories = BTreeMap::<&str, Vec<f64>>::new();
    for (cell, arms) in &latency {
        let [code_skipped, searched, translated, skipped, bound_skipped] = counters
            .get(cell)
            .copied()
            .ok_or("paged query lacks the facts planner's counters")?;
        let category = if code_skipped > 0 {
            "Gap"
        } else if searched > 0 || skipped - code_skipped > bound_skipped {
            "Search"
        } else if translated > 0 {
            "Translate"
        } else {
            "None"
        };
        let ratio = arms
            .get("witness_direct_facts")
            .zip(arms.get("witness_direct_zonemap"))
            .map(|(facts, zone_map)| facts / zone_map)
            .ok_or("paged query lacks the facts or zone-map time")?;
        categories.entry(category).or_default().push(ratio);
    }
    for category in ["Gap", "Search", "Translate", "None"] {
        let ratios = categories.get(category).cloned().unwrap_or_default();
        define(
            out,
            &format!("WitPagedCategory{category}Queries"),
            &group_digits(&ratios.len().to_string()),
        );
        if ratios.is_empty() {
            continue;
        }
        define(
            out,
            &format!("WitPagedCategory{category}Geo"),
            &format!("{:.2}", geometric_mean(&ratios)),
        );
        define(
            out,
            &format!("WitPagedCategory{category}Median"),
            &format!("{:.2}", median(&ratios)),
        );
    }
    Ok(())
}

/// Corpus groups of the paged study and the suffixes of their claims.
const GROUP_SUFFIXES: [(&str, &str); 7] = [
    ("publicbi", "PublicBi"),
    ("tpcds", "TpcDs"),
    ("tpch", "TpcH"),
    ("clickbench", "ClickBench"),
    ("nab", "Nab"),
    ("household", "Household"),
    ("large", "NycTaxi"),
];

/// How facts over the zone map spread across the paged queries: in total
/// time, per source table, against the zone map with shortcuts, where the
/// zone map decides some page or none, per corpus group, and at the tails.
fn paged_spread_claims(out: &mut ClaimManifest) -> Result<(), Box<dyn Error>> {
    let latency = paged_latency(PAGED)?;
    let (facts, zone_map) = ("witness_direct_facts", "witness_direct_zonemap");
    let totals = latency
        .values()
        .filter_map(|arms| Some((arms.get(facts)?, arms.get(zone_map)?)))
        .fold((0.0, 0.0), |sum, (top, bottom)| {
            (sum.0 + top, sum.1 + bottom)
        });
    define(
        out,
        "WitPagedFactsOverZonemapTotal",
        &format!("{:.2}", totals.0 / totals.1),
    );

    let tables = table_ratios(&latency, PAGED, facts, zone_map)?
        .iter()
        .map(|ratios| geometric_mean(ratios))
        .collect::<Vec<_>>();
    define(
        out,
        "WitPagedFactsOverZonemapTableGeoMedian",
        &format!("{:.2}", median(&tables)),
    );
    define(
        out,
        "WitPagedFactsSlowerTables",
        &tables
            .iter()
            .filter(|ratio| **ratio > 1.0)
            .count()
            .to_string(),
    );

    let plus = arm_ratios(&latency, facts, "witness_direct_zonemapplus", None);
    define(
        out,
        "WitPagedFactsOverZonemapPlusGeo",
        &format!("{:.2}", geometric_mean(&plus)),
    );
    define(
        out,
        "WitPagedFactsOverZonemapPlusMedian",
        &format!("{:.2}", median(&plus)),
    );

    // Queries in which the zone map skips or accepts some page, and the
    // rest, where it scans every page.
    let queries = read_csv(&format!("{PAGED}/queries.csv"))?;
    let mut decided = BTreeSet::new();
    for row in &queries {
        if queries.field(row, "arm")? == zone_map
            && queries.field(row, "skipped")?.parse::<usize>()?
                + queries.field(row, "accepted")?.parse::<usize>()?
                > 0
        {
            decided.insert((
                queries.field(row, "pair")?,
                queries.field(row, "predicate")?,
            ));
        }
    }
    for (name, keep) in [("Pruned", true), ("Unpruned", false)] {
        let ratios = latency
            .iter()
            .filter(|(cell, _)| decided.contains(*cell) == keep)
            .filter_map(|(_, arms)| Some(arms.get(facts)? / arms.get(zone_map)?))
            .collect::<Vec<_>>();
        define(
            out,
            &format!("WitPagedFactsOverZonemap{name}Queries"),
            &group_digits(&ratios.len().to_string()),
        );
        define(
            out,
            &format!("WitPagedFactsOverZonemap{name}Geo"),
            &format!("{:.2}", geometric_mean(&ratios)),
        );
    }

    let pairs = read_csv(&format!("{PAGED}/pairs.csv"))?;
    let group_of = pairs
        .iter()
        .map(|row| Ok((pairs.field(row, "pair")?, pairs.field(row, "group")?)))
        .collect::<Result<BTreeMap<_, _>, Box<dyn Error>>>()?;
    for (group, suffix) in GROUP_SUFFIXES {
        let ratios = latency
            .iter()
            .filter(|((pair, _), _)| group_of.get(pair).is_some_and(|name| name == group))
            .filter_map(|(_, arms)| Some(arms.get(facts)? / arms.get(zone_map)?))
            .collect::<Vec<_>>();
        if ratios.is_empty() {
            continue;
        }
        define(
            out,
            &format!("WitPagedFactsOverZonemap{suffix}Queries"),
            &group_digits(&ratios.len().to_string()),
        );
        define(
            out,
            &format!("WitPagedFactsOverZonemap{suffix}Geo"),
            &format!("{:.2}", geometric_mean(&ratios)),
        );
    }

    let all = arm_ratios(&latency, facts, zone_map, None);
    define(
        out,
        "WitPagedFactsOverZonemapPTen",
        &format!("{:.2}", quantile(&all, 0.1)),
    );
    define(
        out,
        "WitPagedFactsOverZonemapPNinety",
        &format!("{:.2}", quantile(&all, 0.9)),
    );
    Ok(())
}

/// The census's 16,384-row pages whose facts license an action a zone map
/// cannot, split so the parts add up to `WitPageFactsHOneLarge`: pages
/// licensed to search (whole or per block), and pages licensed only to
/// translate into codes. A skip for a dictionary gap depends on the query's
/// range, not on the page alone, so the census does not record it.
fn census_split_claims(out: &mut ClaimManifest) -> Result<(), Box<dyn Error>> {
    let facts = read_csv(&format!("{CENSUS}/page_facts.csv"))?;
    // pages with more than one value, search, translate only
    let mut tally = [0_usize; 3];
    for row in &facts {
        if facts.field(row, "page_size")? != "16384" || facts.field(row, "state")? != "values" {
            continue;
        }
        let search =
            facts.field(row, "search")? == "true" || facts.field(row, "piecewise")? == "true";
        let translate = facts.field(row, "translate")? == "true";
        tally[0] += 1;
        tally[1] += usize::from(search);
        tally[2] += usize::from(translate && !search);
    }
    let percent = |part: usize| format!("{:.1}", part as f64 * 100.0 / tally[0] as f64);
    define(out, "WitPageFactsHOneSearchLarge", &percent(tally[1]));
    define(
        out,
        "WitPageFactsHOneTranslateOnlyLarge",
        &percent(tally[2]),
    );
    Ok(())
}

/// Facts over the zone map on SF10 lineitem, one thread.
fn scale_zonemap_claims(out: &mut ClaimManifest) -> Result<(), Box<dyn Error>> {
    let scale = read_csv(&format!("{PAGED_SCALE}/scaling.csv"))?;
    let mut times = BTreeMap::<(String, String), BTreeMap<String, f64>>::new();
    for row in &scale {
        if scale.field(row, "threads")? != "1" {
            continue;
        }
        times
            .entry((
                scale.field(row, "selector")?,
                scale.field(row, "predicate")?,
            ))
            .or_default()
            .insert(
                scale.field(row, "arm")?,
                scale.field(row, "median_ns")?.parse()?,
            );
    }
    let ratios = times
        .values()
        .filter_map(|arms| {
            Some(arms.get("witness_direct_facts")? / arms.get("witness_direct_zonemap")?)
        })
        .collect::<Vec<_>>();
    define(
        out,
        "WitScaleFactsOverZonemapQueries",
        &ratios.len().to_string(),
    );
    define(
        out,
        "WitScaleFactsOverZonemapGeo",
        &format!("{:.2}", geometric_mean(&ratios)),
    );
    define(
        out,
        "WitScaleFactsOverZonemapMedian",
        &format!("{:.2}", median(&ratios)),
    );
    define(
        out,
        "WitScaleFactsOverZonemapMin",
        &format!("{:.2}", ratios.iter().copied().fold(f64::MAX, f64::min)),
    );
    Ok(())
}

/// The pages the facts planner did not scan, by the evidence their action
/// rests on:
///
/// - `Structural`: a skip or acceptance on a page without a dictionary,
///   from its frame-of-reference bounds, a constant run, or its being all
///   null;
/// - `Dictionary`: a skip or acceptance by the ends of a page's sorted
///   dictionary, a skip for a gap in it, or a translation into codes, all
///   resting on the dictionary verified when the page was sealed;
/// - `Order`: a search, whole or per block, resting on the order the writer
///   attested.
///
/// A skip on a nullable dictionary page is counted as a dictionary bound,
/// though an all-null page would cite its nulls; the writer stores an
/// all-null page without a dictionary. The column's order certificate acts
/// before pages are planned and is not among these actions.
fn evidence_class_claims(out: &mut ClaimManifest) -> Result<(), Box<dyn Error>> {
    let actions = read_csv(&format!("{PAGED}/actions.csv"))?;
    let mut classes = BTreeMap::<&str, f64>::new();
    for row in &actions {
        let recipe = actions.field(row, "recipe")?;
        let class = match actions.field(row, "action")?.as_str() {
            "scan" => continue,
            "search" | "search_piecewise" => "Order",
            "skip_codes" | "translate" => "Dictionary",
            "skip" | "accept" if recipe.contains("Dictionary") => "Dictionary",
            "skip" | "accept" => "Structural",
            other => return Err(format!("unclassified page action {other}").into()),
        };
        *classes.entry(class).or_default() += actions.field(row, "pages")?.parse::<f64>()?;
    }
    let total = classes.values().sum::<f64>();
    define(out, "WitActionPages", &group_digits(&format!("{total:.0}")));
    for class in ["Structural", "Dictionary", "Order"] {
        define(
            out,
            &format!("WitAction{class}Percent"),
            &format!(
                "{:.1}",
                classes.get(class).copied().unwrap_or(0.0) * 100.0 / total
            ),
        );
    }
    Ok(())
}

/// Verifying a page's dictionaries when it is sealed, per page, over the
/// pairs: the median is `WitPagedVerifyNsPerPage`; here the 90th percentile
/// and the most, and the time against one scan of the pair.
fn verify_cost_claims(out: &mut ClaimManifest) -> Result<(), Box<dyn Error>> {
    let costs = read_csv(&format!("{PAGED}/costs.csv"))?;
    let mut per_page = Vec::new();
    let mut over_scan = Vec::new();
    for row in &costs {
        let verify = costs.field(row, "verify_ns")?.parse::<f64>()?;
        per_page.push(verify / costs.field(row, "pages")?.parse::<f64>()?);
        over_scan.push(verify / costs.field(row, "scan_ns")?.parse::<f64>()?);
    }
    define(
        out,
        "WitPagedVerifyNsPerPageNinety",
        &format!("{:.0}", quantile(&per_page, 0.9)),
    );
    define(
        out,
        "WitPagedVerifyNsPerPageMax",
        &format!("{:.0}", per_page.iter().copied().fold(0.0, f64::max)),
    );
    define(
        out,
        "WitPagedVerifyOverScanPercent",
        &format!("{:.1}", median(&over_scan) * 100.0),
    );
    define(
        out,
        "WitPagedVerifyOverScanPercentNinety",
        &format!("{:.1}", quantile(&over_scan, 0.9) * 100.0),
    );
    Ok(())
}

/// The checker's mutants by the mechanism that rejects them: forged
/// statistics never name a fact and are rejected by construction; another
/// action kind is not licensed by the cited fact; a neighbouring page's or
/// another predicate's action fails the semantic re-check against this
/// page and range; a moved bound is never harmful. Harmful mutants are
/// split into wrong answers and executor errors.
fn checker_mechanism_claims(out: &mut ClaimManifest) -> Result<(), Box<dyn Error>> {
    let checker = read_csv(&format!("{PAGED}/checker.csv"))?;
    // wrong, error, harmless rejected
    let mut mechanisms = BTreeMap::<&str, [usize; 3]>::new();
    for row in &checker {
        let mechanism = match checker.field(row, "family")?.as_str() {
            "forged_statistics" => "Forged",
            "kind" => "Kind",
            "foreign_page" | "stale_plan" => "Recheck",
            "evidence" => "MovedBound",
            other => return Err(format!("unclassified mutation family {other}").into()),
        };
        let counts = mechanisms.entry(mechanism).or_default();
        let mutants = checker.field(row, "mutants")?.parse::<usize>()?;
        let rejected = checker.field(row, "checker")? == "rejected";
        match checker.field(row, "outcome")?.as_str() {
            "wrong" => counts[0] += mutants,
            "error" => counts[1] += mutants,
            "correct" if rejected => counts[2] += mutants,
            _ => {}
        }
    }
    let mut totals = [0_usize; 2];
    for mechanism in ["Forged", "Kind", "Recheck", "MovedBound"] {
        let [wrong, error, harmless] = mechanisms.get(mechanism).copied().unwrap_or_default();
        totals[0] += wrong;
        totals[1] += error;
        for (name, value) in [
            ("Harmful", wrong + error),
            ("Wrong", wrong),
            ("Error", error),
            ("HarmlessRejected", harmless),
        ] {
            define(
                out,
                &format!("WitChecker{mechanism}{name}"),
                &group_digits(&value.to_string()),
            );
        }
    }
    define(
        out,
        "WitCheckerHarmfulWrong",
        &group_digits(&totals[0].to_string()),
    );
    define(
        out,
        "WitCheckerHarmfulError",
        &group_digits(&totals[1].to_string()),
    );
    Ok(())
}

/// The cold audit per column: the time to audit derived bounds over the
/// time to recompute the zone map, on one thread and on the most threads.
fn cold_audit_ratio_claims(out: &mut ClaimManifest) -> Result<(), Box<dyn Error>> {
    let audits = read_csv(&format!("{COLD}/audit.csv"))?;
    let mut times = BTreeMap::<(String, String), BTreeMap<String, f64>>::new();
    for row in &audits {
        times
            .entry((
                audits.field(row, "threads")?,
                audits.field(row, "selector")?,
            ))
            .or_default()
            .insert(
                audits.field(row, "summary")?,
                audits.field(row, "ns")?.parse()?,
            );
    }
    let most = times
        .keys()
        .map(|(threads, _)| threads.parse::<usize>())
        .collect::<Result<BTreeSet<_>, _>>()?
        .into_iter()
        .next_back()
        .filter(|threads| *threads != 1)
        .ok_or("cold audit ran on one thread only")?
        .to_string();
    for (threads, suffix) in [("1".to_string(), ""), (most, "Threaded")] {
        let ratios = times
            .iter()
            .filter(|((count, _), _)| *count == threads)
            .map(|(_, summaries)| {
                Ok(summaries
                    .get("derived_bounds")
                    .zip(summaries.get("zone_map"))
                    .map(|(bounds, zone_map)| bounds / zone_map)
                    .ok_or("cold audit lacks a summary for a column")?)
            })
            .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
        for (name, value) in [
            ("Min", ratios.iter().copied().fold(f64::MAX, f64::min)),
            ("Median", median(&ratios)),
            ("Max", ratios.iter().copied().fold(f64::MIN, f64::max)),
        ] {
            define(
                out,
                &format!("WitColdAuditBoundsOverZone{name}{suffix}"),
                &format!("{value:.2}"),
            );
        }
        define(
            out,
            &format!("WitColdAuditBoundsSlower{suffix}"),
            &ratios
                .iter()
                .filter(|ratio| **ratio > 1.0)
                .count()
                .to_string(),
        );
    }
    Ok(())
}

/// Facts over DuckDB's native engine by how long Witness takes (under
/// 0.2 ms, 0.2 to 1 ms, over 1 ms), and over DuckDB's time less the
/// measured cost of a client round trip that does no work.
fn duck_strata_claims(out: &mut ClaimManifest) -> Result<(), Box<dyn Error>> {
    let latency = paged_latency_with_duckdb(PAGED, DUCKDB)?;
    let freeze = read_csv(&format!("{DUCKDB}/freeze.csv"))?;
    let floor = freeze
        .field(&freeze[0], "client_floor_ns")?
        .parse::<f64>()?;
    let mut strata = BTreeMap::<&str, Vec<f64>>::new();
    let mut floorless = Vec::new();
    for arms in latency.values() {
        let (Some(witness), Some(duck)) = (arms.get("witness_direct_facts"), arms.get("duckdb"))
        else {
            continue;
        };
        let stratum = if *witness < 0.2e6 {
            "Short"
        } else if *witness <= 1e6 {
            "Middle"
        } else {
            "Long"
        };
        strata.entry(stratum).or_default().push(witness / duck);
        if *duck <= floor {
            return Err("a DuckDB query took no longer than its client floor".into());
        }
        floorless.push(witness / (duck - floor));
    }
    for stratum in ["Short", "Middle", "Long"] {
        let ratios = strata.get(stratum).cloned().unwrap_or_default();
        define(
            out,
            &format!("WitDuckNative{stratum}Queries"),
            &group_digits(&ratios.len().to_string()),
        );
        if !ratios.is_empty() {
            define(
                out,
                &format!("WitDuckNative{stratum}Median"),
                &format!("{:.2}", median(&ratios)),
            );
        }
    }
    define(
        out,
        "WitDuckNativeFloorlessMedian",
        &format!("{:.2}", median(&floorless)),
    );
    Ok(())
}

/// Bytes stored over all paged pairs, by each Witness layout over Zstd
/// Parquet.
fn byte_total_claims(out: &mut ClaimManifest) -> Result<(), Box<dyn Error>> {
    let pairs = read_csv(&format!("{PAGED}/pairs.csv"))?;
    let total = |name: &str| {
        pairs
            .iter()
            .map(|row| Ok(pairs.field(row, name)?.parse::<f64>()?))
            .sum::<Result<f64, Box<dyn Error>>>()
    };
    let zstd = total("parquet_zstd_bytes")?;
    define(
        out,
        "WitPagedDirectOverZstdBytesTotal",
        &format!("{:.2}", total("witness_direct_bytes")? / zstd),
    );
    define(
        out,
        "WitPagedCompactOverZstdBytesTotal",
        &format!("{:.2}", total("witness_compact_bytes")? / zstd),
    );
    Ok(())
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
