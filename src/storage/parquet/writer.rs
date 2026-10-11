//! Parquet writer for flat schemas (#566), the inverse of the reader in
//! [`crate::storage::parquet`], with no dependency on Arrow.
//!
//! Columns are `required` or `optional` leaves of one [`ColumnType`] each.
//! Rows arrive as column batches ([`ParquetWriter::write_columns`]) and are
//! buffered until a row group's worth is reached, then written out: one
//! column chunk per column, each a DATA_PAGE (v1) preceded by a dictionary
//! page when dictionary encoding pays off. [`ParquetWriter::finish`] writes
//! the footer. Only the current row group is held in memory.
//!
//! Every chunk carries `min_value`/`max_value`/`null_count` statistics (see
//! [`stats`]) and the footer a type-defined column order, so readers can
//! prune row groups on them.
//!
//! ```
//! use db_core::storage::parquet::writer::{
//!     ColumnSpec, ColumnType, ColumnValues, ParquetWriter, WriterOptions,
//! };
//!
//! let schema = vec![
//!     ColumnSpec::new("ts", ColumnType::TimestampMicros, false),
//!     ColumnSpec::new("level", ColumnType::Utf8, true),
//! ];
//! let mut w = ParquetWriter::new(Vec::new(), schema, WriterOptions::default())?;
//! w.write_columns(vec![
//!     ColumnValues::TimestampMicros(vec![Some(1), Some(2)]),
//!     ColumnValues::Utf8(vec![Some("INFO".into()), None]),
//! ])?;
//! let (bytes, summary) = w.finish()?;
//! assert_eq!(summary.num_rows, 2);
//! assert!(bytes.starts_with(b"PAR1") && bytes.ends_with(b"PAR1"));
//! # Ok::<(), db_core::storage::parquet::writer::WriteError>(())
//! ```

mod encode;
mod meta;
mod snappy;
mod stats;
mod thrift;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::hash::Hash;
use std::io::{self, Write};

pub use thrift::EncodeError;

use crate::storage::parquet::thrift::Value;

const MAGIC: &[u8] = b"PAR1";

/// The logical type of a column, and with it its physical type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    /// BOOLEAN.
    Boolean,
    /// INT32.
    Int32,
    /// INT64.
    Int64,
    /// FLOAT.
    Float,
    /// DOUBLE.
    Double,
    /// BYTE_ARRAY annotated as a UTF-8 string.
    Utf8,
    /// INT64 annotated as a UTC timestamp in microseconds.
    TimestampMicros,
    /// INT64 annotated as a timestamp in nanoseconds, values UTC by
    /// convention. Marked `isAdjustedToUTC = false`, as DuckDB writes its
    /// `TIMESTAMP_NS` (DuckDB has no time-zone-aware nanosecond type and
    /// would read a UTC-marked one as microsecond `TIMESTAMPTZ`). LogicalType
    /// only: the legacy ConvertedType has no nanosecond unit.
    TimestampNanos,
}

/// One column of the schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSpec {
    /// Column name, unique within the schema.
    pub name: String,
    /// Logical (and physical) type.
    pub column_type: ColumnType,
    /// `optional` (may hold nulls) rather than `required`.
    pub nullable: bool,
}

impl ColumnSpec {
    /// A column called `name`.
    pub fn new(name: impl Into<String>, column_type: ColumnType, nullable: bool) -> Self {
        ColumnSpec {
            name: name.into(),
            column_type,
            nullable,
        }
    }
}

/// A batch of values for one column; `None` is null.
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnValues {
    /// For a [`ColumnType::Boolean`] column.
    Boolean(Vec<Option<bool>>),
    /// For a [`ColumnType::Int32`] column.
    Int32(Vec<Option<i32>>),
    /// For a [`ColumnType::Int64`] column.
    Int64(Vec<Option<i64>>),
    /// For a [`ColumnType::Float`] column.
    Float(Vec<Option<f32>>),
    /// For a [`ColumnType::Double`] column.
    Double(Vec<Option<f64>>),
    /// For a [`ColumnType::Utf8`] column.
    Utf8(Vec<Option<String>>),
    /// For a [`ColumnType::TimestampMicros`] column: microseconds since the
    /// Unix epoch, UTC.
    TimestampMicros(Vec<Option<i64>>),
    /// For a [`ColumnType::TimestampNanos`] column: nanoseconds since the
    /// Unix epoch, UTC.
    TimestampNanos(Vec<Option<i64>>),
}

