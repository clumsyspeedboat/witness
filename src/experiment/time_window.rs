//! Exact cross-column time-window aggregate and fair Parquet controls.

use arrow_array::{Array, BooleanArray, Int64Array, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use bytes::Bytes;
use parquet::arrow::arrow_reader::{
    ArrowPredicateFn, ArrowReaderOptions, ParquetRecordBatchReaderBuilder, RowFilter, RowSelection,
};
use parquet::arrow::{ArrowWriter, ProjectionMask};
use parquet::basic::BoundaryOrder;
use parquet::basic::{Compression, Encoding, ZstdLevel};
use parquet::errors::{ParquetError, Result as ParquetResult};
use parquet::file::metadata::PageIndexPolicy;
use parquet::file::metadata::ParquetMetaData;
use parquet::file::page_index::column_index::ColumnIndexMetaData;
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use parquet::file::reader::{ChunkReader, Length};
use std::io::{Cursor, Read};
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParquetAggregate {
    pub sum: i128,
    pub matched_rows: usize,
    pub bytes_read: usize,
    pub unique_bytes: usize,
    pub read_calls: usize,
    pub timestamp_values_examined: usize,
    pub page_index_entries_examined: usize,
    pub candidate_pages: usize,
    pub boundary_order_used: bool,
}

pub fn brute_window_sum(
    timestamps: &[Option<i64>],
    values: &[Option<i64>],
    lower: i64,
    upper: i64,
) -> Result<(i128, Vec<(usize, usize)>), String> {
    if timestamps.len() != values.len() {
        return Err("timestamp/value length mismatch".to_string());
    }
    let mut sum = 0i128;
    let mut ranges = Vec::new();
    let mut run_start = None;
    for (index, (timestamp, value)) in timestamps.iter().zip(values).enumerate() {
        let selected = timestamp.is_some_and(|timestamp| lower <= timestamp && timestamp < upper);
        if selected {
            if run_start.is_none() {
                run_start = Some(index);
            }
            if let Some(value) = value {
                sum = sum
                    .checked_add(*value as i128)
                    .ok_or_else(|| "time-window sum overflow".to_string())?;
            }
        } else if let Some(start) = run_start.take() {
            ranges.push((start, index));
        }
    }
    if let Some(start) = run_start {
        ranges.push((start, timestamps.len()));
    }
    Ok((sum, ranges))
}

pub fn parquet_pair_file(
    timestamps: &[Option<i64>],
    values: &[Option<i64>],
    page_rows: usize,
) -> Result<Vec<u8>, String> {
    if timestamps.len() != values.len() || page_rows == 0 {
        return Err("invalid timestamp/value pair".to_string());
    }
    let schema = Arc::new(Schema::new(vec![
        Field::new(
            "timestamp",
            DataType::Int64,
            timestamps.iter().any(Option::is_none),
        ),
        Field::new("value", DataType::Int64, values.iter().any(Option::is_none)),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(Int64Array::from(timestamps.to_vec())),
            Arc::new(Int64Array::from(values.to_vec())),
        ],
    )
    .map_err(|error| error.to_string())?;
    let properties = WriterProperties::builder()
        .set_dictionary_enabled(false)
        .set_encoding(Encoding::DELTA_BINARY_PACKED)
        .set_compression(Compression::ZSTD(
            ZstdLevel::try_new(3).map_err(|error| error.to_string())?,
        ))
        .set_statistics_enabled(EnabledStatistics::Page)
        .set_data_page_row_count_limit(page_rows)
        .set_write_batch_size(page_rows)
        .set_max_row_group_row_count(Some(page_rows.saturating_mul(64)))
        .build();
    let mut out = Vec::new();
    let mut writer = ArrowWriter::try_new(Cursor::new(&mut out), schema, Some(properties))
        .map_err(|error| error.to_string())?;
    writer.write(&batch).map_err(|error| error.to_string())?;
    writer.close().map_err(|error| error.to_string())?;
    Ok(out)
}

pub fn parquet_full_sum(bytes: Bytes, lower: i64, upper: i64) -> Result<ParquetAggregate, String> {
    parquet_full_sum_reader(bytes, lower, upper, 0)
}

pub fn parquet_late_sum(bytes: Bytes, lower: i64, upper: i64) -> Result<ParquetAggregate, String> {
    parquet_late_sum_reader(bytes, lower, upper, 0)
}

pub fn parquet_indexed_sum(
    bytes: Bytes,
    lower: i64,
    upper: i64,
) -> Result<ParquetAggregate, String> {
    parquet_indexed_sum_reader(bytes, lower, upper, 0)
}

pub fn parquet_boundary_sum(
    bytes: Bytes,
    lower: i64,
    upper: i64,
) -> Result<ParquetAggregate, String> {
    parquet_boundary_sum_reader(bytes, lower, upper, 0)
}

pub fn parquet_oracle_sum(
    bytes: Bytes,
    ranges: &[(usize, usize)],
    rows: usize,
) -> Result<ParquetAggregate, String> {
    parquet_oracle_sum_reader(bytes, ranges, rows, 0)
}

pub fn parquet_full_sum_counted(
    bytes: Bytes,
    lower: i64,
    upper: i64,
) -> Result<ParquetAggregate, String> {
    let reader = CountingChunkReader::new(bytes);
    let trace = reader.trace.clone();
    parquet_full_sum_reader(reader, lower, upper, 0).map(|result| traced(result, &trace))
}

pub fn parquet_late_sum_counted(
    bytes: Bytes,
    lower: i64,
    upper: i64,
) -> Result<ParquetAggregate, String> {
    let reader = CountingChunkReader::new(bytes);
    let trace = reader.trace.clone();
    parquet_late_sum_reader(reader, lower, upper, 0).map(|mut result| {
        result = traced(result, &trace);
        result
    })
}

pub fn parquet_indexed_sum_counted(
    bytes: Bytes,
    lower: i64,
    upper: i64,
) -> Result<ParquetAggregate, String> {
    let reader = CountingChunkReader::new(bytes);
    let trace = reader.trace.clone();
    parquet_indexed_sum_reader(reader, lower, upper, 0).map(|mut result| {
        result = traced(result, &trace);
        result
    })
}

pub fn parquet_boundary_sum_counted(
    bytes: Bytes,
    lower: i64,
    upper: i64,
) -> Result<ParquetAggregate, String> {
    let reader = CountingChunkReader::new(bytes);
    let trace = reader.trace.clone();
    parquet_boundary_sum_reader(reader, lower, upper, 0).map(|result| traced(result, &trace))
}

pub fn parquet_oracle_sum_counted(
    bytes: Bytes,
    ranges: &[(usize, usize)],
    rows: usize,
) -> Result<ParquetAggregate, String> {
    let reader = CountingChunkReader::new(bytes);
    let trace = reader.trace.clone();
    parquet_oracle_sum_reader(reader, ranges, rows, 0).map(|mut result| {
        result = traced(result, &trace);
        result
    })
}

fn traced(mut result: ParquetAggregate, trace: &ReadTrace) -> ParquetAggregate {
    let counts = trace.counts();
    result.bytes_read = counts.delivered;
    result.unique_bytes = counts.unique;
    result.read_calls = counts.calls;
    result
}

fn reader_options() -> ArrowReaderOptions {
    ArrowReaderOptions::new().with_page_index_policy(PageIndexPolicy::Required)
}

fn parquet_full_sum_reader<T: ChunkReader + 'static>(
    reader: T,
    lower: i64,
    upper: i64,
    bytes_read: usize,
) -> Result<ParquetAggregate, String> {
    let batches = ParquetRecordBatchReaderBuilder::try_new_with_options(reader, reader_options())
        .map_err(|error| error.to_string())?
        .build()
        .map_err(|error| error.to_string())?;
    let mut sum = 0i128;
    let mut matched_rows = 0usize;
    let mut timestamp_values_examined = 0usize;
    for batch in batches {
        let batch = batch.map_err(|error| error.to_string())?;
        let timestamps = int64_column(&batch, 0)?;
        let values = int64_column(&batch, 1)?;
        timestamp_values_examined += batch.num_rows();
        for index in 0..batch.num_rows() {
            if !timestamps.is_null(index) {
                let timestamp = timestamps.value(index);
                if lower <= timestamp && timestamp < upper {
                    matched_rows += 1;
                    if !values.is_null(index) {
                        sum = sum
                            .checked_add(values.value(index) as i128)
                            .ok_or_else(|| "Parquet sum overflow".to_string())?;
                    }
                }
            }
        }
    }
    Ok(ParquetAggregate {
        sum,
        matched_rows,
        bytes_read,
        unique_bytes: bytes_read,
        read_calls: 0,
        timestamp_values_examined,
        page_index_entries_examined: 0,
        candidate_pages: 0,
        boundary_order_used: false,
    })
}

fn parquet_late_sum_reader<T: ChunkReader + 'static>(
    reader: T,
    lower: i64,
    upper: i64,
    bytes_read: usize,
) -> Result<ParquetAggregate, String> {
    let builder = ParquetRecordBatchReaderBuilder::try_new_with_options(reader, reader_options())
        .map_err(|error| error.to_string())?;
    let timestamp_projection = ProjectionMask::leaves(builder.parquet_schema(), [0]);
    let value_projection = ProjectionMask::leaves(builder.parquet_schema(), [1]);
    let predicate = ArrowPredicateFn::new(timestamp_projection, move |batch: RecordBatch| {
        let timestamps = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| {
                arrow_schema::ArrowError::CastError("expected Int64 timestamp".into())
            })?;
        Ok(BooleanArray::from_iter((0..timestamps.len()).map(
            |index| {
                (!timestamps.is_null(index)).then(|| {
                    let timestamp = timestamps.value(index);
                    lower <= timestamp && timestamp < upper
                })
            },
        )))
    });
    let timestamp_values_examined = usize::try_from(builder.metadata().file_metadata().num_rows())
        .map_err(|_| "Parquet row count exceeds usize")?;
    let batches = builder
        .with_row_filter(RowFilter::new(vec![Box::new(predicate)]))
        .with_projection(value_projection)
        .build()
        .map_err(|error| error.to_string())?;
    sum_projected_values(batches, bytes_read, timestamp_values_examined)
}

