//! Parquet footer parsing: extracts `FileMetaData` (schema, row groups,
//! column chunks) from the tail of a Parquet file.
//!
//! File layout: `[data ...][metadata][4-byte metadata length][b"PAR1"]`

use crate::storage::parquet::page::Encoding;
use crate::storage::parquet::schema_tree;
use crate::storage::parquet::thrift::{self, ThriftError, Value};
use std::fmt;

pub(super) const MAGIC: &[u8; 4] = b"PAR1";
pub(super) const FOOTER_LEN: usize = 8; // 4-byte length + 4-byte magic

/// Why a Parquet footer could not be read.
#[derive(Debug)]
pub enum FooterError {
    /// The file is shorter than the footer, or than the metadata length
    /// the footer declares.
    FileTooShort,
    /// The file does not end in the `PAR1` magic.
    BadMagic,
    /// The metadata is not valid Thrift Compact Protocol.
    Thrift(ThriftError),
    /// The flattened schema list does not describe a valid tree.
    InvalidSchema(&'static str),
}

impl fmt::Display for FooterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FooterError::FileTooShort => write!(f, "file too short to contain a Parquet footer"),
            FooterError::BadMagic => write!(f, "missing PAR1 magic bytes"),
            FooterError::Thrift(e) => write!(f, "failed to decode footer metadata: {e}"),
            FooterError::InvalidSchema(why) => write!(f, "invalid footer schema: {why}"),
        }
    }
}

impl std::error::Error for FooterError {}

impl From<ThriftError> for FooterError {
    fn from(e: ThriftError) -> Self {
        FooterError::Thrift(e)
    }
}

/// Result of footer parsing.
pub type Result<T> = std::result::Result<T, FooterError>;

/// Physical type of a leaf column, `parquet.thrift` `Type` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhysicalType {
    /// `BOOLEAN` (0).
    Boolean,
    /// `INT32` (1).
    Int32,
    /// `INT64` (2).
    Int64,
    /// `INT96` (3), the legacy timestamp encoding.
    Int96,
    /// `FLOAT` (4).
    Float,
    /// `DOUBLE` (5).
    Double,
    /// `BYTE_ARRAY` (6).
    ByteArray,
    /// `FIXED_LEN_BYTE_ARRAY` (7).
    FixedLenByteArray,
    /// Any other value, as read.
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
    /// `REQUIRED` (0): exactly one value.
    Required,
    /// `OPTIONAL` (1): zero or one value.
    Optional,
    /// `REPEATED` (2): zero or more values.
    Repeated,
    /// Any other value, as read.
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
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SchemaElement {
    /// Field name (field 4); empty when absent.
    pub name: String,
    /// `None` for the schema root and other group (non-leaf) nodes.
    pub physical_type: Option<PhysicalType>,
    /// Repetition (field 3); `None` for the root of most writers.
    pub repetition: Option<Repetition>,
    /// Number of children (field 5) for a group node; `None` for a leaf.
    pub num_children: Option<i32>,
    /// Fixed byte width for a `FIXED_LEN_BYTE_ARRAY` leaf; `None` otherwise.
    pub type_length: Option<i32>,
    /// Old-style (`ConvertedType`) logical type annotation, e.g. `DECIMAL`
    /// or `TIMESTAMP_MICROS`. Real writers (DuckDB, pyarrow, Spark) still
    /// set this alongside the newer [`LogicalType`] union.
    pub converted_type: Option<ConvertedType>,
    /// `DECIMAL`'s scale (digits after the decimal point).
    pub scale: Option<i32>,
    /// `DECIMAL`'s precision (total significant digits).
    pub precision: Option<i32>,
    /// A `TIMESTAMP` logical type's units per second (1 000, 1 000 000 or
    /// 1 000 000 000), from the `LogicalType` union (field 10). Nanosecond
    /// timestamps have no `ConvertedType`, so this is the only place their
    /// unit is recorded.
    pub timestamp_units: Option<i64>,
    /// The `LogicalType` union (field 10), for describing the file. The
    /// reader decodes by `converted_type` and `timestamp_units`.
    pub logical_type: Option<LogicalType>,
}