impl ColumnValues {
    /// An empty batch of the given type.
    pub fn empty(column_type: ColumnType) -> Self {
        match column_type {
            ColumnType::Boolean => ColumnValues::Boolean(Vec::new()),
            ColumnType::Int32 => ColumnValues::Int32(Vec::new()),
            ColumnType::Int64 => ColumnValues::Int64(Vec::new()),
            ColumnType::Float => ColumnValues::Float(Vec::new()),
            ColumnType::Double => ColumnValues::Double(Vec::new()),
            ColumnType::Utf8 => ColumnValues::Utf8(Vec::new()),
            ColumnType::TimestampMicros => ColumnValues::TimestampMicros(Vec::new()),
            ColumnType::TimestampNanos => ColumnValues::TimestampNanos(Vec::new()),
        }
    }

    /// The column type these values belong to.
    pub fn column_type(&self) -> ColumnType {
        match self {
            ColumnValues::Boolean(_) => ColumnType::Boolean,
            ColumnValues::Int32(_) => ColumnType::Int32,
            ColumnValues::Int64(_) => ColumnType::Int64,
            ColumnValues::Float(_) => ColumnType::Float,
            ColumnValues::Double(_) => ColumnType::Double,
            ColumnValues::Utf8(_) => ColumnType::Utf8,
            ColumnValues::TimestampMicros(_) => ColumnType::TimestampMicros,
            ColumnValues::TimestampNanos(_) => ColumnType::TimestampNanos,
        }
    }

    /// Number of values, nulls included.
    pub fn len(&self) -> usize {
        match self {
            ColumnValues::Boolean(v) => v.len(),
            ColumnValues::Int32(v) => v.len(),
            ColumnValues::Int64(v)
            | ColumnValues::TimestampMicros(v)
            | ColumnValues::TimestampNanos(v) => v.len(),
            ColumnValues::Float(v) => v.len(),
            ColumnValues::Double(v) => v.len(),
            ColumnValues::Utf8(v) => v.len(),
        }
    }

    /// No values at all.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Validity per value: `true` where the value is not null.
    fn validity(&self) -> Vec<bool> {
        match self {
            ColumnValues::Boolean(v) => v.iter().map(Option::is_some).collect(),
            ColumnValues::Int32(v) => v.iter().map(Option::is_some).collect(),
            ColumnValues::Int64(v)
            | ColumnValues::TimestampMicros(v)
            | ColumnValues::TimestampNanos(v) => v.iter().map(Option::is_some).collect(),
            ColumnValues::Float(v) => v.iter().map(Option::is_some).collect(),
            ColumnValues::Double(v) => v.iter().map(Option::is_some).collect(),
            ColumnValues::Utf8(v) => v.iter().map(Option::is_some).collect(),
        }
    }

    /// Appends `other`, which the caller has checked is of the same type;
    /// a batch of another type is left unappended.
    fn append(&mut self, other: ColumnValues) {
        match (self, other) {
            (ColumnValues::Boolean(a), ColumnValues::Boolean(b)) => a.extend(b),
            (ColumnValues::Int32(a), ColumnValues::Int32(b)) => a.extend(b),
            (ColumnValues::Int64(a), ColumnValues::Int64(b)) => a.extend(b),
            (ColumnValues::Float(a), ColumnValues::Float(b)) => a.extend(b),
            (ColumnValues::Double(a), ColumnValues::Double(b)) => a.extend(b),
            (ColumnValues::Utf8(a), ColumnValues::Utf8(b)) => a.extend(b),
            (ColumnValues::TimestampMicros(a), ColumnValues::TimestampMicros(b)) => a.extend(b),
            (ColumnValues::TimestampNanos(a), ColumnValues::TimestampNanos(b)) => a.extend(b),
            _ => {}
        }
    }

