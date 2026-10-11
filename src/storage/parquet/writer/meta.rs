//! Parquet metadata as [`Value`] trees for [`super::thrift`]: page headers,
//! column metadata, the schema and the footer's `FileMetaData`. Field ids
//! are parquet-format's `parquet.thrift`; fields are listed in id order.
//!
//! Spec: https://github.com/apache/parquet-format/blob/master/src/main/thrift/parquet.thrift

use super::stats::ChunkStats;
use super::{ColumnSpec, ColumnType};
use crate::storage::parquet::thrift::Value;

/// `Encoding` ids.
pub(crate) const ENCODING_PLAIN: i32 = 0;
pub(crate) const ENCODING_RLE: i32 = 3;
pub(crate) const ENCODING_RLE_DICTIONARY: i32 = 8;

const PAGE_TYPE_DATA: i32 = 0;
const PAGE_TYPE_DICTIONARY: i32 = 2;

const TYPE_BOOLEAN: i32 = 0;
const TYPE_INT32: i32 = 1;
const TYPE_INT64: i32 = 2;
const TYPE_FLOAT: i32 = 4;
const TYPE_DOUBLE: i32 = 5;
const TYPE_BYTE_ARRAY: i32 = 6;

const REPETITION_REQUIRED: i32 = 0;
const REPETITION_OPTIONAL: i32 = 1;

const CONVERTED_UTF8: i32 = 0;
const CONVERTED_TIMESTAMP_MICROS: i32 = 10;

fn empty() -> Value {
    Value::Struct(Vec::new())
}

fn string(s: &str) -> Value {
    Value::Binary(s.as_bytes().to_vec())
}

/// Sizes and counts of one page, already range-checked to `i32`.
pub(crate) struct PageSizes {
    pub(crate) uncompressed: i32,
    pub(crate) compressed: i32,
    pub(crate) num_values: i32,
}

/// `PageHeader` for a DATA_PAGE (v1) with RLE definition levels.
pub(crate) fn data_page_header(sizes: &PageSizes, encoding: i32) -> Vec<(i16, Value)> {
    vec![
        (1, Value::I32(PAGE_TYPE_DATA)),
        (2, Value::I32(sizes.uncompressed)),
        (3, Value::I32(sizes.compressed)),
        (
            5,
            Value::Struct(vec![
                (1, Value::I32(sizes.num_values)),
                (2, Value::I32(encoding)),
                (3, Value::I32(ENCODING_RLE)),
                (4, Value::I32(ENCODING_RLE)),
            ]),
        ),
    ]
}

/// `PageHeader` for a PLAIN-encoded DICTIONARY_PAGE.
pub(crate) fn dictionary_page_header(sizes: &PageSizes) -> Vec<(i16, Value)> {
    vec![
        (1, Value::I32(PAGE_TYPE_DICTIONARY)),
        (2, Value::I32(sizes.uncompressed)),
        (3, Value::I32(sizes.compressed)),
        (
            7,
            Value::Struct(vec![
                (1, Value::I32(sizes.num_values)),
                (2, Value::I32(ENCODING_PLAIN)),
            ]),
        ),
    ]
}

pub(crate) fn physical_type(column_type: ColumnType) -> i32 {
    match column_type {
        ColumnType::Boolean => TYPE_BOOLEAN,
        ColumnType::Int32 => TYPE_INT32,
        ColumnType::Int64 | ColumnType::TimestampMicros | ColumnType::TimestampNanos => TYPE_INT64,
        ColumnType::Float => TYPE_FLOAT,
        ColumnType::Double => TYPE_DOUBLE,
        ColumnType::Utf8 => TYPE_BYTE_ARRAY,
    }
}

/// `Statistics` with only the type-ordered `min_value`/`max_value` (5, 6)
/// and `null_count` (3); the deprecated signed `min`/`max` (1, 2) are left
/// out because they are wrong for strings.
fn statistics(stats: &ChunkStats, null_count: i64) -> Value {
    let mut fields = vec![(3, Value::I64(null_count))];
    if let Some(max) = &stats.max {
        fields.push((5, Value::Binary(max.clone())));
    }
    if let Some(min) = &stats.min {
        fields.push((6, Value::Binary(min.clone())));
    }
    Value::Struct(fields)
}

/// Where a column chunk's pages landed and how big they are.
pub(crate) struct ChunkLayout {
    pub(crate) encodings: Vec<i32>,
    pub(crate) codec: i32,
    pub(crate) num_values: i64,
    pub(crate) total_uncompressed_size: i64,
    pub(crate) total_compressed_size: i64,
    pub(crate) data_page_offset: i64,
    pub(crate) dictionary_page_offset: Option<i64>,
}

