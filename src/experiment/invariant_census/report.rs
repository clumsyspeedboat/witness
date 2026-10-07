use std::collections::BTreeMap;
use std::error::Error;
use std::fs;

use super::model::{CandidateRow, ColumnRow, PageRow, SourceRow};
use super::statistics::{DISPLACEMENT_THRESHOLDS, median};

pub fn write_all(
    result_dir: &str,
    columns: &[ColumnRow],
    pages: &[PageRow],
    candidates: &[CandidateRow],
) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(result_dir)?;
    write_columns(result_dir, columns)?;
    write_pages(result_dir, pages)?;
    write_candidates(result_dir, candidates)?;
    let sources = source_rows(columns, pages);
    write_sources(result_dir, &sources)?;
    write_summary(result_dir, columns, pages, candidates, &sources)?;
    Ok(())
}

fn write_columns(result_dir: &str, rows: &[ColumnRow]) -> Result<(), Box<dyn Error>> {
    let mut writer = csv::Writer::from_path(format!("{result_dir}/columns.csv"))?;
    writer.write_record([
        "column",
        "group",
        "source",
        "name",
        "rows",
        "nulls",
        "unique_non_null",
        "global_monotone_non_null",
        "null_placement",
        "distinct_non_null",
        "max_rank_displacement",
        "monotone_segment_rows_ge_128",
        "monotone_segments_ge_128",
        "smallest_candidate_bytes",
        "smallest_access_ready_bytes",
        "structural_monotone_bytes",
        "structural_monotone_premium",
        "structural_monotone_access_ready_bytes",
        "checked_monotone_access_ready_bytes",
        "structural_piecewise_access_ready_bytes",
        "order_mapping_access_ready_bytes",
        "order_mapping_access_ready_premium",
        "smallest_chunked_bytes",
    ])?;
    for row in rows {
        writer.write_record([
            row.id.to_string(),
            row.group.clone(),
            row.source.clone(),
            row.name.clone(),
            row.rows.to_string(),
            row.nulls.to_string(),
            row.unique_non_null.to_string(),
            row.global_monotone_non_null.to_string(),
            row.null_placement.into(),
            row.distinct_non_null.to_string(),
            row.max_rank_displacement.to_string(),
            row.monotone_segment_rows.to_string(),
            row.monotone_segments.to_string(),
            row.smallest_candidate_bytes.to_string(),
            row.smallest_access_ready_bytes.to_string(),
            row.structural_monotone_bytes
                .map(|value| value.to_string())
                .unwrap_or_default(),
            row.structural_monotone_premium
                .map(|value| format!("{value:.6}"))
                .unwrap_or_default(),
            row.structural_monotone_access_ready_bytes
                .map(|value| value.to_string())
                .unwrap_or_default(),
            row.checked_monotone_access_ready_bytes
                .map(|value| value.to_string())
                .unwrap_or_default(),
            row.structural_piecewise_access_ready_bytes
                .map(|value| value.to_string())
                .unwrap_or_default(),
            row.order_mapping_access_ready_bytes
                .map(|value| value.to_string())
                .unwrap_or_default(),
            row.order_mapping_access_ready_premium
                .map(|value| format!("{value:.6}"))
                .unwrap_or_default(),
            row.smallest_chunked_bytes
                .map(|value| value.to_string())
                .unwrap_or_default(),
        ])?;
    }
    writer.flush()?;
    Ok(())
}

fn write_pages(result_dir: &str, rows: &[PageRow]) -> Result<(), Box<dyn Error>> {
    let mut writer = csv::Writer::from_path(format!("{result_dir}/pages.csv"))?;
    writer.write_record([
        "column",
        "page_size",
        "page",
        "rows",
        "nulls",
        "monotone_non_null",
        "distinct_non_null",
        "max_rank_displacement",
        "unique_non_null",
    ])?;
    for row in rows {
        writer.write_record([
            row.column.to_string(),
            row.page_size.to_string(),
            row.page.to_string(),
            row.rows.to_string(),
            row.nulls.to_string(),
            row.monotone_non_null.to_string(),
            row.distinct_non_null.to_string(),
            row.max_rank_displacement.to_string(),
            row.unique_non_null.to_string(),
        ])?;
    }
    writer.flush()?;
    Ok(())
}