fn parquet_indexed_sum_reader<T: ChunkReader + 'static>(
    reader: T,
    lower: i64,
    upper: i64,
    bytes_read: usize,
) -> Result<ParquetAggregate, String> {
    let builder = ParquetRecordBatchReaderBuilder::try_new_with_options(reader, reader_options())
        .map_err(|error| error.to_string())?;
    let candidates = page_index_candidate_ranges(builder.metadata(), lower, upper, false)?;
    let candidate_ranges = candidates.ranges;
    let timestamp_values_examined = candidate_ranges.iter().map(|range| range.len()).sum();
    let rows = usize::try_from(builder.metadata().file_metadata().num_rows())
        .map_err(|_| "Parquet row count exceeds usize")?;
    let selection = RowSelection::from_consecutive_ranges(candidate_ranges.into_iter(), rows);
    let timestamp_projection = ProjectionMask::leaves(builder.parquet_schema(), [0]);
    let value_projection = ProjectionMask::leaves(builder.parquet_schema(), [1]);
    let predicate = ArrowPredicateFn::new(timestamp_projection, move |batch: RecordBatch| {
        let timestamps = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| {
                arrow_schema::ArrowError::CastError("expected Int64 timestamp".into())
            })?;
        Ok(BooleanArray::from_iter((0..timestamps.len()).map(
            |index| {
                (!timestamps.is_null(index)).then(|| {
                    let timestamp = timestamps.value(index);
                    lower <= timestamp && timestamp < upper
                })
            },
        )))
    });
    let batches = builder
        .with_row_selection(selection)
        .with_row_filter(RowFilter::new(vec![Box::new(predicate)]))
        .with_projection(value_projection)
        .build()
        .map_err(|error| error.to_string())?;
    let mut result = sum_projected_values(batches, bytes_read, timestamp_values_examined)?;
    result.page_index_entries_examined = candidates.entries_examined;
    result.candidate_pages = candidates.candidate_pages;
    Ok(result)
}

