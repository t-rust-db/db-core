//! Column types and values (#566): what a Parquet file's flat or MAP
//! columns hold, shared by the writer ([`super::writer`]) and by readers
//! that know a table's schema without writing it (ADR 0029's
//! `ingest-read`).

pub use crate::storage::parquet::parquet_file::MapEntries;

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
    /// BYTE_ARRAY with no annotation: raw bytes (DuckDB `BLOB`).
    Binary,
    /// INT32 annotated as an unsigned 8-bit integer.
    UInt8,
    /// INT32 annotated as an unsigned 16-bit integer.
    UInt16,
    /// INT32 annotated as an unsigned 32-bit integer (stored as its bit
    /// pattern).
    UInt32,
    /// INT64 annotated as an unsigned 64-bit integer (stored as its bit
    /// pattern).
    UInt64,
    /// A fixed-point number, annotated DECIMAL(precision, scale): the
    /// unscaled value in INT32 up to precision 9, INT64 up to 18, and a
    /// 16-byte big-endian FIXED_LEN_BYTE_ARRAY above that, as DuckDB
    /// writes them. Precision is 1..=38, scale at most the precision.
    Decimal {
        /// Total significant digits.
        precision: u8,
        /// Digits after the decimal point.
        scale: u8,
    },
    /// FIXED_LEN_BYTE_ARRAY of this many bytes (at least 1), no
    /// annotation: e.g. 16 for an IPv6 address.
    FixedLenByteArray(u16),
    /// A `MAP<UTF8, UTF8>`: the group DuckDB writes for `MAP(VARCHAR,
    /// VARCHAR)`, a required UTF8 `key` and an optional UTF8 `value` per
    /// entry (#621). Keys within a map are unique.
    MapUtf8,
}

/// Where a [`ColumnType::Decimal`]'s unscaled value is stored.
#[cfg(any(feature = "storage-column-write", feature = "ingest-read"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DecimalStorage {
    Int32,
    Int64,
    Fixed16,
}