    /// Moves the values out, leaving an empty batch of the same type.
    fn take(&mut self) -> ColumnValues {
        std::mem::replace(self, ColumnValues::empty(self.column_type()))
    }
}

/// Page compression codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    /// No compression.
    Uncompressed,
    /// Snappy (raw block).
    Snappy,
}

impl Compression {
    fn codec_id(self) -> i32 {
        match self {
            Compression::Uncompressed => 0,
            Compression::Snappy => 1,
        }
    }
}

/// Writer settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterOptions {
    /// Page compression.
    pub compression: Compression,
    /// A row group is written once at least this many rows are buffered
    /// (so a row group can exceed it by up to one batch). Must be > 0.
    pub row_group_rows: usize,
    /// Dictionary-encode INT32, INT64, timestamp and string columns when
    /// it pays off: at most half the chunk's values distinct and the
    /// dictionary within `dictionary_max_bytes`. Otherwise the chunk is
    /// PLAIN.
    pub dictionary: bool,
    /// Largest PLAIN-encoded dictionary per column chunk, in bytes.
    pub dictionary_max_bytes: usize,
    /// The footer's `created_by`.
    pub created_by: String,
    /// The footer's `key_value_metadata`, in order (none when empty).
    pub key_value_metadata: Vec<(String, String)>,
}

impl Default for WriterOptions {
    /// Snappy, DuckDB's default row group size (122 880 rows), dictionary
    /// encoding up to 1 MiB per chunk.
    fn default() -> Self {
        WriterOptions {
            compression: Compression::Snappy,
            row_group_rows: 122_880,
            dictionary: true,
            dictionary_max_bytes: 1 << 20,
            created_by: format!("db-core version {}", env!("CARGO_PKG_VERSION")),
            key_value_metadata: Vec::new(),
        }
    }
}

/// Why a write failed.
#[derive(Debug)]
pub enum WriteError {
    /// The underlying writer failed.
    Io(io::Error),
    /// The metadata could not be encoded.
    Encode(EncodeError),
    /// The schema is empty, or has an empty or duplicate column name.
    InvalidSchema(String),
    /// [`WriterOptions::row_group_rows`] is 0.
    InvalidOptions(String),
    /// A batch has a different number of columns than the schema.
    ColumnCount {
        /// Columns in the schema.
        expected: usize,
        /// Columns in the batch.
        found: usize,
    },
    /// A batch column's values are not of the schema column's type.
    TypeMismatch {
        /// The column.
        column: String,
    },
    /// The batch's columns do not all have the same number of values.
    LengthMismatch {
        /// The column.
        column: String,
        /// Values in the batch's first column.
        expected: usize,
        /// Values in this column.
        found: usize,
    },
    /// A `required` column got a null.
    NullInRequired {
        /// The column.
        column: String,
    },
    /// A size or count exceeds what a Parquet field can hold (a page over
    /// 2 GiB, a string over 4 GiB, a file over 2^63 bytes).
    TooLarge(String),
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WriteError::Io(e) => write!(f, "parquet write: {e}"),
            WriteError::Encode(e) => write!(f, "parquet metadata: {e}"),
            WriteError::InvalidSchema(why) => write!(f, "invalid parquet schema: {why}"),
            WriteError::InvalidOptions(why) => write!(f, "invalid parquet writer options: {why}"),
            WriteError::ColumnCount { expected, found } => {
                write!(f, "batch has {found} columns, schema has {expected}")
            }
            WriteError::TypeMismatch { column } => {
                write!(f, "column '{column}': values do not match the column type")
            }
            WriteError::LengthMismatch {
                column,
                expected,
                found,
            } => write!(f, "column '{column}': {found} values, expected {expected}"),
            WriteError::NullInRequired { column } => {
                write!(f, "column '{column}' is required but got a null")
            }
            WriteError::TooLarge(what) => write!(f, "too large for parquet: {what}"),
        }
    }
}

impl std::error::Error for WriteError {}

impl From<io::Error> for WriteError {
    fn from(e: io::Error) -> Self {
        WriteError::Io(e)
    }
}