fn parquet_boundary_sum_reader<T: ChunkReader + 'static>(
    reader: T,
    lower: i64,
    upper: i64,
    bytes_read: usize,
) -> Result<ParquetAggregate, String> {
    let builder = ParquetRecordBatchReaderBuilder::try_new_with_options(reader, reader_options())
        .map_err(|error| error.to_string())?;
    let candidates = page_index_candidate_ranges(builder.metadata(), lower, upper, true)?;
    let timestamp_values_examined = candidates.ranges.iter().map(|range| range.len()).sum();
    let rows = usize::try_from(builder.metadata().file_metadata().num_rows())
        .map_err(|_| "Parquet row count exceeds usize")?;
    let selection = RowSelection::from_consecutive_ranges(candidates.ranges.into_iter(), rows);
    let timestamp_projection = ProjectionMask::leaves(builder.parquet_schema(), [0]);
    let value_projection = ProjectionMask::leaves(builder.parquet_schema(), [1]);
    let predicate = ArrowPredicateFn::new(timestamp_projection, move |batch: RecordBatch| {
        let timestamps = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| {
                arrow_schema::ArrowError::CastError("expected Int64 timestamp".into())
            })?;
        Ok(BooleanArray::from_iter((0..timestamps.len()).map(
            |index| {
                (!timestamps.is_null(index)).then(|| {
                    let timestamp = timestamps.value(index);
                    lower <= timestamp && timestamp < upper
                })
            },
        )))
    });
    let batches = builder
        .with_row_selection(selection)
        .with_row_filter(RowFilter::new(vec![Box::new(predicate)]))
        .with_projection(value_projection)
        .build()
        .map_err(|error| error.to_string())?;
    let mut result = sum_projected_values(batches, bytes_read, timestamp_values_examined)?;
    result.page_index_entries_examined = candidates.entries_examined;
    result.candidate_pages = candidates.candidate_pages;
    result.boundary_order_used = candidates.boundary_order_used;
    Ok(result)
}

