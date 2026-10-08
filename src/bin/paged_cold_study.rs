//! Selective SUM over paged columns read cold from files: TPC-H SF10 lineitem,
//! each column written as one paged file, every query run after its files are
//! evicted from the page cache.
//!
//! Four ways to plan the same pages are compared on one executor:
//! - `scan`: read and scan every page;
//! - `zonemap_footer`: read the footer's exact per-page minima and maxima in
//!   one read, then plan from them;
//! - `facts_footer`: read the footer's derived bounds in one read, then plan
//!   from them as from a zone map, re-deriving the bound of every page the
//!   query reads from the bytes it read, so only unread pages rely on it;
//! - `facts_pages`: plan every page from the facts its own fields prove, which
//!   reads those fields from each page.
//!
//! Separately, the study audits each column's derived-bounds footer: it
//! re-derives every page's bound from the page's fields, cold, and compares.

#[allow(dead_code)]
#[path = "predicate_pipeline/workload.rs"]
mod workload;

#[allow(dead_code)]
#[path = "paged_study/common.rs"]
mod common;

use std::error::Error;
use std::fs::{self, File};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::time::Instant;

use witness::access_compiler::paged::{
    self, FileStore, PageCounts, PagePolicy, PageStore, PagedColumn, Planning, sum_where_in,
};
use witness::access_compiler::paged_file::{self, PagedFileLayout, Summary};
use witness::access_compiler::{Span, primitive_rule_fingerprint, vector};
use witness::experiment::datasets::scale_columns;

use common::{PAGE_ROWS, csv, direct_menu, quote, truth};
use workload::predicates;

const DEFAULT_RESULT_DIR: &str = "experiments/results/paged_cold";
const VALUE_COLUMN: &str = "extendedprice";
const REPEATS: usize = 3;
/// Largest share of a file's pages allowed to stay resident after eviction.
const RESIDENT_LIMIT: f64 = 0.01;

#[derive(Clone, Copy)]
enum Arm {
    Scan,
    ZoneMapFooter,
    FactsFooter,
    FactsPages,
}

impl Arm {
    const ALL: [Arm; 4] = [
        Arm::Scan,
        Arm::ZoneMapFooter,
        Arm::FactsFooter,
        Arm::FactsPages,
    ];

    fn name(self) -> &'static str {
        match self {
            Arm::Scan => "scan",
            Arm::ZoneMapFooter => "zonemap_footer",
            Arm::FactsFooter => "facts_footer",
            Arm::FactsPages => "facts_pages",
        }
    }
}

/// A column written to a paged file, with a copy whose statistics are its
/// derived bounds, for planning from the bounds footer.
struct Stored {
    column: PagedColumn,
    bounded: PagedColumn,
    path: std::path::PathBuf,
}