impl From<EncodeError> for WriteError {
    fn from(e: EncodeError) -> Self {
        WriteError::Encode(e)
    }
}

fn too_large(what: &str) -> WriteError {
    WriteError::TooLarge(what.to_string())
}

fn to_i32(n: usize, what: &str) -> Result<i32, WriteError> {
    i32::try_from(n).map_err(|_| too_large(what))
}

fn to_i64(n: u64, what: &str) -> Result<i64, WriteError> {
    i64::try_from(n).map_err(|_| too_large(what))
}

/// What [`ParquetWriter::finish`] wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSummary {
    /// Rows in the file.
    pub num_rows: u64,
    /// Row groups in the file.
    pub row_groups: usize,
    /// File size in bytes.
    pub bytes: u64,
}

/// The output and how many bytes have gone into it: page offsets in the
/// metadata are file positions.
struct Sink<W: Write> {
    out: W,
    offset: u64,
}

impl<W: Write> Sink<W> {
    fn write(&mut self, bytes: &[u8]) -> Result<(), WriteError> {
        self.out.write_all(bytes)?;
        let n = u64::try_from(bytes.len()).map_err(|_| too_large("write"))?;
        self.offset = self
            .offset
            .checked_add(n)
            .ok_or_else(|| too_large("file size"))?;
        Ok(())
    }

    fn position(&self) -> Result<i64, WriteError> {
        to_i64(self.offset, "file offset")
    }
}

/// Streaming Parquet writer; see the [module docs](self).
pub struct ParquetWriter<W: Write> {
    sink: Sink<W>,
    schema: Vec<ColumnSpec>,
    options: WriterOptions,
    buffer: Vec<ColumnValues>,
    buffered_rows: usize,
    row_groups: Vec<Value>,
    num_rows: u64,
}

impl<W: Write> ParquetWriter<W> {
    /// Starts a file on `out` (writes the leading magic).
    pub fn new(
        out: W,
        schema: Vec<ColumnSpec>,
        options: WriterOptions,
    ) -> Result<Self, WriteError> {
        validate_schema(&schema)?;
        if options.row_group_rows == 0 {
            return Err(WriteError::InvalidOptions(
                "row_group_rows must be at least 1".to_string(),
            ));
        }
        let buffer = schema
            .iter()
            .map(|c| ColumnValues::empty(c.column_type))
            .collect();
        let mut sink = Sink { out, offset: 0 };
        sink.write(MAGIC)?;
        Ok(ParquetWriter {
            sink,
            schema,
            options,
            buffer,
            buffered_rows: 0,
            row_groups: Vec::new(),
            num_rows: 0,
        })
    }

    /// The schema the file is written with.
    pub fn schema(&self) -> &[ColumnSpec] {
        &self.schema
    }

    /// Appends one batch: a value list per schema column, in schema order,
    /// all of the same length. Writes a row group once
    /// [`WriterOptions::row_group_rows`] rows are buffered. A rejected
    /// batch leaves the writer unchanged.
    pub fn write_columns(&mut self, columns: Vec<ColumnValues>) -> Result<(), WriteError> {
        if columns.len() != self.schema.len() {
            return Err(WriteError::ColumnCount {
                expected: self.schema.len(),
                found: columns.len(),
            });
        }
        let rows = columns.first().map_or(0, ColumnValues::len);
        for (spec, values) in self.schema.iter().zip(&columns) {
            check_column(spec, values, rows)?;
        }
        for (buffer, values) in self.buffer.iter_mut().zip(columns) {
            buffer.append(values);
        }
        self.buffered_rows = self.buffered_rows.saturating_add(rows);
        if self.buffered_rows >= self.options.row_group_rows {
            self.flush_row_group()?;
        }
        Ok(())
    }