fn write_candidates(result_dir: &str, rows: &[CandidateRow]) -> Result<(), Box<dyn Error>> {
    let mut writer = csv::Writer::from_path(format!("{result_dir}/candidates.csv"))?;
    writer.write_record([
        "column",
        "recipe",
        "bytes",
        "structural_monotone",
        "structural_piecewise_monotone",
        "checked_monotone",
        "order_preserving_mapping",
        "framed",
        "restart_bound",
        "semantic_facts",
        "mapping_facts",
        "access_facts",
        "access_ready",
        "chunked",
    ])?;
    for row in rows {
        writer.write_record([
            row.column.to_string(),
            row.recipe.clone(),
            row.bytes.to_string(),
            row.structural_monotone.to_string(),
            row.structural_piecewise_monotone.to_string(),
            row.checked_monotone.to_string(),
            row.order_preserving_mapping.to_string(),
            row.framed.to_string(),
            row.restart_bound
                .map(|value| value.to_string())
                .unwrap_or_default(),
            row.semantic_facts.to_string(),
            row.mapping_facts.to_string(),
            row.access_facts.to_string(),
            row.access_ready.to_string(),
            row.chunked.to_string(),
        ])?;
    }
    writer.flush()?;
    Ok(())
}

fn write_sources(result_dir: &str, rows: &[SourceRow]) -> Result<(), Box<dyn Error>> {
    let mut writer = csv::Writer::from_path(format!("{result_dir}/sources.csv"))?;
    writer.write_record([
        "group",
        "source",
        "columns",
        "rows",
        "global_monotone_columns",
        "page_1024_total",
        "page_1024_monotone",
        "page_16384_total",
        "page_16384_monotone",
        "structural_monotone_columns",
    ])?;
    for row in rows {
        writer.write_record([
            row.group.clone(),
            row.source.clone(),
            row.columns.to_string(),
            row.rows.to_string(),
            row.global_monotone_columns.to_string(),
            row.page_1024_total.to_string(),
            row.page_1024_monotone.to_string(),
            row.page_16384_total.to_string(),
            row.page_16384_monotone.to_string(),
            row.structural_monotone_columns.to_string(),
        ])?;
    }
    writer.flush()?;
    Ok(())
}