#[cfg(any(feature = "storage-column-write", feature = "ingest-read"))]
impl DecimalStorage {
    pub(crate) fn for_precision(precision: u8) -> Self {
        if precision <= 9 {
            DecimalStorage::Int32
        } else if precision <= 18 {
            DecimalStorage::Int64
        } else {
            DecimalStorage::Fixed16
        }
    }
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
    /// For a [`ColumnType::Binary`] column.
    Binary(Vec<Option<Vec<u8>>>),
    /// For a [`ColumnType::UInt8`] column.
    UInt8(Vec<Option<u8>>),
    /// For a [`ColumnType::UInt16`] column.
    UInt16(Vec<Option<u16>>),
    /// For a [`ColumnType::UInt32`] column.
    UInt32(Vec<Option<u32>>),
    /// For a [`ColumnType::UInt64`] column.
    UInt64(Vec<Option<u64>>),
    /// For a [`ColumnType::Decimal`] column: the unscaled values (`12.34`
    /// at scale 2 is `1234`), each within the column's precision.
    Decimal(Vec<Option<i128>>),
    /// For a [`ColumnType::FixedLenByteArray`] column: each value exactly
    /// the column's length.
    FixedLenByteArray(Vec<Option<Vec<u8>>>),
    /// For a [`ColumnType::MapUtf8`] column: each map's entries in order,
    /// `None` for a NULL map, `Some(vec![])` for an empty one.
    Map(Vec<Option<MapEntries>>),
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
            ColumnType::Binary => ColumnValues::Binary(Vec::new()),
            ColumnType::UInt8 => ColumnValues::UInt8(Vec::new()),
            ColumnType::UInt16 => ColumnValues::UInt16(Vec::new()),
            ColumnType::UInt32 => ColumnValues::UInt32(Vec::new()),
            ColumnType::UInt64 => ColumnValues::UInt64(Vec::new()),
            ColumnType::Decimal { .. } => ColumnValues::Decimal(Vec::new()),
            ColumnType::FixedLenByteArray(_) => ColumnValues::FixedLenByteArray(Vec::new()),
            ColumnType::MapUtf8 => ColumnValues::Map(Vec::new()),
        }
    }

    /// Whether these values are of `column_type`'s variant (any precision,
    /// scale or length: those are checked per value).
    pub fn is_for(&self, column_type: ColumnType) -> bool {
        matches!(
            (self, column_type),
            (ColumnValues::Boolean(_), ColumnType::Boolean)
                | (ColumnValues::Int32(_), ColumnType::Int32)
                | (ColumnValues::Int64(_), ColumnType::Int64)
                | (ColumnValues::Float(_), ColumnType::Float)
                | (ColumnValues::Double(_), ColumnType::Double)
                | (ColumnValues::Utf8(_), ColumnType::Utf8)
                | (
                    ColumnValues::TimestampMicros(_),
                    ColumnType::TimestampMicros
                )
                | (ColumnValues::TimestampNanos(_), ColumnType::TimestampNanos)
                | (ColumnValues::Binary(_), ColumnType::Binary)
                | (ColumnValues::UInt8(_), ColumnType::UInt8)
                | (ColumnValues::UInt16(_), ColumnType::UInt16)
                | (ColumnValues::UInt32(_), ColumnType::UInt32)
                | (ColumnValues::UInt64(_), ColumnType::UInt64)
                | (ColumnValues::Decimal(_), ColumnType::Decimal { .. })
                | (
                    ColumnValues::FixedLenByteArray(_),
                    ColumnType::FixedLenByteArray(_)
                )
                | (ColumnValues::Map(_), ColumnType::MapUtf8)
        )
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
            ColumnValues::Binary(v) | ColumnValues::FixedLenByteArray(v) => v.len(),
            ColumnValues::UInt8(v) => v.len(),
            ColumnValues::UInt16(v) => v.len(),
            ColumnValues::UInt32(v) => v.len(),
            ColumnValues::UInt64(v) => v.len(),
            ColumnValues::Decimal(v) => v.len(),
            ColumnValues::Map(v) => v.len(),
        }
    }

    /// No values at all.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Validity per value: `true` where the value is not null.
    #[cfg(feature = "storage-column-write")]
    pub(crate) fn validity(&self) -> Vec<bool> {
        match self {
            ColumnValues::Boolean(v) => v.iter().map(Option::is_some).collect(),
            ColumnValues::Int32(v) => v.iter().map(Option::is_some).collect(),
            ColumnValues::Int64(v)
            | ColumnValues::TimestampMicros(v)
            | ColumnValues::TimestampNanos(v) => v.iter().map(Option::is_some).collect(),
            ColumnValues::Float(v) => v.iter().map(Option::is_some).collect(),
            ColumnValues::Double(v) => v.iter().map(Option::is_some).collect(),
            ColumnValues::Utf8(v) => v.iter().map(Option::is_some).collect(),
            ColumnValues::Binary(v) | ColumnValues::FixedLenByteArray(v) => {
                v.iter().map(Option::is_some).collect()
            }
            ColumnValues::UInt8(v) => v.iter().map(Option::is_some).collect(),
            ColumnValues::UInt16(v) => v.iter().map(Option::is_some).collect(),
            ColumnValues::UInt32(v) => v.iter().map(Option::is_some).collect(),
            ColumnValues::UInt64(v) => v.iter().map(Option::is_some).collect(),
            ColumnValues::Decimal(v) => v.iter().map(Option::is_some).collect(),
            ColumnValues::Map(v) => v.iter().map(Option::is_some).collect(),
        }
    }

    /// Appends `other`, which the caller has checked is of the same type;
    /// a batch of another type is left unappended.
    #[cfg(feature = "storage-column-write")]
    pub(crate) fn append(&mut self, other: ColumnValues) {
        match (self, other) {
            (ColumnValues::Boolean(a), ColumnValues::Boolean(b)) => a.extend(b),
            (ColumnValues::Int32(a), ColumnValues::Int32(b)) => a.extend(b),
            (ColumnValues::Int64(a), ColumnValues::Int64(b)) => a.extend(b),
            (ColumnValues::Float(a), ColumnValues::Float(b)) => a.extend(b),
            (ColumnValues::Double(a), ColumnValues::Double(b)) => a.extend(b),
            (ColumnValues::Utf8(a), ColumnValues::Utf8(b)) => a.extend(b),
            (ColumnValues::TimestampMicros(a), ColumnValues::TimestampMicros(b)) => a.extend(b),
            (ColumnValues::TimestampNanos(a), ColumnValues::TimestampNanos(b)) => a.extend(b),
            (ColumnValues::Binary(a), ColumnValues::Binary(b)) => a.extend(b),
            (ColumnValues::UInt8(a), ColumnValues::UInt8(b)) => a.extend(b),
            (ColumnValues::UInt16(a), ColumnValues::UInt16(b)) => a.extend(b),
            (ColumnValues::UInt32(a), ColumnValues::UInt32(b)) => a.extend(b),
            (ColumnValues::UInt64(a), ColumnValues::UInt64(b)) => a.extend(b),
            (ColumnValues::Decimal(a), ColumnValues::Decimal(b)) => a.extend(b),
            (ColumnValues::FixedLenByteArray(a), ColumnValues::FixedLenByteArray(b)) => a.extend(b),
            (ColumnValues::Map(a), ColumnValues::Map(b)) => a.extend(b),
            _ => {}
        }
    }
}