/// `parquet.thrift` `LogicalType` union: what a physical value means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalType {
    /// `STRING` (1).
    String,
    /// `MAP` (2).
    Map,
    /// `LIST` (3).
    List,
    /// `ENUM` (4).
    Enum,
    /// `DECIMAL` (5).
    Decimal {
        /// Digits after the decimal point.
        scale: i32,
        /// Total significant digits.
        precision: i32,
    },
    /// `DATE` (6).
    Date,
    /// `TIME` (7).
    Time {
        /// Units per second (1 000, 1 000 000 or 1 000 000 000), if known.
        units: Option<i64>,
        /// `isAdjustedToUTC`.
        utc: bool,
    },
    /// `TIMESTAMP` (8).
    Timestamp {
        /// Units per second (1 000, 1 000 000 or 1 000 000 000), if known.
        units: Option<i64>,
        /// `isAdjustedToUTC`: an instant, not a local wall-clock time.
        utc: bool,
    },
    /// `INTEGER` (10).
    Integer {
        /// 8, 16, 32 or 64.
        bit_width: i32,
        /// Whether the value is signed.
        signed: bool,
    },
    /// `UNKNOWN` (11): always null.
    Unknown,
    /// `JSON` (12).
    Json,
    /// `BSON` (13).
    Bson,
    /// `UUID` (14).
    Uuid,
    /// `FLOAT16` (15).
    Float16,
    /// Any other member of the union, by field id.
    Other(i16),
}

/// `parquet.thrift` `ConvertedType` enum (values relevant to this reader).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvertedType {
    /// `DECIMAL` (5).
    Decimal,
    /// `TIMESTAMP_MILLIS` (9).
    TimestampMillis,
    /// `TIMESTAMP_MICROS` (10).
    TimestampMicros,
    /// `UINT_8`..`UINT_64`: an INT32/INT64 holding an unsigned value's
    /// bit pattern.
    Uint8,
    /// `UINT_16` (12).
    Uint16,
    /// `UINT_32` (13).
    Uint32,
    /// `UINT_64` (14).
    Uint64,
    /// Any other value, as read.
    Other(i32),
}

impl From<i32> for ConvertedType {
    fn from(v: i32) -> Self {
        match v {
            5 => ConvertedType::Decimal,
            9 => ConvertedType::TimestampMillis,
            10 => ConvertedType::TimestampMicros,
            11 => ConvertedType::Uint8,
            12 => ConvertedType::Uint16,
            13 => ConvertedType::Uint32,
            14 => ConvertedType::Uint64,
            other => ConvertedType::Other(other),
        }
    }
}

/// `parquet.thrift` `CompressionCodec` enum (values relevant to this reader).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// `UNCOMPRESSED` (0).
    Uncompressed,
    /// `SNAPPY` (1).
    Snappy,
    /// `ZSTD` (6).
    Zstd,
    /// Any other codec, as read.
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

/// `parquet.thrift` `ColumnMetaData`: where one column chunk lives and how
/// it is encoded. Offsets and sizes are untrusted and unchecked here.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnMetaData {
    /// Physical type (field 1); `Unknown(-1)` when absent.
    pub physical_type: PhysicalType,
    /// Encodings used by the chunk's pages, levels included (field 2).
    pub encodings: Vec<Encoding>,
    /// Compression codec (field 4); `Uncompressed` when absent.
    pub codec: Codec,
    /// Path from the schema root to this leaf (field 3).
    pub path_in_schema: Vec<String>,
    /// Number of values, nulls included (field 5).
    pub num_values: i64,
    /// Uncompressed size of the chunk's pages in bytes (field 6).
    pub total_uncompressed_size: i64,
    /// Compressed size of the chunk's pages in bytes (field 7).
    pub total_compressed_size: i64,
    /// File offset of the first data page (field 9).
    pub data_page_offset: i64,
    /// File offset of the dictionary page, if any (field 11).
    pub dictionary_page_offset: Option<i64>,
    /// Row-group pruning statistics for this column chunk, if the writer
    /// included them. `None` means "no statistics" -- callers must treat
    /// that as "cannot prune", never as an error (writers are free to omit
    /// them, and older files predate `min_value`/`max_value`).
    pub statistics: Option<Statistics>,
}