    /// Writes the buffered rows as a row group now (nothing when no rows
    /// are buffered), e.g. to bound how long rows wait in memory.
    pub fn flush_row_group(&mut self) -> Result<(), WriteError> {
        if self.buffered_rows == 0 {
            return Ok(());
        }
        let rows = u64::try_from(self.buffered_rows).map_err(|_| too_large("row count"))?;
        let group_start = self.sink.position()?;
        let columns: Vec<ColumnValues> = self.buffer.iter_mut().map(ColumnValues::take).collect();
        let mut chunks = Vec::with_capacity(columns.len());
        let mut uncompressed: u64 = 0;
        let mut compressed: u64 = 0;
        for (spec, values) in self.schema.iter().zip(&columns) {
            let chunk = write_chunk(&mut self.sink, spec, values, &self.options)?;
            uncompressed = uncompressed
                .checked_add(chunk.uncompressed)
                .ok_or_else(|| too_large("row group size"))?;
            compressed = compressed
                .checked_add(chunk.compressed)
                .ok_or_else(|| too_large("row group size"))?;
            chunks.push(chunk.metadata);
        }
        self.row_groups.push(meta::row_group(
            chunks,
            to_i64(uncompressed, "row group size")?,
            to_i64(rows, "row count")?,
            group_start,
            to_i64(compressed, "row group size")?,
        ));
        self.num_rows = self.num_rows.saturating_add(rows);
        self.buffered_rows = 0;
        Ok(())
    }

    /// Writes any buffered rows and the footer, and hands back the output.
    pub fn finish(mut self) -> Result<(W, FileSummary), WriteError> {
        self.flush_row_group()?;
        let row_groups = self.row_groups.len();
        let fields = meta::file_metadata(
            &self.schema,
            to_i64(self.num_rows, "row count")?,
            std::mem::take(&mut self.row_groups),
            &self.options.created_by,
            &self.options.key_value_metadata,
        );
        let footer = thrift::encode_struct(&fields)?;
        let footer_len = u32::try_from(footer.len()).map_err(|_| too_large("footer"))?;
        self.sink.write(&footer)?;
        self.sink.write(&footer_len.to_le_bytes())?;
        self.sink.write(MAGIC)?;
        self.sink.out.flush()?;
        let summary = FileSummary {
            num_rows: self.num_rows,
            row_groups,
            bytes: self.sink.offset,
        };
        Ok((self.sink.out, summary))
    }
}

fn validate_schema(schema: &[ColumnSpec]) -> Result<(), WriteError> {
    if schema.is_empty() {
        return Err(WriteError::InvalidSchema("no columns".to_string()));
    }
    let mut seen = HashSet::new();
    for spec in schema {
        if spec.name.is_empty() {
            return Err(WriteError::InvalidSchema("empty column name".to_string()));
        }
        if !seen.insert(spec.name.as_str()) {
            return Err(WriteError::InvalidSchema(format!(
                "duplicate column name '{}'",
                spec.name
            )));
        }
    }
    Ok(())
}

fn check_column(spec: &ColumnSpec, values: &ColumnValues, rows: usize) -> Result<(), WriteError> {
    if values.column_type() != spec.column_type {
        return Err(WriteError::TypeMismatch {
            column: spec.name.clone(),
        });
    }
    if values.len() != rows {
        return Err(WriteError::LengthMismatch {
            column: spec.name.clone(),
            expected: rows,
            found: values.len(),
        });
    }
    if spec.nullable {
        return Ok(());
    }
    if values.validity().contains(&false) {
        return Err(WriteError::NullInRequired {
            column: spec.name.clone(),
        });
    }
    Ok(())
}

/// A written column chunk: its `ColumnChunk` metadata and on-disk sizes
/// (page headers included).
struct WrittenChunk {
    metadata: Value,
    uncompressed: u64,
    compressed: u64,
}

/// A chunk's encoded values: the data page body after the definition
/// levels, its encoding, and the dictionary page body and entry count when
/// dictionary-encoded.
struct EncodedValues {
    data: Vec<u8>,
    encoding: i32,
    dictionary: Option<(Vec<u8>, usize)>,
}