fn main() -> Result<(), Box<dyn Error>> {
    let result_dir = std::env::var("WITNESS_PAGED_COLD_RESULT_DIR")
        .unwrap_or_else(|_| DEFAULT_RESULT_DIR.into());
    let file_dir = format!("{result_dir}/files");
    fs::create_dir_all(&file_dir)?;
    let columns = scale_columns()?;
    let value_index = columns
        .iter()
        .position(|column| column.name.contains(VALUE_COLUMN))
        .ok_or("scale corpus lacks the value column; run fetch_scale_corpus.py")?;
    let policy = PagePolicy::smallest(direct_menu());
    let stored = std::thread::scope(|scope| {
        columns
            .iter()
            .map(|column| {
                let (policy, file_dir) = (&policy, &file_dir);
                scope.spawn(move || store(column, policy, file_dir))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().expect("writing a paged file panicked"))
            .collect::<Result<Vec<_>, String>>()
    })?;
    eprintln!("wrote {} paged files", stored.len());

    let mut queries = csv(format!("{result_dir}/queries.csv"))?;
    writeln!(
        queries,
        "selector,predicate,arm,repeat,ns,io_operations,io_bytes,footer_bytes,matched_rows,pages,skipped,resident_pages"
    )?;
    let mut audits = csv(format!("{result_dir}/audit.csv"))?;
    writeln!(
        audits,
        "selector,summary,pages,bounded_pages,ns,io_operations,io_bytes,footer_bytes"
    )?;
    let value = &stored[value_index];
    for (index, (column, selector)) in columns.iter().zip(&stored).enumerate() {
        if index == value_index {
            continue;
        }
        audit(&mut audits, &column.name, selector, value)?;
        for predicate in predicates(&column.values)? {
            let expected = truth(
                &column.values,
                &columns[value_index].values,
                predicate.low,
                predicate.high,
            );
            for repeat in 0..REPEATS {
                for arm in Arm::ALL {
                    let resident = evict(&[&selector.path, &value.path])?;
                    let started = Instant::now();
                    let (sum, matched, counts, footer_bytes) =
                        cold_query(arm, selector, value, predicate.low, predicate.high)?;
                    let nanos = started.elapsed().as_nanos();
                    if (sum, matched) != expected {
                        return Err(format!(
                            "{} returned {:?} on {} {}, expected {expected:?}",
                            arm.name(),
                            (sum, matched),
                            column.name,
                            predicate.label
                        )
                        .into());
                    }
                    writeln!(
                        queries,
                        "{},{},{},{repeat},{nanos},{},{},{footer_bytes},{matched},{},{},{resident}",
                        quote(&column.name),
                        predicate.label,
                        arm.name(),
                        counts.io_operations,
                        counts.io_bytes,
                        selector.column.pages().len(),
                        counts.skipped,
                    )?;
                }
            }
        }
        eprintln!("measured {}", column.name);
    }
    queries.flush()?;
    audits.flush()?;
    fs::write(
        format!("{result_dir}/freeze.csv"),
        format!(
            "rule_fingerprint,page_rows,repeats,threads\n{:#018x},{PAGE_ROWS},{REPEATS},1\n",
            primitive_rule_fingerprint()
        ),
    )?;
    fs::remove_dir_all(&file_dir)?;
    Ok(())
}

/// Encodes a column, writes it as a paged file that is on disk before any
/// eviction, and builds its bounds-as-statistics copy.
fn store(
    column: &witness::experiment::types::Column,
    policy: &PagePolicy,
    dir: &str,
) -> Result<Stored, String> {
    let paged = PagedColumn::from_values(&column.values, PAGE_ROWS, policy)?;
    let path = Path::new(dir).join(format!("{}.wpf", column.name));
    paged_file::write(&paged, &path)?;
    // Dirty pages cannot be evicted; flush the file to disk first.
    File::open(&path)
        .and_then(|file| file.sync_all())
        .map_err(|error| error.to_string())?;
    let mut bounded = paged.clone();
    for (index, page) in paged.pages().iter().enumerate() {
        bounded.replace_statistics(index, paged::derived_bounds(page)?)?;
    }
    Ok(Stored {
        column: paged,
        bounded,
        path,
    })
}

/// One cold query: open both files, read a footer section if the arm plans
/// from one, then plan and execute through file stores on one thread.
fn cold_query(
    arm: Arm,
    selector: &Stored,
    value: &Stored,
    low: i64,
    high: i64,
) -> Result<(i128, usize, PageCounts, u64), String> {
    let (selector_file, value_file) = (open(&selector.path)?, open(&value.path)?);
    let selector_layout = paged_file::open(&selector_file)?;
    let value_layout = paged_file::open(&value_file)?;
    let mut footer_bytes = 0;
    let (planned, planning) = match arm {
        Arm::Scan => (&selector.column, Planning::ScanOnly),
        Arm::FactsPages => (&selector.column, Planning::Facts),
        Arm::ZoneMapFooter | Arm::FactsFooter => {
            let (summary, column) = if matches!(arm, Arm::ZoneMapFooter) {
                (Summary::ZoneMap, &selector.column)
            } else {
                (Summary::DerivedBounds, &selector.bounded)
            };
            let entries = paged_file::read_summary(&selector_file, &selector_layout, summary)?;
            footer_bytes = selector_layout.section_bytes(summary);
            // The in-memory copy plans from these same entries.
            if entries.iter().zip(column.pages()).any(|(entry, page)| {
                entry.interval != page.stats || entry.non_null != page.non_null
            }) {
                return Err("footer disagrees with the column it was written from".into());
            }
            // A footer of derived bounds is checked against every page the
            // query reads; a zone map can only be trusted.
            let planning = if matches!(arm, Arm::ZoneMapFooter) {
                Planning::StoredStatistics
            } else {
                Planning::DerivedFooter
            };
            (column, planning)
        }
    };
    let selector_store = file_store(&selector_file, &selector_layout);
    let value_store = file_store(&value_file, &value_layout);
    let (sum, matched, counts) = sum_where_in(
        (planned, &selector_store),
        (&value.column, &value_store),
        low,
        high,
        planning,
        1,
    )?;
    Ok((sum, matched, counts, footer_bytes))
}

/// What it costs any reader to audit a footer section against the pages,
/// cold. A derived bound is re-derived from the few fields it comes from,
/// read where they lie; an exact minimum and maximum can only be recomputed
/// by reading the page, in one read, and decoding it.
fn audit(
    out: &mut impl Write,
    name: &str,
    selector: &Stored,
    value: &Stored,
) -> Result<(), Box<dyn Error>> {
    for summary in [Summary::DerivedBounds, Summary::ZoneMap] {
        evict(&[&selector.path, &value.path])?;
        let started = Instant::now();
        let file = open(&selector.path)?;
        let layout = paged_file::open(&file)?;
        let footer = paged_file::read_summary(&file, &layout, summary)?;
        let store = file_store(&file, &layout);
        let mut counts = PageCounts::default();
        let mut bounded = 0;
        for (index, (page, entry)) in selector.column.pages().iter().zip(&footer).enumerate() {
            let (interval, metrics) = match summary {
                Summary::DerivedBounds => {
                    let mut session = store.session(page, index, paged::Access::Plan)?;
                    (
                        paged::derived_bounds_in(page, &mut session)?,
                        session.metrics(),
                    )
                }
                Summary::ZoneMap => {
                    // One read fetches the page; the bytes read are decoded.
                    let stored = page.column.page.bytes();
                    let mut bytes = vec![0_u8; stored.len()];
                    file.read_exact_at(&mut bytes, layout.offsets[index])?;
                    if bytes != stored {
                        return Err(format!("{name} page {index}: file bytes differ").into());
                    }
                    let mut session = witness::access_compiler::ReadSession::new_untracked(
                        &page.column.page,
                        witness::access_compiler::ClosureMode::Selective,
                    );
                    let range = exact_range(page, &mut session)?;
                    counts.io_operations += 1;
                    counts.io_bytes += bytes.len();
                    if range != entry.interval {
                        return Err(
                            format!("{name} page {index}: footer entry fails its audit").into()
                        );
                    }
                    bounded += usize::from(range.is_some());
                    continue;
                }
            };
            if interval != entry.interval {
                return Err(format!("{name} page {index}: footer entry fails its audit").into());
            }
            bounded += usize::from(interval.is_some());
            counts.io_operations += metrics.transfer_operations;
            counts.io_bytes += metrics.transferred_bytes;
        }
        let nanos = started.elapsed().as_nanos();
        writeln!(
            out,
            "{},{},{},{bounded},{nanos},{},{},{}",
            quote(name),
            match summary {
                Summary::DerivedBounds => "derived_bounds",
                Summary::ZoneMap => "zone_map",
            },
            selector.column.pages().len(),
            counts.io_operations,
            counts.io_bytes,
            layout.section_bytes(summary),
        )?;
    }
    Ok(())
}

/// The exact smallest and largest non-null value of a page, by decoding it.
fn exact_range(
    page: &paged::Page,
    session: &mut witness::access_compiler::ReadSession<'_>,
) -> Result<Option<(i128, i128)>, String> {
    let mut range: Option<(i128, i128)> = None;
    let mut widen = |value: i128| {
        range = Some(range.map_or((value, value), |(low, high)| {
            (low.min(value), high.max(value))
        }));
    };
    vector::scan(
        &page.column.decoder,
        session,
        Span::new(0, page.rows())?,
        &mut |_, _, chunk| {
            match chunk {
                vector::Chunk::Values(values) => values.iter().for_each(|&value| widen(value)),
                vector::Chunk::Run { value, .. } => widen(value),
                vector::Chunk::Nulls { .. } => {}
            }
            Ok(())
        },
    )?;
    Ok(range)
}

fn open(path: &Path) -> Result<File, String> {
    File::open(path).map_err(|error| format!("{}: {error}", path.display()))
}

fn file_store<'f>(file: &'f File, layout: &PagedFileLayout) -> FileStore<'f> {
    FileStore {
        file,
        offsets: layout.offsets.clone(),
    }
}