struct PageCandidates {
    ranges: Vec<std::ops::Range<usize>>,
    entries_examined: usize,
    candidate_pages: usize,
    boundary_order_used: bool,
}

fn page_index_candidate_ranges(
    metadata: &ParquetMetaData,
    lower: i64,
    upper: i64,
    use_boundary_order: bool,
) -> Result<PageCandidates, String> {
    let column_indexes = metadata
        .column_index()
        .ok_or_else(|| "Parquet column index missing".to_string())?;
    let offset_indexes = metadata
        .offset_index()
        .ok_or_else(|| "Parquet offset index missing".to_string())?;
    let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
    let mut entries_examined = 0;
    let mut candidate_pages = 0;
    let mut boundary_order_used = false;
    let mut row_group_start = 0usize;
    for row_group in 0..metadata.num_row_groups() {
        let index = match &column_indexes[row_group][0] {
            ColumnIndexMetaData::INT64(index) => index,
            _ => return Err("timestamp page index is not INT64".to_string()),
        };
        let locations = offset_indexes[row_group][0].page_locations();
        if locations.len() != index.num_pages() as usize {
            return Err("Parquet page and offset index lengths differ".to_string());
        }
        let row_group_rows = usize::try_from(metadata.row_group(row_group).num_rows())
            .map_err(|_| "Parquet row-group count exceeds usize")?;
        let sorted_group = use_boundary_order
            && column_indexes[row_group][0].get_boundary_order() == Some(BoundaryOrder::ASCENDING);
        let page_range = if sorted_group {
            boundary_order_used = true;
            ascending_candidate_pages(index, lower, upper, &mut entries_examined)?
        } else {
            0..locations.len()
        };
        for page in page_range {
            let location = &locations[page];
            let Some(&minimum) = index.min_value(page) else {
                continue;
            };
            let maximum = *index
                .max_value(page)
                .ok_or_else(|| "Parquet page maximum missing".to_string())?;
            if maximum < lower || minimum >= upper {
                if !sorted_group {
                    entries_examined += 1;
                }
                continue;
            }
            if !sorted_group {
                entries_examined += 1;
            }
            candidate_pages += 1;
            let start = usize::try_from(location.first_row_index)
                .map_err(|_| "negative Parquet page row offset")?;
            let end = if page + 1 < locations.len() {
                usize::try_from(locations[page + 1].first_row_index)
                    .map_err(|_| "negative Parquet page row offset")?
            } else {
                row_group_rows
            };
            push_range(&mut ranges, row_group_start + start, row_group_start + end);
        }
        row_group_start = row_group_start
            .checked_add(row_group_rows)
            .ok_or_else(|| "Parquet row offset overflow".to_string())?;
    }
    Ok(PageCandidates {
        ranges,
        entries_examined,
        candidate_pages,
        boundary_order_used,
    })
}

