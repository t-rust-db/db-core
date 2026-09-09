//! Parquet footer parsing: extracts `FileMetaData` (schema, row groups,
//! column chunks) from the tail of a Parquet file.
//!
//! File layout: `[data ...][metadata][4-byte metadata length][b"PAR1"]`

use crate::column::parquet::thrift::{self, ThriftError, Value};
use std::fmt;

const MAGIC: &[u8; 4] = b"PAR1";
const FOOTER_LEN: usize = 8; // 4-byte length + 4-byte magic

#[derive(Debug)]
pub enum FooterError {
    FileTooShort,
    BadMagic,
    Thrift(ThriftError),
}

impl fmt::Display for FooterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FooterError::FileTooShort => write!(f, "file too short to contain a Parquet footer"),
            FooterError::BadMagic => write!(f, "missing PAR1 magic bytes"),
            FooterError::Thrift(e) => write!(f, "failed to decode footer metadata: {e}"),
        }
    }
}

impl std::error::Error for FooterError {}

impl From<ThriftError> for FooterError {
    fn from(e: ThriftError) -> Self {
        FooterError::Thrift(e)
    }
}

pub type Result<T> = std::result::Result<T, FooterError>;

/// Physical type of a leaf column, `parquet.thrift` `Type` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhysicalType {
    Boolean,
    Int32,
    Int64,
    Int96,
    Float,
    Double,
    ByteArray,
    FixedLenByteArray,
    Unknown(i32),
}

impl From<i32> for PhysicalType {
    fn from(v: i32) -> Self {
        match v {
            0 => PhysicalType::Boolean,
            1 => PhysicalType::Int32,
            2 => PhysicalType::Int64,
            3 => PhysicalType::Int96,
            4 => PhysicalType::Float,
            5 => PhysicalType::Double,
            6 => PhysicalType::ByteArray,
            7 => PhysicalType::FixedLenByteArray,
            other => PhysicalType::Unknown(other),
        }
    }
}

/// `parquet.thrift` `FieldRepetitionType` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repetition {
    Required,
    Optional,
    Repeated,
    Unknown(i32),
}

impl From<i32> for Repetition {
    fn from(v: i32) -> Self {
        match v {
            0 => Repetition::Required,
            1 => Repetition::Optional,
            2 => Repetition::Repeated,
            other => Repetition::Unknown(other),
        }
    }
}

/// One entry of the flattened `FileMetaData.schema` list.
#[derive(Debug, Clone, PartialEq)]
pub struct SchemaElement {
    pub name: String,
    /// `None` for the schema root and other group (non-leaf) nodes.
    pub physical_type: Option<PhysicalType>,
    pub repetition: Option<Repetition>,
    pub num_children: Option<i32>,
    /// Fixed byte width for a `FIXED_LEN_BYTE_ARRAY` leaf; `None` otherwise.
    pub type_length: Option<i32>,
    /// Old-style (`ConvertedType`) logical type annotation, e.g. `DECIMAL`
    /// or `TIMESTAMP_MICROS`. Real writers (DuckDB, pyarrow, Spark) still
    /// set this alongside the newer `LogicalType` union, which this reader
    /// doesn't parse.
    pub converted_type: Option<ConvertedType>,
    /// `DECIMAL`'s scale (digits after the decimal point).
    pub scale: Option<i32>,
    /// `DECIMAL`'s precision (total significant digits).
    pub precision: Option<i32>,
}

/// `parquet.thrift` `ConvertedType` enum (values relevant to this reader).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvertedType {
    Decimal,
    TimestampMillis,
    TimestampMicros,
    Other(i32),
}

impl From<i32> for ConvertedType {
    fn from(v: i32) -> Self {
        match v {
            5 => ConvertedType::Decimal,
            9 => ConvertedType::TimestampMillis,
            10 => ConvertedType::TimestampMicros,
            other => ConvertedType::Other(other),
        }
    }
}

/// `parquet.thrift` `CompressionCodec` enum (values relevant to this reader).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Uncompressed,
    Snappy,
    Zstd,
    Other(i32),
}