fn write_summary(
    result_dir: &str,
    columns: &[ColumnRow],
    pages: &[PageRow],
    candidates: &[CandidateRow],
    sources: &[SourceRow],
) -> Result<(), Box<dyn Error>> {
    let mut metrics = Vec::<(String, String)>::new();
    metrics.push(("columns".into(), columns.len().to_string()));
    metrics.push(("sources".into(), sources.len().to_string()));
    metrics.push((
        "rows".into(),
        columns
            .iter()
            .map(|row| row.rows)
            .sum::<usize>()
            .to_string(),
    ));
    let monotone = columns
        .iter()
        .filter(|row| row.global_monotone_non_null)
        .count();
    metrics.push(("global_monotone_columns".into(), monotone.to_string()));
    metrics.push((
        "global_monotone_column_fraction".into(),
        ratio(monotone, columns.len()),
    ));
    metrics.push((
        "global_monotone_source_weighted_fraction".into(),
        format!(
            "{:.6}",
            sources
                .iter()
                .map(|row| row.global_monotone_columns as f64 / row.columns as f64)
                .sum::<f64>()
                / sources.len() as f64
        ),
    ));
    // A constant column is trivially non-decreasing; order evidence that
    // matters is reported over the columns that are not constant.
    let constant = |row: &&ColumnRow| row.unique_non_null == 1;
    let constant_columns = columns.iter().filter(constant).count();
    metrics.push(("constant_columns".into(), constant_columns.to_string()));
    metrics.push((
        "constant_column_fraction".into(),
        ratio(constant_columns, columns.len()),
    ));
    let nonconstant_monotone = columns
        .iter()
        .filter(|row| row.global_monotone_non_null && row.unique_non_null > 1)
        .count();
    metrics.push((
        "nonconstant_monotone_columns".into(),
        nonconstant_monotone.to_string(),
    ));
    metrics.push((
        "nonconstant_monotone_column_fraction".into(),
        ratio(nonconstant_monotone, columns.len()),
    ));
    let mut by_source = BTreeMap::<(&str, &str), (usize, usize)>::new();
    for row in columns {
        let entry = by_source
            .entry((row.group.as_str(), row.source.as_str()))
            .or_default();
        entry.0 += 1;
        entry.1 += usize::from(row.global_monotone_non_null && row.unique_non_null > 1);
    }
    metrics.push((
        "nonconstant_monotone_source_weighted_fraction".into(),
        format!(
            "{:.6}",
            by_source
                .values()
                .map(|(columns, monotone)| *monotone as f64 / *columns as f64)
                .sum::<f64>()
                / by_source.len() as f64
        ),
    ));
    for (name, select) in [
        (
            "access_ready_premium_median",
            (|row: &ColumnRow| Some(row.smallest_access_ready_bytes))
                as fn(&ColumnRow) -> Option<usize>,
        ),
        ("chunked_premium_median", |row: &ColumnRow| {
            row.smallest_chunked_bytes
        }),
    ] {
        metrics.push((
            name.into(),
            format!(
                "{:.6}",
                median(
                    columns
                        .iter()
                        .filter_map(|row| select(row)
                            .map(|bytes| bytes as f64 / row.smallest_candidate_bytes as f64))
                        .collect()
                )
            ),
        ));
    }
    let total_rows = columns
        .iter()
        .map(|row| row.rows - row.nulls)
        .sum::<usize>();
    let monotone_rows = columns
        .iter()
        .filter(|row| row.global_monotone_non_null)
        .map(|row| row.rows - row.nulls)
        .sum::<usize>();
    metrics.push((
        "global_monotone_row_fraction".into(),
        ratio(monotone_rows, total_rows),
    ));
    for threshold in DISPLACEMENT_THRESHOLDS {
        let count = columns
            .iter()
            .filter(|row| row.max_rank_displacement <= threshold)
            .count();
        metrics.push((
            format!("global_k_displaced_le_{threshold}_fraction"),
            ratio(count, columns.len()),
        ));
    }
    for page_size in [1_024, 16_384] {
        let admitted = pages
            .iter()
            .filter(|row| row.page_size == page_size)
            .collect::<Vec<_>>();
        let monotone = admitted.iter().filter(|row| row.monotone_non_null).count();
        metrics.push((
            format!("page_{page_size}_monotone_fraction"),
            ratio(monotone, admitted.len()),
        ));
        let constant_pages = admitted
            .iter()
            .filter(|row| row.unique_non_null == 1)
            .count();
        metrics.push((
            format!("page_{page_size}_constant_fraction"),
            ratio(constant_pages, admitted.len()),
        ));
        let nonconstant_monotone = admitted
            .iter()
            .filter(|row| row.monotone_non_null && row.unique_non_null > 1)
            .count();
        metrics.push((
            format!("page_{page_size}_nonconstant_monotone_fraction"),
            ratio(nonconstant_monotone, admitted.len()),
        ));
        for threshold in DISPLACEMENT_THRESHOLDS {
            let count = admitted
                .iter()
                .filter(|row| row.max_rank_displacement <= threshold)
                .count();
            metrics.push((
                format!("page_{page_size}_k_displaced_le_{threshold}_fraction"),
                ratio(count, admitted.len()),
            ));
        }
    }
    let segment_rows = columns
        .iter()
        .map(|row| row.monotone_segment_rows)
        .sum::<usize>();
    metrics.push((
        "monotone_segment_rows_ge_128_fraction".into(),
        ratio(segment_rows, total_rows),
    ));
    let structural = columns
        .iter()
        .filter(|row| row.structural_monotone_bytes.is_some())
        .count();
    metrics.push((
        "structural_monotone_candidate_columns".into(),
        structural.to_string(),
    ));
    metrics.push((
        "structural_monotone_candidate_fraction".into(),
        ratio(structural, columns.len()),
    ));
    metrics.push((
        "structural_monotone_premium_median".into(),
        format!(
            "{:.6}",
            median(
                columns
                    .iter()
                    .filter_map(|row| row.structural_monotone_premium)
                    .collect()
            )
        ),
    ));
    for (name, count) in [
        (
            "structural_monotone_access_ready_columns",
            columns
                .iter()
                .filter(|row| row.structural_monotone_access_ready_bytes.is_some())
                .count(),
        ),
        (
            "checked_monotone_access_ready_columns",
            columns
                .iter()
                .filter(|row| row.checked_monotone_access_ready_bytes.is_some())
                .count(),
        ),
        (
            "structural_piecewise_access_ready_columns",
            columns
                .iter()
                .filter(|row| row.structural_piecewise_access_ready_bytes.is_some())
                .count(),
        ),
        (
            "order_mapping_access_ready_columns",
            columns
                .iter()
                .filter(|row| row.order_mapping_access_ready_bytes.is_some())
                .count(),
        ),
    ] {
        metrics.push((name.into(), count.to_string()));
        metrics.push((format!("{name}_fraction"), ratio(count, columns.len())));
    }
    metrics.push((
        "order_mapping_access_ready_premium_median".into(),
        format!(
            "{:.6}",
            median(
                columns
                    .iter()
                    .filter_map(|row| row.order_mapping_access_ready_premium)
                    .collect()
            )
        ),
    ));
    let small_domain = columns
        .iter()
        .filter(|row| {
            let non_null = row.rows - row.nulls;
            row.unique_non_null <= 256 || row.unique_non_null.saturating_mul(8) <= non_null
        })
        .count();
    metrics.push(("small_domain_columns".into(), small_domain.to_string()));
    metrics.push((
        "small_domain_column_fraction".into(),
        ratio(small_domain, columns.len()),
    ));
    metrics.push(("candidate_rows".into(), candidates.len().to_string()));
    let mut writer = csv::Writer::from_path(format!("{result_dir}/summary.csv"))?;
    writer.write_record(["metric", "value"])?;
    for (metric, value) in metrics {
        writer.write_record([metric, value])?;
    }
    writer.flush()?;
    Ok(())
}

