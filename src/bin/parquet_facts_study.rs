//! Facts read from another format's encoding: Parquet DELTA_BINARY_PACKED
//! pages, read header by header without decoding a value or consulting the
//! statistics the writer stores.
//!
//! A delta page stores its first value, and each block of deltas stores its
//! smallest delta and one bit width per miniblock. Every step therefore lies
//! in `[min_delta, min_delta + 2^w - 1]`, which bounds every value of the page
//! from the headers alone, and a page whose smallest deltas are all
//! non-negative cannot decrease: the rules Witness applies to its own
//! frame-of-reference and unsigned-delta codecs. The study writes every
//! selector column of the paged study as delta-encoded Parquet with pages of
//! the same rows, derives these facts from the page headers, and counts the
//! pages they skip and accept next to exact page statistics.

#[allow(dead_code)]
#[path = "predicate_pipeline/workload.rs"]
mod workload;

#[allow(dead_code)]
#[path = "paged_study/common.rs"]
mod common;
#[allow(dead_code)]
#[path = "paged_study/corpus.rs"]
mod corpus;

use std::error::Error;
use std::fs;
use std::io::Write;
use std::sync::Arc;

use arrow_array::{Int64Array, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use bytes::Bytes;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Encoding, Type};
use parquet::column::page::Page;
use parquet::file::properties::{EnabledStatistics, WriterProperties, WriterVersion};
use parquet::file::reader::{FileReader, SerializedFileReader};

use common::{PAGE_ROWS, csv};
use workload::predicates;

const DEFAULT_RESULT_DIR: &str = "experiments/results/parquet_facts";

/// What the headers of one delta page prove about its non-null values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HeaderFacts {
    lower: i128,
    upper: i128,
    non_decreasing: bool,
    /// Bytes of the value section the derivation read: the page header and
    /// each block's minimum delta and bit widths, never a packed delta.
    header_bytes: usize,
    value_bytes: usize,
}

fn main() -> Result<(), Box<dyn Error>> {
    let result_dir = std::env::var("WITNESS_PARQUET_FACTS_RESULT_DIR")
        .unwrap_or_else(|_| DEFAULT_RESULT_DIR.into());
    fs::create_dir_all(&result_dir)?;
    let (columns, pairs) = corpus::pairs()?;
    let mut pages_out = csv(format!("{result_dir}/pages.csv"))?;
    writeln!(
        pages_out,
        "pair,page,rows,non_null,stat_low,stat_high,header_low,header_high,non_decreasing,header_bytes,value_bytes"
    )?;
    let mut queries_out = csv(format!("{result_dir}/queries.csv"))?;
    writeln!(
        queries_out,
        "pair,predicate,pages,exact_skipped,exact_accepted,header_skipped,header_accepted"
    )?;
    for (pair_id, pair) in pairs.iter().enumerate() {
        let values = &columns[pair.selector].values;
        let file = delta_file(values)?;
        let facts = page_facts(file)?;
        let pages = values.chunks(PAGE_ROWS).collect::<Vec<_>>();
        if facts.len() != pages.len() {
            return Err(format!(
                "pair {pair_id}: {} delta pages for {} pages of rows",
                facts.len(),
                pages.len()
            )
            .into());
        }
        let stats = pages
            .iter()
            .map(|page| {
                let present = page.iter().flatten().map(|&value| i128::from(value));
                present.clone().min().zip(present.max())
            })
            .collect::<Vec<_>>();
        for (index, ((page, stat), fact)) in pages.iter().zip(&stats).zip(&facts).enumerate() {
            // Soundness: the header bounds must contain every value.
            if let (Some((low, high)), Some(fact)) = (stat, fact)
                && (fact.lower > *low || fact.upper < *high)
            {
                return Err(
                    format!("pair {pair_id} page {index}: header bounds exclude a value").into(),
                );
            }
            let field =
                |value: Option<i128>| value.map(|value| value.to_string()).unwrap_or_default();
            writeln!(
                pages_out,
                "{pair_id},{index},{},{},{},{},{},{},{},{},{}",
                page.len(),
                page.iter().flatten().count(),
                field(stat.map(|stat| stat.0)),
                field(stat.map(|stat| stat.1)),
                field(fact.map(|fact| fact.lower)),
                field(fact.map(|fact| fact.upper)),
                fact.is_some_and(|fact| fact.non_decreasing),
                fact.map_or(0, |fact| fact.header_bytes),
                fact.map_or(0, |fact| fact.value_bytes),
            )?;
        }
        for predicate in predicates(values)? {
            let (low, high) = (i128::from(predicate.low), i128::from(predicate.high));
            let mut counts = [0_usize; 4];
            for ((page, stat), fact) in pages.iter().zip(&stats).zip(&facts) {
                let dense = page.iter().all(Option::is_some);
                let decide = |bounds: Option<(i128, i128)>| match bounds {
                    // A page with no value matches nothing.
                    None => (true, false),
                    Some((lower, upper)) => (
                        upper < low || lower > high,
                        dense && low <= lower && upper <= high,
                    ),
                };
                let exact = decide(*stat);
                let header = match (stat, fact) {
                    (None, _) => (true, false),
                    (Some(_), Some(fact)) => decide(Some((fact.lower, fact.upper))),
                    (Some(_), None) => (false, false),
                };
                counts[0] += usize::from(exact.0);
                counts[1] += usize::from(exact.1);
                counts[2] += usize::from(header.0);
                counts[3] += usize::from(header.1);
            }
            writeln!(
                queries_out,
                "{pair_id},{},{},{},{},{},{}",
                predicate.label,
                pages.len(),
                counts[0],
                counts[1],
                counts[2],
                counts[3]
            )?;
        }
        if pair_id % 50 == 49 {
            eprintln!("read {} of {} pairs", pair_id + 1, pairs.len());
        }
    }
    pages_out.flush()?;
    queries_out.flush()?;
    Ok(())
}