/// `parquet.thrift` `Statistics` struct: min/max bounds and counts for one
/// column chunk, as written by the file's own encoder. These are untrusted
/// file content (ADR-0000): a hostile or buggy writer can lie, so any use
/// of them for row-group pruning must be correctness-neutral -- a wrong
/// statistic may cost time, never change results.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Statistics {
    /// Minimum value, raw bytes in the column's physical-type encoding.
    /// Prefers the newer `min_value` (field 6) over the deprecated `min`
    /// (field 2), which used unsigned byte-comparison for signed integers.
    pub min: Option<Vec<u8>>,
    /// Maximum value, raw bytes in the column's physical-type encoding.
    /// Prefers the newer `max_value` (field 5) over the deprecated `max`
    /// (field 1).
    pub max: Option<Vec<u8>>,
    /// Number of nulls (field 3).
    pub null_count: Option<i64>,
    /// Number of distinct values (field 4).
    pub distinct_count: Option<i64>,
    /// Whether `min` and `max` came from `min_value` / `max_value`, whose
    /// BYTE_ARRAY order is defined as unsigned lexicographic. The
    /// deprecated `min` / `max` were compared signed by some writers, so a
    /// blob bound from them is not trusted for pruning (#868).
    pub bytes_ordered: bool,
}

/// `parquet.thrift` `ColumnChunk`: one column of one row group.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnChunk {
    /// Byte offset of the chunk's metadata copy in the file (field 2).
    pub file_offset: i64,
    /// The chunk's metadata (field 3), which real writers always inline.
    pub meta_data: Option<ColumnMetaData>,
}

/// `parquet.thrift` `RowGroup`: a horizontal slice of the table.
#[derive(Debug, Clone, PartialEq)]
pub struct RowGroup {
    /// One chunk per leaf column, in schema order (field 1).
    pub columns: Vec<ColumnChunk>,
    /// Total uncompressed byte size of the row group (field 2).
    pub total_byte_size: i64,
    /// Number of rows (field 3).
    pub num_rows: i64,
}

/// `parquet.thrift` `FileMetaData`, the decoded footer.
#[derive(Debug, Clone, PartialEq)]
pub struct FileMetaData {
    /// Format version (field 1).
    pub version: i32,
    /// The flattened schema, root first (field 2). [`parse_footer`] has
    /// checked it forms a tree (see [`schema_tree::check_schema`]).
    pub schema: Vec<SchemaElement>,
    /// Total number of rows (field 3).
    pub num_rows: i64,
    /// Row groups in file order (field 4).
    pub row_groups: Vec<RowGroup>,
    /// The writer's identification string (field 6).
    pub created_by: Option<String>,
    /// `key_value_metadata` (field 5): key and optional value, in order.
    pub key_value_metadata: Vec<(String, Option<String>)>,
}