fn write_chunk<W: Write>(
    sink: &mut Sink<W>,
    spec: &ColumnSpec,
    values: &ColumnValues,
    options: &WriterOptions,
) -> Result<WrittenChunk, WriteError> {
    let chunk_stats = stats::chunk_stats(values);
    let mut body = Vec::new();
    if spec.nullable {
        let levels: Vec<u32> = values.validity().into_iter().map(u32::from).collect();
        let mut encoded = Vec::new();
        encode::rle_hybrid(&mut encoded, &levels, 1);
        let len = u32::try_from(encoded.len()).map_err(|_| too_large("definition levels"))?;
        body.extend_from_slice(&len.to_le_bytes());
        body.extend_from_slice(&encoded);
    }
    let encoded = encode_values(values, options)?;
    body.extend_from_slice(&encoded.data);

    let codec = options.compression;
    let mut sizes = PageTotals::default();
    let mut encodings = vec![meta::ENCODING_PLAIN, meta::ENCODING_RLE];
    let dictionary_page_offset = match &encoded.dictionary {
        Some((dictionary, entries)) => {
            let offset = sink.position()?;
            sizes.add(write_page(sink, dictionary, *entries, codec, None)?)?;
            encodings.push(meta::ENCODING_RLE_DICTIONARY);
            Some(offset)
        }
        None => None,
    };
    let data_page_offset = sink.position()?;
    sizes.add(write_page(
        sink,
        &body,
        values.len(),
        codec,
        Some(encoded.encoding),
    )?)?;

    let layout = meta::ChunkLayout {
        encodings,
        codec: codec.codec_id(),
        num_values: to_i64(
            u64::try_from(values.len()).unwrap_or(u64::MAX),
            "value count",
        )?,
        total_uncompressed_size: to_i64(sizes.uncompressed, "column chunk size")?,
        total_compressed_size: to_i64(sizes.compressed, "column chunk size")?,
        data_page_offset,
        dictionary_page_offset,
    };
    let null_count = to_i64(
        u64::try_from(chunk_stats.null_count).unwrap_or(u64::MAX),
        "null count",
    )?;
    Ok(WrittenChunk {
        metadata: meta::column_chunk(spec, &layout, &chunk_stats, null_count),
        uncompressed: sizes.uncompressed,
        compressed: sizes.compressed,
    })
}

/// Running on-disk size of a chunk's pages.
#[derive(Default)]
struct PageTotals {
    uncompressed: u64,
    compressed: u64,
}

impl PageTotals {
    fn add(&mut self, page: PageTotals) -> Result<(), WriteError> {
        self.uncompressed = self
            .uncompressed
            .checked_add(page.uncompressed)
            .ok_or_else(|| too_large("column chunk size"))?;
        self.compressed = self
            .compressed
            .checked_add(page.compressed)
            .ok_or_else(|| too_large("column chunk size"))?;
        Ok(())
    }
}

/// Writes one page: a data page in `data_encoding`, or (for `None`) a
/// PLAIN dictionary page. Returns its size with the header.
fn write_page<W: Write>(
    sink: &mut Sink<W>,
    body: &[u8],
    num_values: usize,
    compression: Compression,
    data_encoding: Option<i32>,
) -> Result<PageTotals, WriteError> {
    let compressed = match compression {
        Compression::Uncompressed => None,
        Compression::Snappy => Some(snappy::compress(body)),
    };
    let payload = compressed.as_deref().unwrap_or(body);
    let sizes = meta::PageSizes {
        uncompressed: to_i32(body.len(), "page size")?,
        compressed: to_i32(payload.len(), "page size")?,
        num_values: to_i32(num_values, "values per page")?,
    };
    let header = match data_encoding {
        Some(encoding) => meta::data_page_header(&sizes, encoding),
        None => meta::dictionary_page_header(&sizes),
    };
    let header = thrift::encode_struct(&header)?;
    sink.write(&header)?;
    sink.write(payload)?;
    let header_len = u64::try_from(header.len()).map_err(|_| too_large("page header"))?;
    let body_len = u64::try_from(body.len()).map_err(|_| too_large("page size"))?;
    let payload_len = u64::try_from(payload.len()).map_err(|_| too_large("page size"))?;
    Ok(PageTotals {
        uncompressed: header_len.saturating_add(body_len),
        compressed: header_len.saturating_add(payload_len),
    })
}