fn source_rows(columns: &[ColumnRow], pages: &[PageRow]) -> Vec<SourceRow> {
    let mut output = BTreeMap::<(&str, &str), SourceRow>::new();
    for column in columns {
        let row = output
            .entry((&column.group, &column.source))
            .or_insert_with(|| SourceRow {
                group: column.group.clone(),
                source: column.source.clone(),
                columns: 0,
                rows: 0,
                global_monotone_columns: 0,
                page_1024_total: 0,
                page_1024_monotone: 0,
                page_16384_total: 0,
                page_16384_monotone: 0,
                structural_monotone_columns: 0,
            });
        row.columns += 1;
        row.rows += column.rows;
        row.global_monotone_columns += usize::from(column.global_monotone_non_null);
        row.structural_monotone_columns += usize::from(column.structural_monotone_bytes.is_some());
    }
    for page in pages {
        let column = &columns[page.column];
        let row = output.get_mut(&(&column.group, &column.source)).unwrap();
        match page.page_size {
            1_024 => {
                row.page_1024_total += 1;
                row.page_1024_monotone += usize::from(page.monotone_non_null);
            }
            16_384 => {
                row.page_16384_total += 1;
                row.page_16384_monotone += usize::from(page.monotone_non_null);
            }
            _ => unreachable!(),
        }
    }
    output.into_values().collect()
}

fn ratio(numerator: usize, denominator: usize) -> String {
    if denominator == 0 {
        "0.000000".into()
    } else {
        format!("{:.6}", numerator as f64 / denominator as f64)
    }
}
