//! Fresh-process query of self-contained native files.

use std::time::Instant;

use witness::access_compiler::native_file;
use witness::access_compiler::paged::{self, Planning};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let regions = arguments.first().is_some_and(|arg| arg == "--regions");
    if regions {
        arguments.remove(0);
    }
    if arguments.len() != 4 {
        return Err("usage: native_query [--regions] SELECTOR VALUE LOW HIGH".into());
    }
    let low: i64 = arguments[2].parse()?;
    let high: i64 = arguments[3].parse()?;
    let clock = Instant::now();
    let mut selector = native_file::open(&arguments[0])?;
    let value = native_file::open(&arguments[1])?;
    if regions {
        selector.column.enable_ordered_regions();
    }
    let open_ns = clock.elapsed().as_nanos();
    let clock = Instant::now();
    let (sum, matched, counts) = paged::sum_where(
        &selector.column,
        &value.column,
        low,
        high,
        Planning::Facts,
        1,
    )?;
    let query_ns = clock.elapsed().as_nanos();
    println!(
        "{{\"sum\":{sum},\"matched\":{matched},\"rows\":{},\"pages\":{},\"skipped\":{},\"regions\":{},\"file_bytes\":{},\"descriptor_bytes\":{},\"open_ns\":{open_ns},\"query_ns\":{query_ns}}}",
        selector.column.rows(),
        selector.column.pages().len(),
        counts.skipped,
        selector.column.ordered_regions().len(),
        selector.file_bytes + value.file_bytes,
        selector.descriptor_bytes + value.descriptor_bytes
    );
    Ok(())
}