/// Drops the files from the page cache and returns how many of their 4 KiB
/// pages are still resident; fails if eviction left more than
/// `RESIDENT_LIMIT` of any file behind.
fn evict(paths: &[&Path]) -> Result<usize, String> {
    let mut resident = 0;
    for path in paths {
        let file = open(path)?;
        // SAFETY: posix_fadvise only reads the descriptor and scalar range.
        let status =
            unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
        if status != 0 {
            return Err(format!("posix_fadvise(DONTNEED) failed with {status}"));
        }
        let (still, total) = residency(&file)?;
        if still as f64 > RESIDENT_LIMIT * total as f64 {
            return Err(format!(
                "{}: {still} of {total} pages stayed resident after eviction",
                path.display()
            ));
        }
        resident += still;
    }
    Ok(resident)
}

/// Resident and total 4 KiB pages of a file, from mincore over a mapping.
fn residency(file: &File) -> Result<(usize, usize), String> {
    let length = file.metadata().map_err(|error| error.to_string())?.len() as usize;
    if length == 0 {
        return Ok((0, 0));
    }
    let pages = length.div_ceil(4096);
    // SAFETY: a read-only private mapping of the open file, unmapped below;
    // mincore writes exactly one byte per page into `vector`.
    unsafe {
        let mapping = libc::mmap(
            std::ptr::null_mut(),
            length,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            file.as_raw_fd(),
            0,
        );
        if mapping == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let mut vector = vec![0_u8; pages];
        let status = libc::mincore(mapping, length, vector.as_mut_ptr());
        libc::munmap(mapping, length);
        if status != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok((vector.iter().filter(|byte| **byte & 1 == 1).count(), pages))
    }
}
