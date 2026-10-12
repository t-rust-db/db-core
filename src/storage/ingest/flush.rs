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
//! The file's key-value metadata records `lsn_lo` and `lsn_hi`, so a file
//! describes itself without the manifest.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use super::page::Page;
use crate::storage::parquet::writer::{
    ColumnSpec, ColumnType, ColumnValues, ParquetWriter, WriteError, WriterOptions,
};
use crate::storage::stream::adapter::column_values;
use crate::storage::stream::segment::OwnedColumn;
use crate::vm::batch::Value;

/// The predefined stream columns and their Parquet types.
const PREDEFINED: [(&str, ColumnType); 7] = [
    ("timestamp", ColumnType::TimestampNanos),
    ("observed_ts", ColumnType::TimestampNanos),
    ("severity", ColumnType::Int32),
    ("severity_text", ColumnType::Utf8),
    ("facility", ColumnType::Utf8),
    ("message", ColumnType::Utf8),
    ("raw", ColumnType::Utf8),
];

/// What a flushed file holds, for its manifest entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FlushedFile {
    pub(crate) rows: u64,
    pub(crate) bytes: u64,
    pub(crate) observed_ts: Option<(i64, i64)>,
    pub(crate) timestamp: Option<(i64, i64)>,
}

fn field_type(column: &OwnedColumn) -> ColumnType {
    match column {
        OwnedColumn::Int(_) => ColumnType::Int64,
        OwnedColumn::Float(_) => ColumnType::Double,
        OwnedColumn::Bool(_) => ColumnType::Boolean,
        OwnedColumn::Dict { .. } | OwnedColumn::Str(_) | OwnedColumn::Mixed(_) => ColumnType::Utf8,
    }
}

/// The type that holds both.
fn widen(a: ColumnType, b: ColumnType) -> ColumnType {
    match (a, b) {
        _ if a == b => a,
        (ColumnType::Int64, ColumnType::Double) | (ColumnType::Double, ColumnType::Int64) => {
            ColumnType::Double
        }
        _ => ColumnType::Utf8,
    }
}

/// The seven predefined columns, the start of every table schema.
pub(crate) fn predefined_specs() -> Vec<ColumnSpec> {
    PREDEFINED
        .iter()
        .map(|(name, t)| ColumnSpec::new(*name, *t, true))
        .collect()
}

/// Adds field `name` of type `t` to `specs`, widening a field already
/// there. A name of a predefined column is ignored.
pub(crate) fn add_field(specs: &mut Vec<ColumnSpec>, name: &str, t: ColumnType) {
    if PREDEFINED.iter().any(|(p, _)| *p == name) {
        return;
    }
    match specs.iter_mut().find(|s| s.name == name) {
        Some(spec) => spec.column_type = widen(spec.column_type, t),
        None => specs.push(ColumnSpec::new(name, t, true)),
    }
}

/// Adds every field of `page` to `specs`.
pub(crate) fn add_page_fields(specs: &mut Vec<ColumnSpec>, page: &Page) {
    for name in page.segment.field_names() {
        if let Some(column) = page.segment.field(name) {
            add_field(specs, name, field_type(column));
        }
    }
}

/// The file schema for `pages`.
pub(crate) fn schema(pages: &[Page]) -> Vec<ColumnSpec> {
    let mut specs = predefined_specs();
    for page in pages {
        add_page_fields(&mut specs, page);
    }
    specs
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

/// One page's columns in `specs` order.
fn page_columns(page: &Page, specs: &[ColumnSpec]) -> Vec<ColumnValues> {
    let rows = page.segment.len();
    specs
        .iter()
        .map(|spec| {
            let values =
                column_values(&page.segment, &spec.name).unwrap_or_else(|| vec![Value::Null; rows]);
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

/// Writes `pages` (contiguous, oldest first, at least one) to `path` and
/// fsyncs it.
pub(crate) fn write_file(
    path: &Path,
    pages: &[Page],
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
        writer.write_columns(page_columns(page, &specs))?;
    }
    let (out, summary) = writer.finish()?;
    let file = out
        .into_inner()
        .map_err(|e| WriteError::Io(e.into_error()))?;
    file.sync_all()?;
    let observed = bounds(
        pages
            .iter()
            .flat_map(|p| p.segment.observed_ts_ns().iter().copied()),
    );
    let timestamp = bounds(
        pages
            .iter()
            .flat_map(|p| p.segment.event_ts_ns().iter().copied()),
    );
    Ok(FlushedFile {
        rows: summary.num_rows,
        bytes: summary.bytes,
        observed_ts: observed,
        timestamp,
    })
}

#[cfg(test)]
mod tests;
