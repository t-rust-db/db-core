//! Pages to one Parquet file (ADR 0027, flush step 1).
//!
//! The file's columns are the seven predefined stream columns, then every
//! Tier-3 field any page has, in first-seen order. A field missing from a
//! page is NULL in that page's rows. A field whose type differs between
//! pages takes the widest: integers and floats become DOUBLE, any other
//! mix becomes a string (ADR 0018's "conflicts degrade to Str"). A field
//! named like a predefined column is left out: the predefined one is what
//! queries see.
//!
//! A record table's (#594) file has exactly the table's schema instead.
//!
//! The file's key-value metadata records `lsn_lo` and `lsn_hi`, so a file
//! describes itself without the manifest.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use super::fields::schema;
use super::manifest::KeyRange;
use super::page::{Page, PageBody};
use crate::storage::parquet::writer::{
    ColumnSpec, ColumnType, ColumnValues, ParquetWriter, WriteError, WriterOptions,
};
use crate::storage::stream::adapter::stored_column_values;
use crate::storage::stream::detect::Format;
use crate::vm::batch::Value;

/// What a flushed file holds, for its manifest entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FlushedFile {
    pub(crate) rows: u64,
    pub(crate) bytes: u64,
    pub(crate) observed_ts: Option<(i64, i64)>,
    pub(crate) timestamp: Option<(i64, i64)>,
    pub(crate) keys: Vec<KeyRange>,
}

/// `values` as `column_type`. Called only with the type [`schema`] chose,
/// so every non-null value converts; a string column renders the others.
pub(crate) fn convert(values: Vec<Value>, column_type: ColumnType) -> ColumnValues {
    let ints = |v: &Value| match v {
        Value::Int(i) => Some(*i),
        _ => None,
    };
    match column_type {
        ColumnType::TimestampNanos => {
            ColumnValues::TimestampNanos(values.iter().map(ints).collect())
        }
        // `schema` never picks these two; kept total, values as their
        // nearest representation (none for FLOAT, which would narrow).
        ColumnType::TimestampMicros => {
            ColumnValues::TimestampMicros(values.iter().map(ints).collect())
        }
        ColumnType::Int64 => ColumnValues::Int64(values.iter().map(ints).collect()),
        ColumnType::Int32 => ColumnValues::Int32(
            values
                .iter()
                .map(|v| ints(v).and_then(|i| i32::try_from(i).ok()))
                .collect(),
        ),
        ColumnType::Double => ColumnValues::Double(
            values
                .iter()
                .map(|v| match v {
                    Value::Float(f) => Some(*f),
                    // An INT/FLOAT conflict widens to DOUBLE.
                    Value::Int(i) => Some(*i as f64),
                    _ => None,
                })
                .collect(),
        ),
        ColumnType::Float => ColumnValues::Float(vec![None; values.len()]),
        ColumnType::UInt8 => ColumnValues::UInt8(unsigned(&values)),
        ColumnType::UInt16 => ColumnValues::UInt16(unsigned(&values)),
        ColumnType::UInt32 => ColumnValues::UInt32(unsigned(&values)),
        ColumnType::UInt64 => ColumnValues::UInt64(unsigned(&values)),
        // Nor these: a decimal or byte value has no exact `Value` to come
        // from.
        ColumnType::Decimal { .. } => ColumnValues::Decimal(vec![None; values.len()]),
        ColumnType::Binary => ColumnValues::Binary(vec![None; values.len()]),
        ColumnType::FixedLenByteArray(_) => {
            ColumnValues::FixedLenByteArray(vec![None; values.len()])
        }
        ColumnType::MapUtf8 => ColumnValues::Map(vec![None; values.len()]),
        ColumnType::ListUtf8 => ColumnValues::List(vec![None; values.len()]),
        ColumnType::Boolean => ColumnValues::Boolean(
            values
                .iter()
                .map(|v| match v {
                    Value::Bool(b) => Some(*b),
                    _ => None,
                })
                .collect(),
        ),
        ColumnType::Utf8 => ColumnValues::Utf8(
            values
                .into_iter()
                .map(|v| match v {
                    Value::Null => None,
                    Value::Str(s) => Some(s.into_owned()),
                    other => Some(other.to_string()),
                })
                .collect(),
        ),
    }
}

/// Integers that fit `T`, the others NULL.
fn unsigned<T: TryFrom<i64>>(values: &[Value]) -> Vec<Option<T>> {
    values
        .iter()
        .map(|v| match v {
            Value::Int(i) => T::try_from(*i).ok(),
            _ => None,
        })
        .collect()
}

/// One page's columns in `specs` order, as a `format` table stores them.
fn page_columns(page: &Page, specs: &[ColumnSpec], format: Format) -> Vec<ColumnValues> {
    let segment = match &page.body {
        PageBody::Lines(segment) => segment,
        PageBody::Rows(rows) => return rows.columns().to_vec(),
    };
    let rows = segment.len();
    specs
        .iter()
        .map(|spec| {
            let values = stored_column_values(segment, &spec.name, format)
                .unwrap_or_else(|| vec![Value::Null; rows]);
            convert(values, spec.column_type)
        })
        .collect()
}

pub(crate) fn bounds(values: impl Iterator<Item = Option<i64>>) -> Option<(i64, i64)> {
    values.flatten().fold(None, |acc, v| match acc {
        None => Some((v, v)),
        Some((lo, hi)) => Some((lo.min(v), hi.max(v))),
    })
}

/// Writes `pages` (contiguous, oldest first, at least one) of a `format`
/// table to `path` and fsyncs it.
pub(crate) fn write_file(
    path: &Path,
    pages: &[Page],
    format: Format,
    options: &WriterOptions,
) -> Result<FlushedFile, WriteError> {
    let (Some(first), Some(last)) = (pages.first(), pages.last()) else {
        return Err(WriteError::InvalidSchema("no pages to flush".to_string()));
    };
    let specs = schema(pages);
    let mut options = options.clone();
    options.key_value_metadata = vec![
        ("db-core.lsn_lo".to_string(), first.lsn_lo.to_string()),
        ("db-core.lsn_hi".to_string(), last.lsn_hi.to_string()),
    ];
    let mut writer =
        ParquetWriter::new(BufWriter::new(File::create(path)?), specs.clone(), options)?;
    for page in pages {
        writer.write_columns(page_columns(page, &specs, format))?;
    }
    let (out, summary) = writer.finish()?;
    let file = out
        .into_inner()
        .map_err(|e| WriteError::Io(e.into_error()))?;
    file.sync_all()?;
    let observed = bounds(pages.iter().flat_map(Page::observed_ts_ns));
    // A record table has no event-time column of its own.
    let timestamp = bounds(
        pages
            .iter()
            .filter_map(Page::lines)
            .flat_map(|s| s.event_ts_ns().iter().copied()),
    );
    Ok(FlushedFile {
        rows: summary.num_rows,
        bytes: summary.bytes,
        observed_ts: observed,
        timestamp,
        keys: Vec::new(),
    })
}

#[cfg(test)]
mod tests;