/// `ColumnChunk` with its `ColumnMetaData` inline (no `file_path`: the
/// data is in this file). `file_offset` is the chunk's first page.
pub(crate) fn column_chunk(
    spec: &ColumnSpec,
    layout: &ChunkLayout,
    stats: &ChunkStats,
    null_count: i64,
) -> Value {
    let mut meta = vec![
        (1, Value::I32(physical_type(spec.column_type))),
        (
            2,
            Value::List(layout.encodings.iter().map(|e| Value::I32(*e)).collect()),
        ),
        (3, Value::List(vec![string(&spec.name)])),
        (4, Value::I32(layout.codec)),
        (5, Value::I64(layout.num_values)),
        (6, Value::I64(layout.total_uncompressed_size)),
        (7, Value::I64(layout.total_compressed_size)),
        (9, Value::I64(layout.data_page_offset)),
    ];
    if let Some(offset) = layout.dictionary_page_offset {
        meta.push((11, Value::I64(offset)));
    }
    meta.push((12, statistics(stats, null_count)));
    let first_page = layout
        .dictionary_page_offset
        .unwrap_or(layout.data_page_offset);
    Value::Struct(vec![(2, Value::I64(first_page)), (3, Value::Struct(meta))])
}

/// `RowGroup`: its column chunks, uncompressed byte size and row count,
/// plus the optional `file_offset` (5) and `total_compressed_size` (6).
pub(crate) fn row_group(
    columns: Vec<Value>,
    total_byte_size: i64,
    num_rows: i64,
    file_offset: i64,
    total_compressed_size: i64,
) -> Value {
    Value::Struct(vec![
        (1, Value::List(columns)),
        (2, Value::I64(total_byte_size)),
        (3, Value::I64(num_rows)),
        (5, Value::I64(file_offset)),
        (6, Value::I64(total_compressed_size)),
    ])
}

/// One flat `SchemaElement` per column, with the legacy `ConvertedType` and
/// the `LogicalType` union for strings and timestamps.
fn schema_element(spec: &ColumnSpec) -> Value {
    let repetition = if spec.nullable {
        REPETITION_OPTIONAL
    } else {
        REPETITION_REQUIRED
    };
    let mut fields = vec![
        (1, Value::I32(physical_type(spec.column_type))),
        (3, Value::I32(repetition)),
        (4, string(&spec.name)),
    ];
    match spec.column_type {
        ColumnType::Utf8 => {
            fields.push((6, Value::I32(CONVERTED_UTF8)));
            // LogicalType.STRING (1): StringType {}
            fields.push((10, Value::Struct(vec![(1, empty())])));
        }
        ColumnType::TimestampMicros => {
            fields.push((6, Value::I32(CONVERTED_TIMESTAMP_MICROS)));
            // LogicalType.TIMESTAMP (8): { isAdjustedToUTC: true, unit: MICROS (2) }
            let unit = Value::Struct(vec![(2, empty())]);
            let timestamp = Value::Struct(vec![(1, Value::Bool(true)), (2, unit)]);
            fields.push((10, Value::Struct(vec![(8, timestamp)])));
        }
        ColumnType::TimestampNanos => {
            // LogicalType.TIMESTAMP (8): { isAdjustedToUTC: false, unit: NANOS (3) }.
            // Not UTC-marked, as DuckDB writes TIMESTAMP_NS: DuckDB has no
            // time-zone-aware nanosecond type and reads a UTC-marked one as
            // microsecond TIMESTAMPTZ, truncating. The values are UTC by
            // convention.
            let unit = Value::Struct(vec![(3, empty())]);
            let timestamp = Value::Struct(vec![(1, Value::Bool(false)), (2, unit)]);
            fields.push((10, Value::Struct(vec![(8, timestamp)])));
        }
        ColumnType::Boolean
        | ColumnType::Int32
        | ColumnType::Int64
        | ColumnType::Float
        | ColumnType::Double => {}
    }
    Value::Struct(fields)
}

/// `FileMetaData`: version 1, the flattened schema (a root group named
/// `schema`, then one leaf per column), row count, row groups, any
/// key-value metadata, writer identity, and a `TypeDefinedOrder` per
/// column so readers trust `min_value`/`max_value`.
pub(crate) fn file_metadata(
    schema: &[ColumnSpec],
    num_rows: i64,
    row_groups: Vec<Value>,
    created_by: &str,
    key_value_metadata: &[(String, String)],
) -> Vec<(i16, Value)> {
    let num_children = i32::try_from(schema.len()).unwrap_or(i32::MAX);
    let root = Value::Struct(vec![(4, string("schema")), (5, Value::I32(num_children))]);
    let elements = std::iter::once(root)
        .chain(schema.iter().map(schema_element))
        .collect();
    // ColumnOrder.TYPE_ORDER (1): TypeDefinedOrder {}
    let orders = schema
        .iter()
        .map(|_| Value::Struct(vec![(1, empty())]))
        .collect();
    let mut fields = vec![
        (1, Value::I32(1)),
        (2, Value::List(elements)),
        (3, Value::I64(num_rows)),
        (4, Value::List(row_groups)),
    ];
    if !key_value_metadata.is_empty() {
        // KeyValue { 1: key, 2: value }
        let pairs = key_value_metadata
            .iter()
            .map(|(k, v)| Value::Struct(vec![(1, string(k)), (2, string(v))]))
            .collect();
        fields.push((5, Value::List(pairs)));
    }
    fields.push((6, string(created_by)));
    fields.push((7, Value::List(orders)));
    fields
}