fn ascending_candidate_pages(
    index: &parquet::file::page_index::column_index::PrimitiveColumnIndex<i64>,
    lower: i64,
    upper: i64,
    examined: &mut usize,
) -> Result<std::ops::Range<usize>, String> {
    let pages = index.num_pages() as usize;
    let mut left = 0;
    let mut right = pages;
    while left < right {
        let mid = left + (right - left) / 2;
        *examined += 1;
        let maximum = index.max_value(mid).ok_or("Parquet page maximum missing")?;
        if *maximum < lower {
            left = mid + 1;
        } else {
            right = mid;
        }
    }
    let start = left;
    right = pages;
    while left < right {
        let mid = left + (right - left) / 2;
        *examined += 1;
        let minimum = index.min_value(mid).ok_or("Parquet page minimum missing")?;
        if *minimum < upper {
            left = mid + 1;
        } else {
            right = mid;
        }
    }
    Ok(start..left)
}

fn push_range(ranges: &mut Vec<std::ops::Range<usize>>, start: usize, end: usize) {
    if let Some(previous) = ranges.last_mut()
        && previous.end == start
    {
        previous.end = end;
    } else if start < end {
        ranges.push(start..end);
    }
}

fn parquet_oracle_sum_reader<T: ChunkReader + 'static>(
    reader: T,
    ranges: &[(usize, usize)],
    rows: usize,
    bytes_read: usize,
) -> Result<ParquetAggregate, String> {
    let builder = ParquetRecordBatchReaderBuilder::try_new_with_options(reader, reader_options())
        .map_err(|error| error.to_string())?;
    let value_projection = ProjectionMask::leaves(builder.parquet_schema(), [1]);
    let selection =
        RowSelection::from_consecutive_ranges(ranges.iter().map(|&(start, end)| start..end), rows);
    let batches = builder
        .with_row_selection(selection)
        .with_projection(value_projection)
        .build()
        .map_err(|error| error.to_string())?;
    sum_projected_values(batches, bytes_read, 0)
}