/// Locate and decode the `FileMetaData` footer within a full Parquet file
/// buffer (typically a memory-mapped file). The input is untrusted: any
/// malformed footer is an `Err`, never a panic.
pub fn parse_footer(file: &[u8]) -> Result<FileMetaData> {
    let tail_start = file
        .len()
        .checked_sub(FOOTER_LEN)
        .ok_or(FooterError::FileTooShort)?;
    let tail: [u8; FOOTER_LEN] = file
        .get(tail_start..)
        .and_then(|t| t.try_into().ok())
        .ok_or(FooterError::FileTooShort)?;
    let [l0, l1, l2, l3, magic @ ..] = tail;
    if magic != *MAGIC {
        return Err(FooterError::BadMagic);
    }
    let metadata_len = usize::try_from(u32::from_le_bytes([l0, l1, l2, l3]))
        .map_err(|_| FooterError::FileTooShort)?;

    let metadata_start = tail_start
        .checked_sub(metadata_len)
        .ok_or(FooterError::FileTooShort)?;
    let metadata_bytes = file
        .get(metadata_start..tail_start)
        .ok_or(FooterError::FileTooShort)?;

    let (fields, _) = thrift::decode_struct(metadata_bytes)?;
    let metadata = parse_file_meta_data(&fields);
    schema_tree::check_schema(&metadata.schema)?;
    Ok(metadata)
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
    let key_value_metadata = get(5)
        .and_then(Value::as_list)
        .map(|items| {
            items
                .iter()
                .filter_map(|kv| {
                    let key = kv.field(1).and_then(Value::as_str)?.to_string();
                    let value = kv.field(2).and_then(Value::as_str).map(str::to_string);
                    Some((key, value))
                })
                .collect()
        })
        .unwrap_or_default();

    FileMetaData {
        version,
        schema,
        num_rows,
        row_groups,
        created_by,
        key_value_metadata,
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
    // LogicalType (10) is a union; TIMESTAMP is its field 8, a
    // TimestampType whose `unit` (2) is the TimeUnit union MILLIS (1),
    // MICROS (2) or NANOS (3).
    let timestamp_units = v.field(10).and_then(|l| l.field(8)).and_then(time_units);
    let logical_type = v.field(10).and_then(parse_logical_type);
    SchemaElement {
        name,
        physical_type,
        repetition,
        num_children,
        type_length,
        converted_type,
        scale,
        precision,
        timestamp_units,
        logical_type,
    }
}

/// A TimeType or TimestampType's `unit` (2), the TimeUnit union, as units
/// per second.
fn time_units(t: &Value) -> Option<i64> {
    let unit = t.field(2).and_then(Value::as_struct)?;
    match unit.first()?.0 {
        1 => Some(1_000),
        2 => Some(1_000_000),
        3 => Some(1_000_000_000),
        _ => None,
    }
}

/// The `LogicalType` union: its one set field names the member.
fn parse_logical_type(v: &Value) -> Option<LogicalType> {
    let (id, member) = v.as_struct()?.first()?;
    let int = |field| member.field(field).and_then(Value::as_i32).unwrap_or(0);
    let utc = member.field(1).and_then(Value::as_bool).unwrap_or(false);
    Some(match id {
        1 => LogicalType::String,
        2 => LogicalType::Map,
        3 => LogicalType::List,
        4 => LogicalType::Enum,
        5 => LogicalType::Decimal {
            scale: int(1),
            precision: int(2),
        },
        6 => LogicalType::Date,
        7 => LogicalType::Time {
            units: time_units(member),
            utc,
        },
        8 => LogicalType::Timestamp {
            units: time_units(member),
            utc,
        },
        10 => LogicalType::Integer {
            bit_width: int(1),
            signed: member.field(2).and_then(Value::as_bool).unwrap_or(false),
        },
        11 => LogicalType::Unknown,
        12 => LogicalType::Json,
        13 => LogicalType::Bson,
        14 => LogicalType::Uuid,
        15 => LogicalType::Float16,
        other => LogicalType::Other(*other),
    })
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
    let encodings = v
        .field(2)
        .and_then(Value::as_list)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_i32)
                .map(Encoding::from)
                .collect()
        })
        .unwrap_or_default();
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
    let statistics = v.field(12).map(parse_statistics);
    ColumnMetaData {
        physical_type,
        encodings,
        codec,
        path_in_schema,
        num_values,
        total_uncompressed_size,
        total_compressed_size,
        data_page_offset,
        dictionary_page_offset,
        statistics,
    }
}

