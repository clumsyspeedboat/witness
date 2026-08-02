use std::collections::BTreeSet;
use std::io::Cursor;
use std::sync::Arc;

use arrow_array::{ArrayRef, Int64Array, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use orc_rust::arrow_writer::ArrowWriterBuilder as OrcWriterBuilder;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, Encoding, ZstdLevel};
use parquet::file::properties::WriterProperties;

use crate::access_compiler::{InputColumn, Recipe};

pub const EXAMPLE_ROWS: usize = 16;

#[derive(Clone, Debug)]
pub struct ExampleColumn {
    pub name: &'static str,
    pub role: &'static str,
    pub values: Vec<Option<i64>>,
    pub patch_rows: BTreeSet<usize>,
    pub recipe: Recipe,
}

impl ExampleColumn {
    pub fn input(&self) -> InputColumn {
        InputColumn {
            values: self.values.clone(),
            patch_rows: self.patch_rows.clone(),
        }
    }
}

pub fn example_columns() -> Vec<ExampleColumn> {
    let bitpack = || Recipe::BitPack;
    vec![
        ExampleColumn {
            name: "event_time",
            role: "globally monotone selector",
            values: (0..EXAMPLE_ROWS)
                .map(|row| Some(1_700_000_000 + 60 * row as i64))
                .collect(),
            patch_rows: BTreeSet::new(),
            recipe: Recipe::UnsignedDelta {
                restart_interval: 128,
                deltas: Box::new(bitpack()),
            },
        },
        ExampleColumn {
            name: "meter",
            role: "non-monotone numeric measure",
            values: [
                10, 11, 11, 12, 12, 12, 13, 14, 13, 14, 15, 15, 16, 17, 16, 18,
            ]
            .into_iter()
            .map(Some)
            .collect(),
            patch_rows: BTreeSet::new(),
            recipe: Recipe::For(Box::new(bitpack())),
        },
        ExampleColumn {
            name: "status",
            role: "small-domain run-valued selector",
            values: [1, 1, 1, 2, 2, 2, 2, 1, 1, 3, 3, 3, 2, 2, 1, 1]
                .into_iter()
                .map(Some)
                .collect(),
            patch_rows: BTreeSet::new(),
            recipe: Recipe::Dictionary(Box::new(Recipe::Rle {
                index_interval: 4,
                values: Box::new(bitpack()),
            })),
        },
        ExampleColumn {
            name: "sparse_event",
            role: "nullable sparse category",
            values: [
                None,
                None,
                None,
                Some(7),
                None,
                None,
                None,
                Some(7),
                None,
                None,
                None,
                None,
                Some(9),
                None,
                None,
                None,
            ]
            .to_vec(),
            patch_rows: BTreeSet::new(),
            recipe: Recipe::Nullable {
                rank_interval: 8,
                values: Box::new(Recipe::Dictionary(Box::new(Recipe::Rle {
                    index_interval: 2,
                    values: Box::new(bitpack()),
                }))),
            },
        },
        ExampleColumn {
            name: "reading",
            role: "mostly regular values with one patched outlier",
            values: [
                100, 101, 102, 103, 104, 105, 106, 107, 108, 10_000, 110, 111, 112, 113, 114, 115,
            ]
            .into_iter()
            .map(Some)
            .collect(),
            patch_rows: BTreeSet::from([9]),
            recipe: Recipe::Patch {
                index_interval: 4,
                values: Box::new(Recipe::For(Box::new(bitpack()))),
            },
        },
    ]
}

pub fn table_csv(columns: &[ExampleColumn]) -> Result<Vec<u8>, String> {
    validate_columns(columns)?;
    let mut output = Vec::new();
    {
        let mut writer = csv::Writer::from_writer(&mut output);
        writer
            .write_record(columns.iter().map(|column| column.name))
            .map_err(|error| error.to_string())?;
        for row in 0..EXAMPLE_ROWS {
            writer
                .write_record(columns.iter().map(|column| {
                    column.values[row].map_or_else(String::new, |value| value.to_string())
                }))
                .map_err(|error| error.to_string())?;
        }
        writer.flush().map_err(|error| error.to_string())?;
    }
    Ok(output)
}

pub fn table_parquet(columns: &[ExampleColumn], delta_zstd: bool) -> Result<Vec<u8>, String> {
    let (schema, batch) = arrow_table(columns)?;
    let properties = if delta_zstd {
        WriterProperties::builder()
            .set_dictionary_enabled(false)
            .set_encoding(Encoding::DELTA_BINARY_PACKED)
            .set_compression(Compression::ZSTD(
                ZstdLevel::try_new(3).map_err(|error| error.to_string())?,
            ))
            .build()
    } else {
        WriterProperties::builder()
            .set_dictionary_enabled(true)
            .set_compression(Compression::SNAPPY)
            .build()
    };
    let mut output = Vec::new();
    let mut writer = ArrowWriter::try_new(Cursor::new(&mut output), schema, Some(properties))
        .map_err(|error| error.to_string())?;
    writer.write(&batch).map_err(|error| error.to_string())?;
    writer.close().map_err(|error| error.to_string())?;
    Ok(output)
}

pub fn table_orc(columns: &[ExampleColumn]) -> Result<Vec<u8>, String> {
    let (schema, batch) = arrow_table(columns)?;
    let mut output = Vec::new();
    {
        let mut writer = OrcWriterBuilder::new(&mut output, schema)
            .try_build()
            .map_err(|error| error.to_string())?;
        writer.write(&batch).map_err(|error| error.to_string())?;
        writer.close().map_err(|error| error.to_string())?;
    }
    Ok(output)
}

fn arrow_table(columns: &[ExampleColumn]) -> Result<(Arc<Schema>, RecordBatch), String> {
    validate_columns(columns)?;
    let schema = Arc::new(Schema::new(
        columns
            .iter()
            .map(|column| {
                Field::new(
                    column.name,
                    DataType::Int64,
                    column.values.iter().any(Option::is_none),
                )
            })
            .collect::<Vec<_>>(),
    ));
    let arrays = columns
        .iter()
        .map(|column| Arc::new(Int64Array::from(column.values.clone())) as ArrayRef)
        .collect();
    let batch = RecordBatch::try_new(schema.clone(), arrays).map_err(|error| error.to_string())?;
    Ok((schema, batch))
}

fn validate_columns(columns: &[ExampleColumn]) -> Result<(), String> {
    if columns.is_empty()
        || columns
            .iter()
            .any(|column| column.values.len() != EXAMPLE_ROWS)
    {
        return Err("documentation columns must contain exactly 16 rows".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_is_rectangular_and_exercises_distinct_structures() {
        let columns = example_columns();
        assert_eq!(columns.len(), 5);
        assert!(
            columns
                .iter()
                .all(|column| column.values.len() == EXAMPLE_ROWS)
        );
        assert!(columns[0].values.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(columns[1].values.windows(2).any(|pair| pair[0] > pair[1]));
        assert!(columns[3].values.iter().any(Option::is_none));
        assert_eq!(columns[4].patch_rows, BTreeSet::from([9]));
    }

    #[test]
    fn real_table_writers_emit_nonempty_artifacts() {
        let columns = example_columns();
        assert!(!table_csv(&columns).unwrap().is_empty());
        assert!(!table_parquet(&columns, false).unwrap().is_empty());
        assert!(!table_parquet(&columns, true).unwrap().is_empty());
        assert!(!table_orc(&columns).unwrap().is_empty());
    }
}