fn sum_projected_values(
    batches: impl Iterator<Item = Result<RecordBatch, arrow_schema::ArrowError>>,
    bytes_read: usize,
    timestamp_values_examined: usize,
) -> Result<ParquetAggregate, String> {
    let mut sum = 0i128;
    let mut matched_rows = 0usize;
    for batch in batches {
        let batch = batch.map_err(|error| error.to_string())?;
        let values = int64_column(&batch, 0)?;
        matched_rows += values.len();
        for index in 0..values.len() {
            if !values.is_null(index) {
                sum = sum
                    .checked_add(values.value(index) as i128)
                    .ok_or_else(|| "Parquet sum overflow".to_string())?;
            }
        }
    }
    Ok(ParquetAggregate {
        sum,
        matched_rows,
        bytes_read,
        unique_bytes: bytes_read,
        read_calls: 0,
        timestamp_values_examined,
        page_index_entries_examined: 0,
        candidate_pages: 0,
        boundary_order_used: false,
    })
}

fn int64_column(batch: &RecordBatch, index: usize) -> Result<&Int64Array, String> {
    batch
        .column(index)
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| "expected Int64 column".to_string())
}

#[derive(Clone)]
struct CountingChunkReader {
    bytes: Bytes,
    trace: Arc<ReadTrace>,
}

impl CountingChunkReader {
    fn new(bytes: Bytes) -> Self {
        Self {
            bytes,
            trace: Arc::new(ReadTrace::default()),
        }
    }
}

impl Length for CountingChunkReader {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }
}

impl ChunkReader for CountingChunkReader {
    type T = CountingRead;

    fn get_read(&self, start: u64) -> ParquetResult<Self::T> {
        if start > self.len() {
            return Err(ParquetError::General(
                "read starts after end of file".into(),
            ));
        }
        let mut cursor = Cursor::new(self.bytes.clone());
        cursor.set_position(start);
        Ok(CountingRead {
            cursor,
            trace: self.trace.clone(),
        })
    }

    fn get_bytes(&self, start: u64, length: usize) -> ParquetResult<Bytes> {
        let start = usize::try_from(start)
            .map_err(|_| ParquetError::General("read offset exceeds usize".into()))?;
        let end = start
            .checked_add(length)
            .ok_or_else(|| ParquetError::General("read range overflow".into()))?;
        if end > self.bytes.len() {
            return Err(ParquetError::General("read exceeds end of file".into()));
        }
        self.trace.record(start..end);
        Ok(self.bytes.slice(start..end))
    }
}

struct CountingRead {
    cursor: Cursor<Bytes>,
    trace: Arc<ReadTrace>,
}

impl Read for CountingRead {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let start = self.cursor.position() as usize;
        let read = self.cursor.read(buffer)?;
        self.trace.record(start..start + read);
        Ok(read)
    }
}

#[derive(Default)]
struct ReadTrace {
    delivered: AtomicUsize,
    calls: AtomicUsize,
    ranges: Mutex<Vec<Range<usize>>>,
}

#[derive(Clone, Copy)]
struct ReadCounts {
    delivered: usize,
    unique: usize,
    calls: usize,
}

impl ReadTrace {
    fn record(&self, range: Range<usize>) {
        if range.is_empty() {
            return;
        }
        self.delivered.fetch_add(range.len(), Ordering::Relaxed);
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.ranges.lock().expect("read trace poisoned").push(range);
    }

    fn counts(&self) -> ReadCounts {
        let mut ranges = self.ranges.lock().expect("read trace poisoned").clone();
        ranges.sort_by_key(|range| range.start);
        let mut unique = 0usize;
        let mut merged: Option<Range<usize>> = None;
        for range in ranges {
            match &mut merged {
                Some(previous) if range.start <= previous.end => {
                    previous.end = previous.end.max(range.end);
                }
                Some(previous) => {
                    unique += previous.len();
                    merged = Some(range);
                }
                None => merged = Some(range),
            }
        }
        unique += merged.map_or(0, |range| range.len());
        ReadCounts {
            delivered: self.delivered.load(Ordering::Relaxed),
            unique,
            calls: self.calls.load(Ordering::Relaxed),
        }
    }
}