impl From<i32> for Codec {
    fn from(v: i32) -> Self {
        match v {
            0 => Codec::Uncompressed,
            1 => Codec::Snappy,
            6 => Codec::Zstd,
            other => Codec::Other(other),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnMetaData {
    pub physical_type: PhysicalType,
    pub codec: Codec,
    pub path_in_schema: Vec<String>,
    pub num_values: i64,
    pub total_uncompressed_size: i64,
    pub total_compressed_size: i64,
    pub data_page_offset: i64,
    pub dictionary_page_offset: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnChunk {
    pub file_offset: i64,
    pub meta_data: Option<ColumnMetaData>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RowGroup {
    pub columns: Vec<ColumnChunk>,
    pub total_byte_size: i64,
    pub num_rows: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileMetaData {
    pub version: i32,
    pub schema: Vec<SchemaElement>,
    pub num_rows: i64,
    pub row_groups: Vec<RowGroup>,
    pub created_by: Option<String>,
}

/// Locate and decode the `FileMetaData` footer within a full Parquet file
/// buffer (typically a memory-mapped file).
pub fn parse_footer(file: &[u8]) -> Result<FileMetaData> {
    if file.len() < FOOTER_LEN {
        return Err(FooterError::FileTooShort);
    }
    let tail = &file[file.len() - FOOTER_LEN..];
    if &tail[4..8] != MAGIC {
        return Err(FooterError::BadMagic);
    }
    let metadata_len = u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]) as usize;

    let metadata_start = file
        .len()
        .checked_sub(FOOTER_LEN + metadata_len)
        .ok_or(FooterError::FileTooShort)?;
    let metadata_bytes = &file[metadata_start..file.len() - FOOTER_LEN];

    let (fields, _) = thrift::decode_struct(metadata_bytes)?;
    Ok(parse_file_meta_data(&fields))
}

fn parse_file_meta_data(fields: &[(i16, Value)]) -> FileMetaData {
    let get = |id: i16| fields.iter().find(|(fid, _)| *fid == id).map(|(_, v)| v);

    let version = get(1).and_then(Value::as_i32).unwrap_or(0);
    let schema = get(2)
        .and_then(Value::as_list)
        .map(|items| items.iter().map(parse_schema_element).collect())
        .unwrap_or_default();
    let num_rows = get(3).and_then(Value::as_i64).unwrap_or(0);
    let row_groups = get(4)
        .and_then(Value::as_list)
        .map(|items| items.iter().map(parse_row_group).collect())
        .unwrap_or_default();
    let created_by = get(6).and_then(Value::as_str).map(str::to_string);

    FileMetaData {
        version,
        schema,
        num_rows,
        row_groups,
        created_by,
    }
}

fn parse_schema_element(v: &Value) -> SchemaElement {
    let name = v.field(4).and_then(Value::as_str).unwrap_or("").to_string();
    let physical_type = v.field(1).and_then(Value::as_i32).map(PhysicalType::from);
    let type_length = v.field(2).and_then(Value::as_i32);
    let repetition = v.field(3).and_then(Value::as_i32).map(Repetition::from);
    let num_children = v.field(5).and_then(Value::as_i32);
    let converted_type = v.field(6).and_then(Value::as_i32).map(ConvertedType::from);
    let scale = v.field(7).and_then(Value::as_i32);
    let precision = v.field(8).and_then(Value::as_i32);
    SchemaElement {
        name,
        physical_type,
        repetition,
        num_children,
        type_length,
        converted_type,
        scale,
        precision,
    }
}

fn parse_row_group(v: &Value) -> RowGroup {
    let columns = v
        .field(1)
        .and_then(Value::as_list)
        .map(|items| items.iter().map(parse_column_chunk).collect())
        .unwrap_or_default();
    let total_byte_size = v.field(2).and_then(Value::as_i64).unwrap_or(0);
    let num_rows = v.field(3).and_then(Value::as_i64).unwrap_or(0);
    RowGroup {
        columns,
        total_byte_size,
        num_rows,
    }
}

fn parse_column_chunk(v: &Value) -> ColumnChunk {
    let file_offset = v.field(2).and_then(Value::as_i64).unwrap_or(0);
    let meta_data = v.field(3).map(parse_column_meta_data);
    ColumnChunk {
        file_offset,
        meta_data,
    }
}

fn parse_column_meta_data(v: &Value) -> ColumnMetaData {
    let physical_type = v
        .field(1)
        .and_then(Value::as_i32)
        .map(PhysicalType::from)
        .unwrap_or(PhysicalType::Unknown(-1));
    let codec = v
        .field(4)
        .and_then(Value::as_i32)
        .map(Codec::from)
        .unwrap_or(Codec::Uncompressed);
    let path_in_schema = v
        .field(3)
        .and_then(Value::as_list)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let num_values = v.field(5).and_then(Value::as_i64).unwrap_or(0);
    let total_uncompressed_size = v.field(6).and_then(Value::as_i64).unwrap_or(0);
    let total_compressed_size = v.field(7).and_then(Value::as_i64).unwrap_or(0);
    let data_page_offset = v.field(9).and_then(Value::as_i64).unwrap_or(0);
    let dictionary_page_offset = v.field(11).and_then(Value::as_i64);
    ColumnMetaData {
        physical_type,
        codec,
        path_in_schema,
        num_values,
        total_uncompressed_size,
        total_compressed_size,
        data_page_offset,
        dictionary_page_offset,
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;

    /// Minimal Thrift Compact Protocol struct encoder, just enough to build
    /// synthetic `FileMetaData` fixtures for these tests.
    struct StructWriter {
        buf: Vec<u8>,
        last_field_id: i16,
    }

    impl StructWriter {
        fn new() -> Self {
            StructWriter {
                buf: Vec::new(),
                last_field_id: 0,
            }
        }

        fn write_varint(&mut self, mut v: u64) {
            loop {
                let mut b = (v & 0x7f) as u8;
                v >>= 7;
                if v != 0 {
                    b |= 0x80;
                }
                self.buf.push(b);
                if v == 0 {
                    break;
                }
            }
        }

        fn zigzag(v: i64) -> u64 {
            ((v << 1) ^ (v >> 63)) as u64
        }

        fn field_header(&mut self, field_id: i16, ctype: u8) {
            let delta = field_id - self.last_field_id;
            assert!(
                (1..=15).contains(&delta),
                "test helper only supports small positive deltas"
            );
            self.buf.push(((delta as u8) << 4) | ctype);
            self.last_field_id = field_id;
        }

        fn i32_field(&mut self, field_id: i16, v: i32) {
            self.field_header(field_id, 0x05);
            self.write_varint(Self::zigzag(v as i64));
        }

        fn i64_field(&mut self, field_id: i16, v: i64) {
            self.field_header(field_id, 0x06);
            self.write_varint(Self::zigzag(v));
        }

        fn string_field(&mut self, field_id: i16, s: &str) {
            self.field_header(field_id, 0x08);
            self.write_varint(s.len() as u64);
            self.buf.extend_from_slice(s.as_bytes());
        }

        fn struct_field(&mut self, field_id: i16, inner: Vec<u8>) {
            self.field_header(field_id, 0x0c);
            self.buf.extend_from_slice(&inner);
        }

        fn list_of_structs_field(&mut self, field_id: i16, items: Vec<Vec<u8>>) {
            self.field_header(field_id, 0x09);
            let n = items.len();
            if n < 15 {
                self.buf.push(((n as u8) << 4) | 0x0c);
            } else {
                self.buf.push((15u8 << 4) | 0x0c);
                self.write_varint(n as u64);
            }
            for item in items {
                self.buf.extend_from_slice(&item);
            }
        }

        fn finish(mut self) -> Vec<u8> {
            self.buf.push(0x00); // STOP
            self.buf
        }
    }

    fn build_schema_element(name: &str, physical_type: i32, repetition: i32) -> Vec<u8> {
        let mut w = StructWriter::new();
        w.i32_field(1, physical_type);
        w.i32_field(3, repetition);
        w.string_field(4, name);
        w.finish()
    }

    fn build_column_meta_data(
        physical_type: i32,
        path: &str,
        num_values: i64,
        data_page_offset: i64,
    ) -> Vec<u8> {
        let mut w = StructWriter::new();
        w.i32_field(1, physical_type);
        // field 3: path_in_schema (list<string>) — build manually since the
        // writer only special-cases list<struct>.
        w.field_header(3, 0x09);
        w.buf.push((1u8 << 4) | 0x08); // size=1, elem type=binary
        w.write_varint(path.len() as u64);
        w.buf.extend_from_slice(path.as_bytes());
        w.i64_field(5, num_values);
        w.i64_field(9, data_page_offset);
        w.finish()
    }

    fn build_column_chunk(file_offset: i64, meta_data: Vec<u8>) -> Vec<u8> {
        let mut w = StructWriter::new();
        w.i64_field(2, file_offset);
        w.struct_field(3, meta_data);
        w.finish()
    }

    fn build_row_group(columns: Vec<Vec<u8>>, total_byte_size: i64, num_rows: i64) -> Vec<u8> {
        let mut w = StructWriter::new();
        w.list_of_structs_field(1, columns);
        w.i64_field(2, total_byte_size);
        w.i64_field(3, num_rows);
        w.finish()
    }

    fn build_file_metadata(
        schema: Vec<Vec<u8>>,
        num_rows: i64,
        row_groups: Vec<Vec<u8>>,
        created_by: &str,
    ) -> Vec<u8> {
        let mut w = StructWriter::new();
        w.i32_field(1, 1); // version
        w.list_of_structs_field(2, schema);
        w.i64_field(3, num_rows);
        w.list_of_structs_field(4, row_groups);
        w.string_field(6, created_by);
        w.finish()
    }

    fn wrap_as_file(metadata: Vec<u8>) -> Vec<u8> {
        let mut file = Vec::new();
        file.extend_from_slice(MAGIC); // leading magic
        let metadata_len = metadata.len() as u32;
        file.extend_from_slice(&metadata);
        file.extend_from_slice(&metadata_len.to_le_bytes());
        file.extend_from_slice(MAGIC);
        file
    }

    #[test]
    fn parses_single_row_group_footer() {
        let root = build_schema_element("schema", 0, 0);
        let col = build_schema_element("id", 2 /* INT64 */, 1 /* OPTIONAL */);

        let col_meta = build_column_meta_data(2, "id", 100, 4);
        let chunk = build_column_chunk(4, col_meta);
        let row_group = build_row_group(vec![chunk], 800, 100);

        let metadata = build_file_metadata(vec![root, col], 100, vec![row_group], "column-rs");
        let file = wrap_as_file(metadata);

        let parsed = parse_footer(&file).unwrap();
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.num_rows, 100);
        assert_eq!(parsed.created_by.as_deref(), Some("column-rs"));
        assert_eq!(parsed.schema.len(), 2);
        assert_eq!(parsed.schema[1].name, "id");
        assert_eq!(parsed.schema[1].physical_type, Some(PhysicalType::Int64));
        assert_eq!(parsed.schema[1].repetition, Some(Repetition::Optional));

        assert_eq!(parsed.row_groups.len(), 1);
        let rg = &parsed.row_groups[0];
        assert_eq!(rg.num_rows, 100);
        assert_eq!(rg.total_byte_size, 800);
        assert_eq!(rg.columns.len(), 1);
        let meta = rg.columns[0].meta_data.as_ref().unwrap();
        assert_eq!(meta.physical_type, PhysicalType::Int64);
        assert_eq!(meta.path_in_schema, vec!["id".to_string()]);
        assert_eq!(meta.num_values, 100);
        assert_eq!(meta.data_page_offset, 4);
    }

    #[test]
    fn parses_multiple_row_groups() {
        let root = build_schema_element("schema", 0, 0);
        let col = build_schema_element("v", 5 /* DOUBLE */, 1);

        let rg1_meta = build_column_meta_data(5, "v", 10, 4);
        let rg1_chunk = build_column_chunk(4, rg1_meta);
        let rg1 = build_row_group(vec![rg1_chunk], 80, 10);

        let rg2_meta = build_column_meta_data(5, "v", 20, 200);
        let rg2_chunk = build_column_chunk(200, rg2_meta);
        let rg2 = build_row_group(vec![rg2_chunk], 160, 20);

        let metadata = build_file_metadata(vec![root, col], 30, vec![rg1, rg2], "duckdb");
        let file = wrap_as_file(metadata);

        let parsed = parse_footer(&file).unwrap();
        assert_eq!(parsed.row_groups.len(), 2);
        assert_eq!(parsed.row_groups[0].num_rows, 10);
        assert_eq!(parsed.row_groups[1].num_rows, 20);
        assert_eq!(parsed.num_rows, 30);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut file = wrap_as_file(build_file_metadata(vec![], 0, vec![], "x"));
        let len = file.len();
        file[len - 1] = b'X'; // corrupt trailing magic
        assert!(matches!(parse_footer(&file), Err(FooterError::BadMagic)));
    }

    #[test]
    fn rejects_truncated_file() {
        let file = vec![0u8; 4]; // shorter than the 8-byte footer
        assert!(matches!(
            parse_footer(&file),
            Err(FooterError::FileTooShort)
        ));
    }
}