fn parse_statistics(v: &Value) -> Statistics {
    let min = v
        .field(6)
        .or_else(|| v.field(2))
        .and_then(Value::as_binary)
        .map(<[u8]>::to_vec);
    let max = v
        .field(5)
        .or_else(|| v.field(1))
        .and_then(Value::as_binary)
        .map(<[u8]>::to_vec);
    let null_count = v.field(3).and_then(Value::as_i64);
    let distinct_count = v.field(4).and_then(Value::as_i64);
    Statistics {
        min,
        max,
        null_count,
        distinct_count,
        bytes_ordered: v.field(6).is_some() && v.field(5).is_some(),
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

        fn binary_field(&mut self, field_id: i16, bytes: &[u8]) {
            self.field_header(field_id, 0x08);
            self.write_varint(bytes.len() as u64);
            self.buf.extend_from_slice(bytes);
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

    fn build_statistics(
        min_value: Option<&[u8]>,
        max_value: Option<&[u8]>,
        null_count: Option<i64>,
        distinct_count: Option<i64>,
    ) -> Vec<u8> {
        let mut w = StructWriter::new();
        if let Some(nc) = null_count {
            w.i64_field(3, nc);
        }
        if let Some(dc) = distinct_count {
            w.i64_field(4, dc);
        }
        if let Some(max) = max_value {
            w.binary_field(5, max);
        }
        if let Some(min) = min_value {
            w.binary_field(6, min);
        }
        w.finish()
    }

    fn build_column_meta_data_with_stats(
        physical_type: i32,
        path: &str,
        num_values: i64,
        data_page_offset: i64,
        statistics: Vec<u8>,
    ) -> Vec<u8> {
        let mut w = StructWriter::new();
        w.i32_field(1, physical_type);
        w.field_header(3, 0x09);
        w.buf.push((1u8 << 4) | 0x08);
        w.write_varint(path.len() as u64);
        w.buf.extend_from_slice(path.as_bytes());
        w.i64_field(5, num_values);
        w.i64_field(9, data_page_offset);
        w.struct_field(12, statistics);
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

    #[test]
    fn parses_statistics_from_new_style_min_max_value_fields() {
        let root = build_schema_element("schema", 0, 0);
        let col = build_schema_element("amount", 5 /* DOUBLE */, 1);

        let stats = build_statistics(
            Some(&0.0f64.to_le_bytes()),
            Some(&9999.85f64.to_le_bytes()),
            Some(0),
            Some(42),
        );
        let col_meta = build_column_meta_data_with_stats(5, "amount", 100, 4, stats);
        let chunk = build_column_chunk(4, col_meta);
        let row_group = build_row_group(vec![chunk], 800, 100);
        let metadata = build_file_metadata(vec![root, col], 100, vec![row_group], "column-rs");
        let file = wrap_as_file(metadata);

        let parsed = parse_footer(&file).unwrap();
        let meta = parsed.row_groups[0].columns[0].meta_data.as_ref().unwrap();
        let statistics = meta.statistics.as_ref().unwrap();
        assert_eq!(statistics.min, Some(0.0f64.to_le_bytes().to_vec()));
        assert_eq!(statistics.max, Some(9999.85f64.to_le_bytes().to_vec()));
        assert_eq!(statistics.null_count, Some(0));
        assert_eq!(statistics.distinct_count, Some(42));
        assert!(statistics.bytes_ordered);
    }

    #[test]
    fn parses_statistics_from_deprecated_min_max_fields() {
        let root = build_schema_element("schema", 0, 0);
        let col = build_schema_element("region", 6 /* BYTE_ARRAY */, 1);

        let mut w = StructWriter::new();
        w.binary_field(1, b"west");
        w.binary_field(2, b"east");
        let stats = w.finish();
        let col_meta = build_column_meta_data_with_stats(6, "region", 100, 4, stats);
        let chunk = build_column_chunk(4, col_meta);
        let row_group = build_row_group(vec![chunk], 800, 100);
        let metadata = build_file_metadata(vec![root, col], 100, vec![row_group], "column-rs");
        let file = wrap_as_file(metadata);

        let parsed = parse_footer(&file).unwrap();
        let meta = parsed.row_groups[0].columns[0].meta_data.as_ref().unwrap();
        let statistics = meta.statistics.as_ref().unwrap();
        assert_eq!(statistics.min, Some(b"east".to_vec()));
        assert_eq!(statistics.max, Some(b"west".to_vec()));
        assert!(!statistics.bytes_ordered);
        assert_eq!(statistics.null_count, None);
        assert_eq!(statistics.distinct_count, None);
    }

    #[test]
    fn missing_statistics_field_parses_as_none() {
        let root = build_schema_element("schema", 0, 0);
        let col = build_schema_element("id", 2, 1);

        let col_meta = build_column_meta_data(2, "id", 100, 4);
        let chunk = build_column_chunk(4, col_meta);
        let row_group = build_row_group(vec![chunk], 800, 100);
        let metadata = build_file_metadata(vec![root, col], 100, vec![row_group], "column-rs");
        let file = wrap_as_file(metadata);

        let parsed = parse_footer(&file).unwrap();
        let meta = parsed.row_groups[0].columns[0].meta_data.as_ref().unwrap();
        assert!(meta.statistics.is_none());
    }
}

/// #657: corrupt footers from the real fixtures. Every case must come back
/// as `Ok` or `Err` -- a panic, stack overflow or huge allocation fails the
/// test run.
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::arithmetic_side_effects
)]
mod corruption_tests {
    use super::*;
    use crate::storage::parquet::parquet_file::ParquetFile;
    use crate::storage::parquet::schema_tree::build_schema_tree;

    const FIXTURES: &[&str] = &[
        "nested_map.parquet",
        "nested_shapes.parquet",
        "production.parquet",
        "production_snappy.parquet",
        "production_uncompressed.parquet",
    ];

    fn fixtures() -> Vec<(&'static str, Vec<u8>)> {
        FIXTURES
            .iter()
            .map(|name| {
                let path = format!("tests/fixtures/parquet/{name}");
                (*name, std::fs::read(&path).expect(&path))
            })
            .collect()
    }

    /// Where the metadata starts, and its bytes.
    fn metadata(file: &[u8]) -> (usize, &[u8]) {
        let tail = file.len() - FOOTER_LEN;
        let len = u32::from_le_bytes(file[tail..tail + 4].try_into().unwrap()) as usize;
        (tail - len, &file[tail - len..tail])
    }

    fn wrap(metadata: &[u8]) -> Vec<u8> {
        let mut file = MAGIC.to_vec();
        file.extend_from_slice(metadata);
        file.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
        file.extend_from_slice(MAGIC);
        file
    }

    /// Parse and, on success, build the tree and open the file the way the
    /// reader does: none of it may panic. True when the footer parsed.
    fn exercise(file: &[u8]) -> bool {
        match parse_footer(file) {
            Ok(meta) => {
                let tree = build_schema_tree(&meta.schema);
                assert!(tree.leaves().len() <= meta.schema.len());
                if let Ok(f) = ParquetFile::open(file) {
                    assert!(f.fields().len() <= meta.schema.len());
                }
                true
            }
            Err(e) => {
                assert!(!e.to_string().is_empty());
                false
            }
        }
    }

    #[test]
    fn fixtures_parse_and_pass_the_schema_check() {
        for (name, file) in fixtures() {
            let meta = parse_footer(&file).unwrap_or_else(|e| panic!("{name}: {e}"));
            schema_tree::check_schema(&meta.schema).unwrap();
            assert!(
                !build_schema_tree(&meta.schema).leaves().is_empty(),
                "{name}"
            );
        }
    }

    /// Cutting the file anywhere in the footer region loses the trailing
    /// magic or the metadata it points at.
    #[test]
    fn every_tail_truncation_is_an_error() {
        for (name, file) in fixtures() {
            let (start, _) = metadata(&file);
            for n in start.saturating_sub(16)..file.len() {
                assert!(parse_footer(&file[..n]).is_err(), "{name} cut to {n}");
            }
        }
    }

    /// The metadata cut short (with a footer that agrees) always ends the
    /// top-level struct before its STOP.
    #[test]
    fn every_metadata_truncation_is_an_error() {
        for (name, file) in fixtures() {
            let (_, meta) = metadata(&file);
            for n in 0..meta.len() {
                assert!(
                    matches!(parse_footer(&wrap(&meta[..n])), Err(FooterError::Thrift(_))),
                    "{name} metadata cut to {n}"
                );
            }
        }
    }

    /// A lying metadata length: every value from 0 to past the real one,
    /// plus the file size and the `u32` extremes.
    #[test]
    fn every_declared_metadata_length_is_handled() {
        for (name, file) in fixtures() {
            let (start, _) = metadata(&file);
            let tail = file.len() - FOOTER_LEN;
            let real = tail - start;
            let lens = (0..=real + 64)
                .chain([tail, tail + 1, u32::MAX as usize - 1, u32::MAX as usize])
                .map(|n| n as u32);
            for len in lens {
                let mut corrupt = file.clone();
                corrupt[tail..tail + 4].copy_from_slice(&len.to_le_bytes());
                let parsed = exercise(&corrupt);
                if len as usize > tail {
                    assert!(!parsed, "{name}: length {len} past the file accepted");
                }
            }
        }
    }

    /// Flip each byte of the metadata and footer with several masks.
    #[test]
    fn single_byte_flips_never_panic() {
        for (_, file) in fixtures() {
            let (start, _) = metadata(&file);
            for pos in start..file.len() {
                for mask in [0x01u8, 0x10, 0x80, 0xff] {
                    let mut corrupt = file.clone();
                    corrupt[pos] ^= mask;
                    exercise(&corrupt);
                }
            }
        }
    }

    /// Overwrite each metadata byte with values the Thrift decoder reads as
    /// STOP, a struct or list header, or a long varint.
    #[test]
    fn hostile_byte_overwrites_never_panic() {
        for (_, file) in fixtures() {
            let (start, meta) = metadata(&file);
            for pos in start..start + meta.len() {
                for byte in [0x00u8, 0x0c, 0x19, 0x1c, 0xf9, 0xfc, 0xff] {
                    let mut corrupt = file.clone();
                    corrupt[pos] = byte;
                    exercise(&corrupt);
                }
            }
        }
    }

    fn varint(out: &mut Vec<u8>, mut v: u64) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return;
            }
            out.push(b | 0x80);
        }
    }

    /// `FileMetaData` whose schema is `n` elements, each with the given
    /// `num_children`.
    fn chain_schema(n: usize, num_children: i32) -> Vec<u8> {
        // Field 2: list<struct> with an escaped (varint) size.
        let mut meta = vec![0x29, 0xfc];
        varint(&mut meta, n as u64);
        for _ in 0..n {
            meta.push(0x55); // field 5 (num_children): i32
            varint(
                &mut meta,
                u64::from(((num_children << 1) ^ (num_children >> 31)) as u32),
            );
            meta.push(0x00);
        }
        meta.push(0x00);
        meta
    }

    /// `chain_schema(n, 1)` with the last element made a leaf.
    fn chain_ending_in_leaf(n: usize) -> Vec<u8> {
        let mut meta = chain_schema(n, 1);
        // The last element is [0x55, 0x02, 0x00] before the final STOP.
        let end = meta.len() - 1;
        meta.splice(end - 3..end, [0x00]);
        meta
    }

    #[test]
    fn schema_num_children_past_the_list_is_an_error() {
        for meta in [chain_schema(1, 3), chain_schema(2, i32::MAX)] {
            let file = wrap(&meta);
            assert!(matches!(
                parse_footer(&file),
                Err(FooterError::InvalidSchema(_))
            ));
            assert!(ParquetFile::open(&file).is_err());
        }
    }

    #[test]
    fn empty_schema_is_an_error() {
        let file = wrap(&[0x00]);
        assert!(matches!(
            parse_footer(&file),
            Err(FooterError::InvalidSchema(_))
        ));
        assert!(ParquetFile::open(&file).is_err());
    }

    /// A 200k-deep schema chain used to recurse once per level in
    /// `build_schema_tree` (and the nested reader after it).
    #[test]
    fn deeply_nested_schema_is_an_error() {
        let file = wrap(&chain_ending_in_leaf(200_000));
        assert!(matches!(
            parse_footer(&file),
            Err(FooterError::InvalidSchema(_))
        ));
        assert!(ParquetFile::open(&file).is_err());
    }

    /// The deepest schema accepted is the root plus `MAX_SCHEMA_DEPTH`
    /// levels; one more is rejected.
    #[test]
    fn schema_at_the_depth_cap_parses() {
        let depth = schema_tree::MAX_SCHEMA_DEPTH;
        let parsed = parse_footer(&wrap(&chain_ending_in_leaf(depth + 1))).unwrap();
        let tree = build_schema_tree(&parsed.schema);
        assert_eq!(tree.leaves().len(), 1);
        assert_eq!(tree.leaves()[0].max_def_level, depth as u32);
        assert!(matches!(
            parse_footer(&wrap(&chain_ending_in_leaf(depth + 2))),
            Err(FooterError::InvalidSchema(_))
        ));
    }

    /// Thrift nesting far past `thrift::MAX_DEPTH` in the footer itself.
    #[test]
    fn deeply_nested_thrift_footer_is_an_error() {
        let mut meta = vec![0x1c; 1_000_000]; // field 1: struct, again and again
        meta.extend(vec![0x00; 1_000_001]);
        assert!(matches!(
            parse_footer(&wrap(&meta)),
            Err(FooterError::Thrift(ThriftError::DepthExceeded))
        ));
    }
}
