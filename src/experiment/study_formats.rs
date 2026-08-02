//! Physical-format adapters used by the canonical study.
//!
//! Every returned byte vector is a self-contained artifact with enough schema,
//! length, and validity information to reconstruct the logical column.

use arrow_array::{Array, Int64Array, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use bytes::Bytes;
use orc_rust::arrow_reader::ArrowReaderBuilder as OrcReaderBuilder;
use orc_rust::arrow_writer::ArrowWriterBuilder as OrcWriterBuilder;
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::{Compression, Encoding, ZstdLevel};
use parquet::file::properties::WriterProperties;
use pco::{ChunkConfig, standalone};
use std::io::Cursor;
use std::sync::Arc;

const RAW_MAGIC: &[u8; 8] = b"RAWI64V1";
const PCO_MAGIC: &[u8; 8] = b"PCOI64V1";

#[derive(Debug, Clone, Copy)]
pub enum ParquetStudyConfig {
    DictionarySnappy,
    DeltaZstd,
}

pub fn raw_i64_file(values: &[Option<i64>]) -> Vec<u8> {
    let validity = validity_bitmap(values);
    let mut out = Vec::with_capacity(20 + validity.len() + values.len() * 8);
    out.extend_from_slice(RAW_MAGIC);
    out.extend_from_slice(&(values.len() as u64).to_le_bytes());
    out.extend_from_slice(&(validity.len() as u32).to_le_bytes());
    out.extend_from_slice(&validity);
    for value in values.iter().flatten() {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

pub fn decode_raw_i64_file(bytes: &[u8]) -> Result<Vec<Option<i64>>, String> {
    if bytes.len() < 20 || &bytes[..8] != RAW_MAGIC {
        return Err("invalid raw-i64 artifact".to_string());
    }
    let n = usize::try_from(u64::from_le_bytes(bytes[8..16].try_into().unwrap()))
        .map_err(|_| "raw-i64 length exceeds usize")?;
    let bitmap_len = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize;
    let payload_start = 20usize
        .checked_add(bitmap_len)
        .ok_or_else(|| "raw-i64 offset overflow".to_string())?;
    if payload_start > bytes.len() || !matches!(bitmap_len, 0) && bitmap_len != n.div_ceil(8) {
        return Err("invalid raw-i64 validity bitmap".to_string());
    }
    restore_dense(n, &bytes[20..payload_start], &bytes[payload_start..])
}

pub fn csv_i64_file(values: &[Option<i64>]) -> Vec<u8> {
    let mut out = String::from("value\n");
    for value in values {
        if let Some(value) = value {
            out.push_str(&value.to_string());
        }
        out.push('\n');
    }
    out.into_bytes()
}

pub fn pco_i64_file(values: &[Option<i64>], level: usize) -> Result<Vec<u8>, String> {
    let validity = validity_bitmap(values);
    let dense: Vec<i64> = values.iter().flatten().copied().collect();
    let compressed = if dense.is_empty() {
        Vec::new()
    } else {
        standalone::simple_compress(
            &dense,
            &ChunkConfig::default().with_compression_level(level),
        )
        .map_err(|error| error.to_string())?
    };
    let mut out = Vec::new();
    out.extend_from_slice(PCO_MAGIC);
    out.extend_from_slice(&(values.len() as u64).to_le_bytes());
    out.extend_from_slice(&(validity.len() as u32).to_le_bytes());
    out.extend_from_slice(&(compressed.len() as u64).to_le_bytes());
    out.extend_from_slice(&validity);
    out.extend_from_slice(&compressed);
    Ok(out)
}

pub fn decode_pco_i64_file(bytes: &[u8]) -> Result<Vec<Option<i64>>, String> {
    if bytes.len() < 28 || &bytes[..8] != PCO_MAGIC {
        return Err("invalid PCodec artifact".to_string());
    }
    let n = usize::try_from(u64::from_le_bytes(bytes[8..16].try_into().unwrap()))
        .map_err(|_| "PCodec length exceeds usize")?;
    let bitmap_len = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize;
    let compressed_len = usize::try_from(u64::from_le_bytes(bytes[20..28].try_into().unwrap()))
        .map_err(|_| "PCodec payload length exceeds usize")?;
    let payload_start = 28usize
        .checked_add(bitmap_len)
        .ok_or_else(|| "PCodec offset overflow".to_string())?;
    let end = payload_start
        .checked_add(compressed_len)
        .ok_or_else(|| "PCodec offset overflow".to_string())?;
    if (!matches!(bitmap_len, 0) && bitmap_len != n.div_ceil(8)) || end != bytes.len() {
        return Err("invalid PCodec artifact lengths".to_string());
    }
    let dense = if compressed_len == 0 {
        Vec::new()
    } else {
        standalone::simple_decompress::<i64>(&bytes[payload_start..end])
            .map_err(|error| error.to_string())?
    };
    restore_values(n, &bytes[28..payload_start], &dense)
}

pub fn parquet_i64_file(
    values: &[Option<i64>],
    config: ParquetStudyConfig,
) -> Result<Vec<u8>, String> {
    let nullable = values.iter().any(Option::is_none);
    let schema = Arc::new(Schema::new(vec![Field::new(
        "value",
        DataType::Int64,
        nullable,
    )]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(Int64Array::from(values.to_vec()))],
    )
    .map_err(|error| error.to_string())?;
    let properties = match config {
        ParquetStudyConfig::DictionarySnappy => WriterProperties::builder()
            .set_dictionary_enabled(true)
            .set_compression(Compression::SNAPPY)
            .build(),
        ParquetStudyConfig::DeltaZstd => WriterProperties::builder()
            .set_dictionary_enabled(false)
            .set_encoding(Encoding::DELTA_BINARY_PACKED)
            .set_compression(Compression::ZSTD(
                ZstdLevel::try_new(3).map_err(|error| error.to_string())?,
            ))
            .build(),
    };
    let mut out = Vec::new();
    let mut writer = ArrowWriter::try_new(Cursor::new(&mut out), schema, Some(properties))
        .map_err(|error| error.to_string())?;
    writer.write(&batch).map_err(|error| error.to_string())?;
    writer.close().map_err(|error| error.to_string())?;
    Ok(out)
}

pub fn decode_parquet_i64_file(bytes: &[u8]) -> Result<Vec<Option<i64>>, String> {
    let reader = ParquetRecordBatchReaderBuilder::try_new(Bytes::copy_from_slice(bytes))
        .map_err(|error| error.to_string())?
        .build()
        .map_err(|error| error.to_string())?;
    let mut out = Vec::new();
    for batch in reader {
        let batch = batch.map_err(|error| error.to_string())?;
        let array = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| "expected Parquet Int64 column".to_string())?;
        out.extend(
            (0..array.len()).map(|index| (!array.is_null(index)).then(|| array.value(index))),
        );
    }
    Ok(out)
}

/// ORC-Rust 0.8.0 writes ORC RLEv2 but does not yet expose outer compression.
/// The study labels this arm accordingly instead of presenting it as ORC+Zstd.
pub fn orc_i64_file(values: &[Option<i64>]) -> Result<Vec<u8>, String> {
    let nullable = values.iter().any(Option::is_none);
    let schema = Arc::new(Schema::new(vec![Field::new(
        "value",
        DataType::Int64,
        nullable,
    )]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(Int64Array::from(values.to_vec()))],
    )
    .map_err(|error| error.to_string())?;
    let mut out = Vec::new();
    {
        let mut writer = OrcWriterBuilder::new(&mut out, schema)
            .try_build()
            .map_err(|error| error.to_string())?;
        writer.write(&batch).map_err(|error| error.to_string())?;
        writer.close().map_err(|error| error.to_string())?;
    }
    Ok(out)
}

pub fn decode_orc_i64_file(bytes: &[u8]) -> Result<Vec<Option<i64>>, String> {
    let reader = OrcReaderBuilder::try_new(Bytes::copy_from_slice(bytes))
        .map_err(|error| error.to_string())?
        .build();
    let mut out = Vec::new();
    for batch in reader {
        let batch = batch.map_err(|error| error.to_string())?;
        let array = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| "expected ORC Int64 column".to_string())?;
        out.extend(
            (0..array.len()).map(|index| (!array.is_null(index)).then(|| array.value(index))),
        );
    }
    Ok(out)
}

pub fn zstd_file(bytes: &[u8], level: i32) -> Result<Vec<u8>, String> {
    zstd::stream::encode_all(bytes, level).map_err(|error| error.to_string())
}

fn validity_bitmap(values: &[Option<i64>]) -> Vec<u8> {
    if values.iter().all(Option::is_some) {
        return Vec::new();
    }
    let mut bitmap = vec![0u8; values.len().div_ceil(8)];
    for (index, value) in values.iter().enumerate() {
        if value.is_some() {
            bitmap[index / 8] |= 1 << (index % 8);
        }
    }
    bitmap
}

fn restore_dense(
    n: usize,
    validity: &[u8],
    dense_bytes: &[u8],
) -> Result<Vec<Option<i64>>, String> {
    if !dense_bytes.len().is_multiple_of(8) {
        return Err("raw-i64 payload is not aligned".to_string());
    }
    let dense = dense_bytes
        .chunks_exact(8)
        .map(|chunk| i64::from_le_bytes(chunk.try_into().unwrap()))
        .collect::<Vec<_>>();
    restore_values(n, validity, &dense)
}

fn restore_values(n: usize, validity: &[u8], dense: &[i64]) -> Result<Vec<Option<i64>>, String> {
    let mut next = dense.iter().copied();
    let mut out = Vec::with_capacity(n);
    for index in 0..n {
        if validity.is_empty() || validity[index / 8] & (1 << (index % 8)) != 0 {
            out.push(Some(
                next.next()
                    .ok_or_else(|| "dense value underflow".to_string())?,
            ));
        } else {
            out.push(None);
        }
    }
    if next.next().is_some() {
        return Err("dense value overflow".to_string());
    }
    Ok(out)
}
