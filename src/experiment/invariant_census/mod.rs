mod candidates;
mod model;
mod report;
mod statistics;

use std::error::Error;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use self::candidates::analyze_candidates;
use self::model::{CandidateRow, ColumnRow, PageRow};
use self::statistics::{analyze_pages, analyze_values};
use super::datasets::invariant_census_columns;
use super::types::Column;

type ColumnAnalysis = (ColumnRow, Vec<PageRow>, Vec<CandidateRow>);

pub fn run(result_dir: &str, max_rows: usize) -> Result<(), Box<dyn Error>> {
    let columns = invariant_census_columns(max_rows)?;
    if columns.len() < 100 {
        return Err(format!(
            "invariant census is unexpectedly small: {} columns",
            columns.len()
        )
        .into());
    }
    // Columns are independent, so they are analyzed on every core and then
    // reassembled in id order; the output is identical to a serial run.
    let next = AtomicUsize::new(0);
    let slots = Mutex::new((0..columns.len()).map(|_| None).collect::<Vec<_>>());
    let threads = std::thread::available_parallelism().map_or(1, usize::from);
    std::thread::scope(|scope| {
        for _ in 0..threads.min(columns.len()) {
            scope.spawn(|| {
                loop {
                    let id = next.fetch_add(1, Ordering::Relaxed);
                    let Some(column) = columns.get(id) else {
                        break;
                    };
                    let analysis = analyze_column(id, column).map_err(|error| {
                        format!(
                            "{}/{}/{}: {error}",
                            column.group, column.source, column.name
                        )
                    });
                    slots.lock().unwrap()[id] = Some(analysis);
                }
            });
        }
    });
    let mut column_rows = Vec::with_capacity(columns.len());
    let mut page_rows = Vec::new();
    let mut candidate_rows = Vec::new();
    for slot in slots.into_inner().unwrap() {
        let (column, pages, candidates) = slot.ok_or("census column was not analyzed")??;
        column_rows.push(column);
        page_rows.extend(pages);
        candidate_rows.extend(candidates);
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

fn analyze_column(id: usize, column: &Column) -> Result<ColumnAnalysis, String> {
    {
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
        let pages = analyze_pages(id, &column.values);
        let row = ColumnRow {
            id,
            group: column.group.clone(),
            source: column.source.clone(),
            name: column.name.clone(),
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
        };
        Ok((row, pages, candidates))
    }
}
