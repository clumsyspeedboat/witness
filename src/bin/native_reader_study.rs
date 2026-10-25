//! Separate diagnostic for reader initialization and appended ordered batches.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::time::Instant;

use witness::access_compiler::native_file;
use witness::access_compiler::paged::{self, PagePolicy, PagedColumn, Planning};
use witness::access_compiler::primitive_rule_fingerprint;
use witness::experiment::datasets::invariant_census_columns;

#[allow(dead_code)]
#[path = "paged_study/common.rs"]
mod common;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory = std::env::var("WITNESS_NATIVE_RESULT_DIR")?;
    fs::create_dir_all(&directory)?;
    let mut freeze = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(format!("{directory}/freeze.csv"))?;
    writeln!(
        freeze,
        "key,value\nstatus,started\nrule_fingerprint,{:016x}\nreader_scope,resident_including_checksum_and_descriptor_parsing\nregion_scope,synthetic_four_appended_sorted_batches\npage_rows,1024\nrounds,9",
        primitive_rule_fingerprint()
    )?;
    freeze.flush()?;
    let policy = PagePolicy::smallest(common::direct_menu());
    let mut readers = common::csv(format!("{directory}/reader.csv"))?;
    writeln!(
        readers,
        "group,source,column,rows,pages,page_bytes,file_bytes,descriptor_bytes,open_ns,first_query_ns,sum,matched"
    )?;
    let mut by_group = BTreeMap::<String, BTreeMap<String, Vec<_>>>::new();
    for column in invariant_census_columns(65_536)? {
        if column.values.iter().any(Option::is_some) {
            by_group
                .entry(column.group.clone())
                .or_default()
                .entry(column.source.clone())
                .or_default()
                .push(column);
        }
    }
    let mut sampled = 0;
    for sources in by_group.values() {
        let sources = sources.values().collect::<Vec<_>>();
        let count = sources.len().min(3);
        for slot in 0..count {
            let columns = sources[slot * sources.len() / count];
            let column = &columns[columns.len() / 2];
            let writer = PagedColumn::from_values(&column.values, 1_024, &policy)?;
            let bytes = native_file::to_bytes(&writer)?;
            drop(writer);
            let clock = Instant::now();
            let loaded = native_file::from_bytes(&bytes)?;
            let open_ns = clock.elapsed().as_nanos();
            if loaded
                .column
                .pages()
                .iter()
                .any(|page| !page.column.truth.is_empty())
            {
                return Err("native reader retained an oracle".into());
            }
            let low = *column.values.iter().flatten().min().ok_or("empty column")?;
            let high = *column.values.iter().flatten().max().ok_or("empty column")?;
            let expected = common::truth(&column.values, &column.values, low, high);
            let clock = Instant::now();
            let (sum, matched, _) = paged::sum_where(
                &loaded.column,
                &loaded.column,
                low,
                high,
                Planning::Facts,
                1,
            )?;
            let query_ns = clock.elapsed().as_nanos();
            if (sum, matched) != expected {
                return Err("native reader answer mismatch".into());
            }
            writeln!(
                readers,
                "{},{},{},{},{},{},{},{},{open_ns},{query_ns},{sum},{matched}",
                common::quote(&column.group),
                common::quote(&column.source),
                common::quote(&column.name),
                column.values.len(),
                loaded.column.pages().len(),
                loaded.page_bytes,
                loaded.file_bytes,
                loaded.descriptor_bytes
            )?;
            sampled += 1;
        }
    }
    readers.flush()?;
    let mut regions = common::csv(format!("{directory}/regions.csv"))?;
    writeln!(
        regions,
        "pages,batches,requested,matched,arm,regions,candidate_pages,p25_ns,median_ns,p75_ns,sum"
    )?;
    for pages in [64, 512, 4_096] {
        let rows_per_batch = pages / 4 * 1_024;
        let values = (0..pages * 1_024)
            .map(|row| Some((row % rows_per_batch) as i64))
            .collect::<Vec<_>>();
        let writer = PagedColumn::from_values(&values, 1_024, &policy)?;
        let bytes = native_file::to_bytes(&writer)?;
        drop(writer);
        let original = native_file::from_bytes(&bytes)?.column;
        let mut indexed = native_file::from_bytes(&bytes)?.column;
        indexed.enable_ordered_regions();
        if indexed.ordered() || indexed.ordered_regions().len() != 4 {
            return Err("unexpected synthetic region certificate".into());
        }
        for per_thousand in [1, 10, 100, 500] {
            let width = (rows_per_batch * per_thousand / 1_000).max(1);
            let low = ((rows_per_batch - width) / 2) as i64;
            let high = low + width as i64 - 1;
            let expected = common::truth(&values, &values, low, high);
            let arms = [
                (&original, Planning::Facts),
                (&indexed, Planning::Facts),
                (&indexed, Planning::StoredStatisticsPlus),
            ];
            let mut runs = arms.map(|(column, planning)| {
                move || paged::sum_where(column, column, low, high, planning, 1)
            });
            for run in &mut runs {
                let result = run()?;
                if (result.0, result.1) != expected {
                    return Err("region answer mismatch".into());
                }
            }
            let timings = common::interleaved(&mut runs, 9)?;
            for (index, (p25, median, p75)) in timings.into_iter().enumerate() {
                let (column, _) = arms[index];
                let candidates: usize = column
                    .candidate_page_ranges(i128::from(low), i128::from(high))?
                    .iter()
                    .map(|range| range.len())
                    .sum();
                let arm = ["original", "regions", "zonemapplus_regions"][index];
                writeln!(
                    regions,
                    "{pages},4,{:.3},{},{arm},{},{candidates},{p25:.1},{median:.1},{p75:.1},{}",
                    per_thousand as f64 / 1_000.0,
                    expected.1,
                    column.ordered_regions().len(),
                    expected.0
                )?;
            }
        }
        regions.flush()?;
        eprintln!("region study: {pages} pages complete");
    }
    writeln!(freeze, "reader_columns,{sampled}\nstatus,complete")?;
    freeze.flush()?;
    eprintln!("native reader: {sampled} corpus columns and 36 region timing cells passed");
    Ok(())
}
