//! A table file's columns (ADR 0027): the seven predefined stream columns,
//! then every Tier-3 field any page has, in first-seen order, each of the
//! widest type it took (integers and floats become DOUBLE, any other mix a
//! string). A field named like a predefined column is left out. Shared by
//! the flush and the read side, which widens a table's files the same way.

use super::page::Page;
#[cfg(feature = "ingest")]
use super::page::PageBody;
use crate::storage::parquet::schema::{ColumnSpec, ColumnType};
use crate::storage::stream::segment::OwnedColumn;

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

fn field_type(column: &OwnedColumn) -> ColumnType {
    match column {
        OwnedColumn::Int(_) => ColumnType::Int64,
        OwnedColumn::Float(_) => ColumnType::Double,
        OwnedColumn::Bool(_) => ColumnType::Boolean,
        OwnedColumn::Dict { .. } | OwnedColumn::Str(_) | OwnedColumn::Mixed(_) => ColumnType::Utf8,
    }
}

/// The type that holds both.
pub(super) fn widen(a: ColumnType, b: ColumnType) -> ColumnType {
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
    let Some(segment) = page.lines() else {
        return;
    };
    for name in segment.field_names() {
        if let Some(column) = segment.field(name) {
            add_field(specs, name, field_type(column));
        }
    }
}

/// The file schema for `pages`: a record table's own, or the predefined
/// columns and every field.
#[cfg(feature = "ingest")]
pub(crate) fn schema(pages: &[Page]) -> Vec<ColumnSpec> {
    if let Some(PageBody::Rows(rows)) = pages.first().map(|p| &p.body) {
        return rows.schema().to_vec();
    }
    let mut specs = predefined_specs();
    for page in pages {
        add_page_fields(&mut specs, page);
    }
    specs
}