fn encode_values(
    values: &ColumnValues,
    options: &WriterOptions,
) -> Result<EncodedValues, WriteError> {
    match values {
        ColumnValues::Boolean(v) => {
            let present: Vec<bool> = v.iter().flatten().copied().collect();
            let mut data = Vec::new();
            encode::plain_bool(&mut data, &present);
            Ok(plain(data))
        }
        ColumnValues::Int32(v) => dictionary_or_plain(v, options, |out, x| {
            out.extend_from_slice(&x.to_le_bytes());
            Ok(())
        }),
        ColumnValues::Int64(v)
        | ColumnValues::TimestampMicros(v)
        | ColumnValues::TimestampNanos(v) => dictionary_or_plain(v, options, |out, x| {
            out.extend_from_slice(&x.to_le_bytes());
            Ok(())
        }),
        ColumnValues::Float(v) => {
            let mut data = Vec::new();
            for x in v.iter().flatten() {
                data.extend_from_slice(&x.to_le_bytes());
            }
            Ok(plain(data))
        }
        ColumnValues::Double(v) => {
            let mut data = Vec::new();
            for x in v.iter().flatten() {
                data.extend_from_slice(&x.to_le_bytes());
            }
            Ok(plain(data))
        }
        ColumnValues::Utf8(v) => dictionary_or_plain(v, options, |out, s| {
            encode::plain_byte_array(out, s.as_bytes()).ok_or_else(|| too_large("string value"))
        }),
    }
}

fn plain(data: Vec<u8>) -> EncodedValues {
    EncodedValues {
        data,
        encoding: meta::ENCODING_PLAIN,
        dictionary: None,
    }
}

fn dictionary_or_plain<T: Eq + Hash>(
    values: &[Option<T>],
    options: &WriterOptions,
    encode_plain: impl Fn(&mut Vec<u8>, &T) -> Result<(), WriteError>,
) -> Result<EncodedValues, WriteError> {
    let present: Vec<&T> = values.iter().flatten().collect();
    if options.dictionary {
        if let Some(encoded) = dictionary(&present, options.dictionary_max_bytes, &encode_plain)? {
            return Ok(encoded);
        }
    }
    let mut data = Vec::new();
    for value in present {
        encode_plain(&mut data, value)?;
    }
    Ok(plain(data))
}

/// Dictionary encoding of `present` (the chunk's non-null values), or
/// `None` when it does not pay off: no values, more than half of them
/// distinct, or a dictionary over `max_bytes`.
fn dictionary<T: Eq + Hash>(
    present: &[&T],
    max_bytes: usize,
    encode_plain: &impl Fn(&mut Vec<u8>, &T) -> Result<(), WriteError>,
) -> Result<Option<EncodedValues>, WriteError> {
    if present.is_empty() {
        return Ok(None);
    }
    let mut ids: HashMap<&T, u32> = HashMap::new();
    let mut dictionary = Vec::new();
    let mut indices = Vec::with_capacity(present.len());
    for &value in present {
        let id = match ids.get(value) {
            Some(id) => *id,
            None => {
                let Ok(id) = u32::try_from(ids.len()) else {
                    return Ok(None);
                };
                encode_plain(&mut dictionary, value)?;
                if dictionary.len() > max_bytes {
                    return Ok(None);
                }
                ids.insert(value, id);
                id
            }
        };
        indices.push(id);
    }
    if ids.len().saturating_mul(2) > present.len() {
        return Ok(None);
    }
    let max_index = u32::try_from(ids.len().saturating_sub(1)).unwrap_or(u32::MAX);
    // Width 0 is legal for a one-entry dictionary but not every reader
    // takes it; one bit costs at most a byte per eight values.
    let width = encode::bit_width(max_index).max(1);
    let mut data = vec![u8::try_from(width).unwrap_or(32)];
    encode::rle_hybrid(&mut data, &indices, width);
    Ok(Some(EncodedValues {
        data,
        encoding: meta::ENCODING_RLE_DICTIONARY,
        dictionary: Some((dictionary, ids.len())),
    }))
}

#[cfg(test)]
mod tests;
