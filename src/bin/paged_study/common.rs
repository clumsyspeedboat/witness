//! Page menus and the timing harness shared by the paged studies.

use std::fs::File;
use std::hint::black_box;
use std::io::BufWriter;
use std::time::Instant;

use witness::access_compiler::Recipe;

/// Rows per page, for Witness pages and Parquet data pages alike, unless
/// `WITNESS_PAGE_ROWS` sets another size for a sensitivity run.
pub const PAGE_ROWS: usize = 16_384;
const REPEATS: usize = 5;
const TARGET_NS: u128 = 2_000_000;

/// Encodings whose rows a reader reaches directly: by offset, through a
/// dictionary, or through a run index. No delta chain, no frame.
pub fn direct_menu() -> Vec<Recipe> {
    let bitpack = || Box::new(Recipe::BitPack);
    vec![
        Recipe::For(bitpack()),
        Recipe::Dictionary(bitpack()),
        Recipe::Dictionary(Box::new(Recipe::Rle {
            index_interval: 32,
            values: bitpack(),
        })),
        Recipe::Rle {
            index_interval: 32,
            values: Box::new(Recipe::For(bitpack())),
        },
    ]
}

/// The direct menu plus delta chains, each optionally in one Zstd frame per
/// page: the smallest pages the codec set can produce.
pub fn compact_menu() -> Vec<Recipe> {
    let bitpack = || Box::new(Recipe::BitPack);
    let mut menu = direct_menu();
    menu.push(Recipe::Delta {
        restart_interval: 256,
        deltas: bitpack(),
    });
    menu.push(Recipe::UnsignedDelta {
        restart_interval: 256,
        deltas: bitpack(),
    });
    let framed = menu
        .iter()
        .map(|recipe| Recipe::Frame(Box::new(recipe.clone())))
        .collect::<Vec<_>>();
    menu.extend(framed);
    menu
}

/// p25, median, and p75 of five calibrated repetitions, each long enough to
/// take at least `TARGET_NS`.
pub fn benchmark<T, F>(mut run: F) -> Result<(f64, f64, f64), String>
where
    F: FnMut() -> Result<T, String>,
{
    black_box(run()?);
    let started = Instant::now();
    black_box(run()?);
    let probe = started.elapsed().as_nanos().max(1);
    let iterations = usize::try_from((TARGET_NS / probe).clamp(1, 1_000)).unwrap();
    let mut samples = Vec::with_capacity(REPEATS);
    for _ in 0..REPEATS {
        let started = Instant::now();
        for _ in 0..iterations {
            black_box(run()?);
        }
        samples.push(started.elapsed().as_nanos() as f64 / iterations as f64);
    }
    samples.sort_by(f64::total_cmp);
    Ok((samples[1], samples[2], samples[3]))
}

/// The page size of this run: `WITNESS_PAGE_ROWS`, or `PAGE_ROWS`.
pub fn page_rows() -> Result<usize, String> {
    match std::env::var("WITNESS_PAGE_ROWS") {
        Ok(rows) => match rows.parse::<usize>() {
            Ok(rows) if rows > 0 => Ok(rows),
            _ => Err(format!(
                "WITNESS_PAGE_ROWS must be a positive integer, not {rows:?}"
            )),
        },
        Err(_) => Ok(PAGE_ROWS),
    }
}

pub fn csv(path: String) -> std::io::Result<BufWriter<File>> {
    Ok(BufWriter::new(File::create(path)?))
}

pub fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

/// Exact SUM and matching row count.
pub fn truth(
    selector: &[Option<i64>],
    value: &[Option<i64>],
    low: i64,
    high: i64,
) -> (i128, usize) {
    let mut sum = 0_i128;
    let mut matched = 0;
    for (selected, value) in selector.iter().zip(value) {
        if selected.is_some_and(|selected| low <= selected && selected <= high) {
            matched += 1;
            sum += value.map_or(0, i128::from);
        }
    }
    (sum, matched)
}