/// One column as uncompressed Parquet v2 data pages of `PAGE_ROWS` rows,
/// delta-encoded, with no dictionary.
fn delta_file(values: &[Option<i64>]) -> Result<Bytes, Box<dyn Error>> {
    let schema = Arc::new(Schema::new(vec![Field::new(
        "value",
        DataType::Int64,
        values.iter().any(Option::is_none),
    )]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(Int64Array::from(values.to_vec()))],
    )?;
    let properties = WriterProperties::builder()
        .set_writer_version(WriterVersion::PARQUET_2_0)
        .set_dictionary_enabled(false)
        .set_encoding(Encoding::DELTA_BINARY_PACKED)
        .set_statistics_enabled(EnabledStatistics::None)
        .set_data_page_row_count_limit(PAGE_ROWS)
        .set_write_batch_size(PAGE_ROWS)
        .set_max_row_group_row_count(Some(values.len().max(1)))
        .build();
    let mut out = Vec::new();
    let mut writer = ArrowWriter::try_new(&mut out, schema, Some(properties))?;
    writer.write(&batch)?;
    writer.close()?;
    Ok(Bytes::from(out))
}

/// Header facts of every data page, in order; `None` for a page that holds
/// no value.
fn page_facts(file: Bytes) -> Result<Vec<Option<HeaderFacts>>, Box<dyn Error>> {
    let reader = SerializedFileReader::new(file)?;
    let metadata = reader.metadata();
    if metadata.num_row_groups() != 1
        || metadata.row_group(0).column(0).column_type() != Type::INT64
    {
        return Err("expected one INT64 column in one row group".into());
    }
    let mut pages = reader.get_row_group(0)?.get_column_page_reader(0)?;
    let mut facts = Vec::new();
    while let Some(page) = pages.get_next_page()? {
        match page {
            Page::DataPageV2 {
                buf,
                encoding: Encoding::DELTA_BINARY_PACKED,
                num_values,
                num_nulls,
                def_levels_byte_len,
                rep_levels_byte_len,
                is_compressed: false,
                ..
            } => {
                let skip = (def_levels_byte_len + rep_levels_byte_len) as usize;
                let values = buf.get(skip..).ok_or("page shorter than its levels")?;
                facts.push(if num_values == num_nulls {
                    None
                } else {
                    Some(delta_header_facts(values)?)
                });
            }
            Page::DictionaryPage { .. } => return Err("unexpected dictionary page".into()),
            other => return Err(format!("unexpected page {:?}", other.page_type()).into()),
        }
    }
    Ok(facts)
}

/// Bounds and order of a DELTA_BINARY_PACKED value section, from its page
/// header and block headers; packed deltas are skipped, never read.
fn delta_header_facts(bytes: &[u8]) -> Result<HeaderFacts, String> {
    let mut cursor = 0;
    let block_size = varint(bytes, &mut cursor)? as usize;
    let miniblocks = varint(bytes, &mut cursor)? as usize;
    let total = varint(bytes, &mut cursor)? as usize;
    let first = i128::from(zigzag(varint(bytes, &mut cursor)?));
    if miniblocks == 0 || block_size == 0 || !block_size.is_multiple_of(miniblocks) {
        return Err("invalid delta header".into());
    }
    let per_miniblock = block_size / miniblocks;
    let (mut lower, mut upper) = (first, first);
    let (mut low_now, mut high_now) = (first, first);
    let mut non_decreasing = true;
    let mut header_bytes = cursor;
    let mut remaining = total.saturating_sub(1);
    while remaining > 0 {
        let block_start = cursor;
        let min_delta = i128::from(zigzag(varint(bytes, &mut cursor)?));
        let widths = bytes
            .get(cursor..cursor + miniblocks)
            .ok_or("delta block truncated")?
            .to_vec();
        cursor += miniblocks;
        header_bytes += cursor - block_start;
        non_decreasing &= min_delta >= 0;
        let in_block = remaining.min(block_size);
        for &width in widths.iter().take(in_block.div_ceil(per_miniblock)) {
            let steps = remaining.min(per_miniblock) as i128;
            if width > 64 {
                return Err("delta bit width exceeds 64".into());
            }
            let widest = (1_i128 << width) - 1;
            // Values move linearly within a miniblock's bounds, so the
            // extremes lie at its ends.
            low_now += steps * min_delta;
            high_now += steps * (min_delta + widest);
            lower = lower.min(low_now);
            upper = upper.max(high_now);
            cursor += per_miniblock * width as usize / 8;
            remaining -= steps as usize;
        }
    }
    Ok(HeaderFacts {
        lower,
        upper,
        non_decreasing,
        header_bytes,
        value_bytes: bytes.len(),
    })
}

fn varint(bytes: &[u8], cursor: &mut usize) -> Result<u64, String> {
    let mut value = 0_u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*cursor).ok_or("varint truncated")?;
        *cursor += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err("varint too long".into())
}

fn zigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}
