mod candidates;
mod model;
mod report;
mod statistics;

use std::error::Error;

use self::candidates::analyze_candidates;
use self::model::ColumnRow;
use self::statistics::{analyze_pages, analyze_values};
use super::datasets::invariant_census_columns;

pub fn run(result_dir: &str, max_rows: usize) -> Result<(), Box<dyn Error>> {
    let columns = invariant_census_columns(max_rows)?;
    if columns.len() < 100 {
        return Err(format!(
            "invariant census is unexpectedly small: {} columns",
            columns.len()
        )
        .into());
    }
    let mut column_rows = Vec::with_capacity(columns.len());
    let mut page_rows = Vec::new();
    let mut candidate_rows = Vec::new();
    for (id, column) in columns.into_iter().enumerate() {
        let statistics = analyze_values(&column.values);
        let candidates = analyze_candidates(id, &column.values)?;
        let smallest_candidate_bytes = candidates
            .iter()
            .map(|candidate| candidate.bytes)
            .min()
            .ok_or("column has no executable candidate")?;
        let smallest_access_ready_bytes = candidates
            .iter()
            .filter(|candidate| candidate.access_ready)
            .map(|candidate| candidate.bytes)
            .min()
            .ok_or("column has no access-ready candidate")?;
        let structural_monotone_bytes = candidates
            .iter()
            .filter(|candidate| candidate.structural_monotone)
            .map(|candidate| candidate.bytes)
            .min();
        let structural_monotone_premium = structural_monotone_bytes
            .map(|bytes| bytes as f64 / smallest_candidate_bytes as f64 - 1.0);
        let structural_monotone_access_ready_bytes = candidates
            .iter()
            .filter(|candidate| candidate.access_ready && candidate.structural_monotone)
            .map(|candidate| candidate.bytes)
            .min();
        let checked_monotone_access_ready_bytes = candidates
            .iter()
            .filter(|candidate| candidate.access_ready && candidate.checked_monotone)
            .map(|candidate| candidate.bytes)
            .min();
        let structural_piecewise_access_ready_bytes = candidates
            .iter()
            .filter(|candidate| candidate.access_ready && candidate.structural_piecewise_monotone)
            .map(|candidate| candidate.bytes)
            .min();
        let order_mapping_access_ready_bytes = candidates
            .iter()
            .filter(|candidate| candidate.access_ready && candidate.order_preserving_mapping)
            .map(|candidate| candidate.bytes)
            .min();
        let order_mapping_access_ready_premium = order_mapping_access_ready_bytes
            .map(|bytes| bytes as f64 / smallest_access_ready_bytes as f64 - 1.0);
        page_rows.extend(analyze_pages(id, &column.values));
        candidate_rows.extend(candidates);
        column_rows.push(ColumnRow {
            id,
            group: column.group,
            source: column.source,
            name: column.name,
            rows: column.values.len(),
            nulls: statistics.nulls,
            unique_non_null: statistics.unique_non_null,
            global_monotone_non_null: statistics.monotone_non_null,
            null_placement: statistics.null_placement,
            distinct_non_null: statistics.distinct_non_null,
            max_rank_displacement: statistics.max_rank_displacement,
            monotone_segment_rows: statistics.monotone_segment_rows,
            monotone_segments: statistics.monotone_segments,
            smallest_candidate_bytes,
            smallest_access_ready_bytes,
            structural_monotone_bytes,
            structural_monotone_premium,
            structural_monotone_access_ready_bytes,
            checked_monotone_access_ready_bytes,
            structural_piecewise_access_ready_bytes,
            order_mapping_access_ready_bytes,
            order_mapping_access_ready_premium,
        });
    }
    report::write_all(result_dir, &column_rows, &page_rows, &candidate_rows)?;
    println!(
        "invariant census wrote {} columns, {} pages, and {} encoding candidates",
        column_rows.len(),
        page_rows.len(),
        candidate_rows.len()
    );
    Ok(())
}
