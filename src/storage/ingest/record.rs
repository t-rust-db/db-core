//! Record tables (#594): a table whose schema is fixed when it is created,
//! appended to with typed rows instead of log lines.
//!
//! A record table has no `raw` line, no predefined stream columns and no
//! type widening: a flush writes exactly the schema's columns, in schema
//! order and types, and [`check_row`] refuses a row that does not match.
//! In the WAL a row is `[observed_ts i64 LE]` (the arrival time, which a
//! tiered table still partitions by) followed by [`encode_row`]'s bytes.
//!
//! In memory a page of rows is a [`RowPage`]: one writer [`ColumnValues`]
//! per schema column, so a flush writes them as they are. A query reads a
//! row page through [`engine_values`], which gives every value the form the
//! column engine decodes the flushed Parquet into (an unsigned value as an
//! integer, a decimal as a float, bytes as lowercase hex), so a record
//! reads the same before and after its flush.
//!
//! A `MAP<UTF8, UTF8>` column (#621) takes [`RecordValue::Map`]. A map whose
//! key repeats (HTTP repeats `Set-Cookie`, `X-Forwarded-For`...) is stored
//! with the repeated values joined, in order, under the key's first
//! position: with `", "` (the HTTP list separator), or a newline for
//! `Set-Cookie`, whose values may contain commas. NULL values are left out
//! of the join; a key whose values are all NULL keeps a NULL value. Keys
//! are compared exactly, as a MAP's are.

#[cfg(feature = "ingest")]
use std::cmp::Ordering;
#[cfg(feature = "ingest")]
use std::collections::HashSet;
use std::sync::Arc;

#[cfg(feature = "engine-column")]
use crate::storage::column::hex;
use crate::storage::parquet::schema::{ColumnSpec, ColumnType, ColumnValues};
#[cfg(any(feature = "ingest", feature = "engine-column"))]
use crate::storage::parquet::FileError;
#[cfg(feature = "ingest")]
use crate::storage::parquet::{Field, RowGroupReader};
#[cfg(feature = "engine-column")]
use crate::vm::batch::Value;

/// One value of a record row.
#[derive(Debug, Clone, PartialEq)]
pub enum RecordValue {
    /// NULL, for a nullable column.
    Null,
    /// A BOOLEAN.
    Bool(bool),
    /// A signed integer: INT32 (in range), INT64, a timestamp, or an
    /// unsigned column when it is not negative and in range.
    Int(i64),
    /// An unsigned integer, for an unsigned column (in range).
    UInt(u64),
    /// A FLOAT (narrowed to `f32`) or DOUBLE.
    Float(f64),
    /// A UTF8 string.
    Str(String),
    /// Bytes, for a BINARY or FIXED_LEN_BYTE_ARRAY (of exactly its length)
    /// column.
    Bytes(Vec<u8>),
    /// A DECIMAL's unscaled value (`12.34` at scale 2 is `1234`), within the
    /// column's precision.
    Decimal(i128),
    /// A MAP's `(key, value)` entries, in order; a repeated key's values
    /// are joined (see the [module docs](self)).
    Map(Vec<(String, Option<String>)>),
}

/// The name a column type has in the manifest.
pub fn type_name(t: ColumnType) -> String {
    match t {
        ColumnType::Boolean => "boolean".into(),
        ColumnType::Int32 => "int32".into(),
        ColumnType::Int64 => "int64".into(),
        ColumnType::Float => "float".into(),
        ColumnType::Double => "double".into(),
        ColumnType::Utf8 => "utf8".into(),
        ColumnType::TimestampMicros => "timestamp_micros".into(),
        ColumnType::TimestampNanos => "timestamp_nanos".into(),
        ColumnType::Binary => "binary".into(),
        ColumnType::UInt8 => "uint8".into(),
        ColumnType::UInt16 => "uint16".into(),
        ColumnType::UInt32 => "uint32".into(),
        ColumnType::UInt64 => "uint64".into(),
        ColumnType::Decimal { precision, scale } => format!("decimal({precision},{scale})"),
        ColumnType::FixedLenByteArray(len) => format!("fixed({len})"),
        ColumnType::MapUtf8 => "map(utf8,utf8)".into(),
    }
}

/// The column type named `name` by [`type_name`].
pub fn type_from_name(name: &str) -> Option<ColumnType> {
    let simple = [
        ColumnType::Boolean,
        ColumnType::Int32,
        ColumnType::Int64,
        ColumnType::Float,
        ColumnType::Double,
        ColumnType::Utf8,
        ColumnType::TimestampMicros,
        ColumnType::TimestampNanos,
        ColumnType::Binary,
        ColumnType::UInt8,
        ColumnType::UInt16,
        ColumnType::UInt32,
        ColumnType::UInt64,
        ColumnType::MapUtf8,
    ];
    if let Some(t) = simple.into_iter().find(|t| type_name(*t) == name) {
        return Some(t);
    }
    if let Some(args) = name
        .strip_prefix("decimal(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let (precision, scale) = args.split_once(',')?;
        return Some(ColumnType::Decimal {
            precision: precision.parse().ok()?,
            scale: scale.parse().ok()?,
        });
    }
    let len = name.strip_prefix("fixed(")?.strip_suffix(')')?;
    Some(ColumnType::FixedLenByteArray(len.parse().ok()?))
}

/// Why `schema` cannot be a record table's: no columns, a duplicate name,
/// a name the manifest cannot store as a plain string, or a type the
/// Parquet writer refuses.
#[cfg(feature = "ingest")]
pub(crate) fn invalid_schema(schema: &[ColumnSpec]) -> Option<String> {
    if schema.is_empty() {
        return Some("a record table needs at least one column".to_string());
    }
    let mut seen = HashSet::new();
    for spec in schema {
        let plain = !spec.name.is_empty()
            && spec
                .name
                .chars()
                .all(|c| !c.is_control() && c != '"' && c != '\\');
        if !plain {
            return Some(format!(
                "column name {:?}: names are non-empty, without quotes, backslashes or control characters",
                spec.name
            ));
        }
        if !seen.insert(spec.name.as_str()) {
            return Some(format!("duplicate column name '{}'", spec.name));
        }
    }
    crate::storage::parquet::writer::validate_schema(schema)
        .err()
        .map(|e| e.to_string())
}

/// Why `row` does not match `schema`: its length, a NULL in a required
/// column, or a value of another type or out of its column's range.
/// Values are never converted between types.
pub(crate) fn check_row(schema: &[ColumnSpec], row: &[RecordValue]) -> Option<String> {
    if row.len() != schema.len() {
        return Some(format!("{} values for {} columns", row.len(), schema.len()));
    }
    schema.iter().zip(row).find_map(|(spec, value)| {
        let fits = match (spec.column_type, value) {
            (_, RecordValue::Null) => spec.nullable,
            (ColumnType::Boolean, RecordValue::Bool(_))
            | (
                ColumnType::Int64 | ColumnType::TimestampMicros | ColumnType::TimestampNanos,
                RecordValue::Int(_),
            )
            | (ColumnType::Float | ColumnType::Double, RecordValue::Float(_))
            | (ColumnType::Utf8, RecordValue::Str(_))
            | (ColumnType::Binary, RecordValue::Bytes(_)) => true,
            (ColumnType::Int32, RecordValue::Int(i)) => i32::try_from(*i).is_ok(),
            (
                ColumnType::UInt8 | ColumnType::UInt16 | ColumnType::UInt32 | ColumnType::UInt64,
                v,
            ) => unsigned(v).is_some_and(|u| u <= unsigned_max(spec.column_type)),
            (ColumnType::Decimal { precision, .. }, RecordValue::Decimal(d)) => {
                d.unsigned_abs() < 10u128.pow(u32::from(precision))
            }
            (ColumnType::FixedLenByteArray(len), RecordValue::Bytes(b)) => {
                b.len() == usize::from(len)
            }
            (ColumnType::MapUtf8, RecordValue::Map(_)) => true,
            _ => false,
        };
        (!fits).then(|| {
            format!(
                "column '{}' ({}{}) cannot hold {value:?}",
                spec.name,
                type_name(spec.column_type),
                if spec.nullable { "" } else { ", required" }
            )
        })
    })
}

/// An unsigned value as `u64`: a `UInt`, or an `Int` that is not negative.
fn unsigned(value: &RecordValue) -> Option<u64> {
    match value {
        RecordValue::UInt(u) => Some(*u),
        RecordValue::Int(i) => u64::try_from(*i).ok(),
        _ => None,
    }
}

fn unsigned_max(t: ColumnType) -> u64 {
    match t {
        ColumnType::UInt8 => u64::from(u8::MAX),
        ColumnType::UInt16 => u64::from(u16::MAX),
        ColumnType::UInt32 => u64::from(u32::MAX),
        _ => u64::MAX,
    }
}

const TAG_NULL: u8 = 0;
const TAG_BOOL: u8 = 1;
const TAG_INT: u8 = 2;
const TAG_UINT: u8 = 3;
const TAG_FLOAT: u8 = 4;
const TAG_STR: u8 = 5;
const TAG_BYTES: u8 = 6;
const TAG_DECIMAL: u8 = 7;
const TAG_MAP: u8 = 8;

/// A row as WAL bytes: per value a tag byte, then its little-endian bytes
/// (a string or bytes as a `u32` length and the bytes). Self-describing,
/// so [`decode_row`] needs no schema; the replay checks the row against
/// it again. `None` when a string or bytes value is over 4 GiB.
#[cfg(feature = "ingest")]
pub(crate) fn encode_row(row: &[RecordValue]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for value in row {
        match value {
            RecordValue::Null => out.push(TAG_NULL),
            RecordValue::Bool(b) => out.extend_from_slice(&[TAG_BOOL, u8::from(*b)]),
            RecordValue::Int(i) => {
                out.push(TAG_INT);
                out.extend_from_slice(&i.to_le_bytes());
            }
            RecordValue::UInt(u) => {
                out.push(TAG_UINT);
                out.extend_from_slice(&u.to_le_bytes());
            }
            RecordValue::Float(f) => {
                out.push(TAG_FLOAT);
                out.extend_from_slice(&f.to_le_bytes());
            }
            RecordValue::Str(s) => {
                out.push(TAG_STR);
                length_prefixed(&mut out, s.as_bytes())?;
            }
            RecordValue::Bytes(b) => {
                out.push(TAG_BYTES);
                length_prefixed(&mut out, b)?;
            }
            RecordValue::Decimal(d) => {
                out.push(TAG_DECIMAL);
                out.extend_from_slice(&d.to_le_bytes());
            }
            // The entry count, then per entry the key and its value (a
            // NULL tag, or a string tag and the string).
            RecordValue::Map(entries) => {
                out.push(TAG_MAP);
                out.extend_from_slice(&u32::try_from(entries.len()).ok()?.to_le_bytes());
                for (key, value) in entries {
                    length_prefixed(&mut out, key.as_bytes())?;
                    match value {
                        None => out.push(TAG_NULL),
                        Some(v) => {
                            out.push(TAG_STR);
                            length_prefixed(&mut out, v.as_bytes())?;
                        }
                    }
                }
            }
        }
    }
    Some(out)
}

#[cfg(feature = "ingest")]
fn length_prefixed(out: &mut Vec<u8>, bytes: &[u8]) -> Option<()> {
    out.extend_from_slice(&u32::try_from(bytes.len()).ok()?.to_le_bytes());
    out.extend_from_slice(bytes);
    Some(())
}

/// The row [`encode_row`] wrote; `None` for bytes it cannot have written.
pub(crate) fn decode_row(bytes: &[u8]) -> Option<Vec<RecordValue>> {
    let mut at = 0usize;
    let mut take = |n: usize| {
        let end = at.checked_add(n)?;
        let taken = bytes.get(at..end)?;
        at = end;
        Some(taken)
    };
    let mut row = Vec::new();
    // Past the last value there is no tag to take.
    while let Some(tag) = take(1) {
        row.push(match tag.first().copied()? {
            TAG_NULL => RecordValue::Null,
            TAG_BOOL => RecordValue::Bool(take(1)? != [0]),
            TAG_INT => RecordValue::Int(i64::from_le_bytes(take(8)?.try_into().ok()?)),
            TAG_UINT => RecordValue::UInt(u64::from_le_bytes(take(8)?.try_into().ok()?)),
            TAG_FLOAT => RecordValue::Float(f64::from_le_bytes(take(8)?.try_into().ok()?)),
            tag @ (TAG_STR | TAG_BYTES) => {
                let len = u32::from_le_bytes(take(4)?.try_into().ok()?);
                let data = take(usize::try_from(len).ok()?)?.to_vec();
                if tag == TAG_STR {
                    RecordValue::Str(String::from_utf8(data).ok()?)
                } else {
                    RecordValue::Bytes(data)
                }
            }
            TAG_DECIMAL => RecordValue::Decimal(i128::from_le_bytes(take(16)?.try_into().ok()?)),
            TAG_MAP => {
                let count = u32::from_le_bytes(take(4)?.try_into().ok()?);
                let mut entries = Vec::new();
                for _ in 0..count {
                    let len = u32::from_le_bytes(take(4)?.try_into().ok()?);
                    let key = String::from_utf8(take(usize::try_from(len).ok()?)?.to_vec()).ok()?;
                    let value = match take(1)?.first().copied()? {
                        TAG_NULL => None,
                        TAG_STR => {
                            let len = u32::from_le_bytes(take(4)?.try_into().ok()?);
                            let bytes = take(usize::try_from(len).ok()?)?.to_vec();
                            Some(String::from_utf8(bytes).ok()?)
                        }
                        _ => return None,
                    };
                    entries.push((key, value));
                }
                RecordValue::Map(entries)
            }
            _ => return None,
        });
    }
    Some(row)
}

/// A page of typed rows: their arrival times and one [`ColumnValues`] per
/// schema column.
#[derive(Debug, Clone)]
pub struct RowPage {
    schema: Arc<[ColumnSpec]>,
    observed: Vec<i64>,
    columns: Vec<ColumnValues>,
}

impl RowPage {
    /// An empty page for `schema`.
    pub(crate) fn new(schema: Arc<[ColumnSpec]>) -> RowPage {
        let columns = schema
            .iter()
            .map(|s| ColumnValues::empty(s.column_type))
            .collect();
        RowPage {
            schema,
            observed: Vec::new(),
            columns,
        }
    }

    /// An empty page with this page's schema.
    pub(crate) fn cleared(&self) -> RowPage {
        RowPage::new(Arc::clone(&self.schema))
    }

    /// Appends `row`, which [`check_row`] has accepted for this schema.
    pub(crate) fn push(&mut self, observed_ts_ns: i64, row: Vec<RecordValue>) {
        self.observed.push(observed_ts_ns);
        for (column, value) in self.columns.iter_mut().zip(row) {
            push_value(column, value);
        }
    }

    /// Rows in the page.
    pub fn len(&self) -> usize {
        self.observed.len()
    }

    /// No rows.
    pub fn is_empty(&self) -> bool {
        self.observed.is_empty()
    }

    /// Each row's arrival time (ns).
    pub fn observed_ts_ns(&self) -> &[i64] {
        &self.observed
    }

    /// The table's schema.
    pub fn schema(&self) -> &[ColumnSpec] {
        &self.schema
    }

    /// The values of each schema column, in schema order.
    pub fn columns(&self) -> &[ColumnValues] {
        &self.columns
    }

    /// Column `name` as the column engine reads it; `None` for a name not
    /// in the schema.
    #[cfg(feature = "engine-column")]
    pub(crate) fn engine_column(&self, name: &str) -> Option<Result<Vec<Value>, String>> {
        let index = self.schema.iter().position(|s| s.name == name)?;
        let spec = self.schema.get(index)?;
        let values = self.columns.get(index)?;
        Some(engine_values(values, spec.column_type))
    }
}

/// `row` with every map's repeated keys joined (see the [module docs](self)).
#[cfg(feature = "ingest")]
pub(crate) fn join_repeated_keys(row: Vec<RecordValue>) -> Vec<RecordValue> {
    row.into_iter()
        .map(|value| match value {
            RecordValue::Map(entries) => RecordValue::Map(join_entries(entries)),
            other => other,
        })
        .collect()
}

#[cfg(feature = "ingest")]
fn join_entries(entries: Vec<(String, Option<String>)>) -> Vec<(String, Option<String>)> {
    let mut out: Vec<(String, Option<String>)> = Vec::with_capacity(entries.len());
    for (key, value) in entries {
        let Some((_, joined)) = out.iter_mut().find(|(k, _)| *k == key) else {
            out.push((key, value));
            continue;
        };
        let separator = if key.eq_ignore_ascii_case("set-cookie") {
            "\n"
        } else {
            ", "
        };
        match (joined.as_mut(), value) {
            (Some(acc), Some(v)) => {
                acc.push_str(separator);
                acc.push_str(&v);
            }
            (None, Some(v)) => *joined = Some(v),
            (_, None) => {}
        }
    }
    out
}

/// Appends `value` to `column`; a value of another type (which
/// [`check_row`] refuses) is appended as NULL.
fn push_value(column: &mut ColumnValues, value: RecordValue) {
    use RecordValue as V;
    match column {
        ColumnValues::Boolean(c) => c.push(match value {
            V::Bool(b) => Some(b),
            _ => None,
        }),
        ColumnValues::Int32(c) => c.push(match value {
            V::Int(i) => i32::try_from(i).ok(),
            _ => None,
        }),
        ColumnValues::Int64(c)
        | ColumnValues::TimestampMicros(c)
        | ColumnValues::TimestampNanos(c) => c.push(match value {
            V::Int(i) => Some(i),
            _ => None,
        }),
        ColumnValues::Float(c) => c.push(match value {
            #[allow(clippy::cast_possible_truncation, reason = "a FLOAT column narrows")]
            V::Float(f) => Some(f as f32),
            _ => None,
        }),
        ColumnValues::Double(c) => c.push(match value {
            V::Float(f) => Some(f),
            _ => None,
        }),
        ColumnValues::Utf8(c) => c.push(match value {
            V::Str(s) => Some(s),
            _ => None,
        }),
        ColumnValues::Binary(c) | ColumnValues::FixedLenByteArray(c) => c.push(match value {
            V::Bytes(b) => Some(b),
            _ => None,
        }),
        ColumnValues::UInt8(c) => c.push(unsigned(&value).and_then(|u| u8::try_from(u).ok())),
        ColumnValues::UInt16(c) => c.push(unsigned(&value).and_then(|u| u16::try_from(u).ok())),
        ColumnValues::UInt32(c) => c.push(unsigned(&value).and_then(|u| u32::try_from(u).ok())),
        ColumnValues::UInt64(c) => c.push(unsigned(&value)),
        ColumnValues::Decimal(c) => c.push(match value {
            V::Decimal(d) => Some(d),
            _ => None,
        }),
        ColumnValues::Map(c) => c.push(match value {
            V::Map(entries) => Some(
                entries
                    .into_iter()
                    .map(|(k, v)| (k.into(), v.map(Into::into)))
                    .collect(),
            ),
            _ => None,
        }),
    }
}

/// `values` (of a `column_type` column) as the column engine decodes them
/// from Parquet (`storage::column::decode`): integers and unsigned values
/// as `Int` (a `UINT_64` above `i64::MAX` is an error, as there), a
/// decimal as [`Decimal::to_f64`], bytes as lowercase hex.
#[cfg(feature = "engine-column")]
pub(crate) fn engine_values(
    values: &ColumnValues,
    column_type: ColumnType,
) -> Result<Vec<Value>, String> {
    fn map<T>(v: &[Option<T>], f: impl Fn(&T) -> Value) -> Vec<Value> {
        v.iter()
            .map(|x| x.as_ref().map_or(Value::Null, &f))
            .collect()
    }
    Ok(match values {
        ColumnValues::Boolean(v) => map(v, |b| Value::Bool(*b)),
        ColumnValues::Int32(v) => map(v, |i| Value::Int(i64::from(*i))),
        ColumnValues::Int64(v)
        | ColumnValues::TimestampMicros(v)
        | ColumnValues::TimestampNanos(v) => map(v, |i| Value::Int(*i)),
        ColumnValues::Float(v) => map(v, |f| Value::Float(f64::from(*f))),
        ColumnValues::Double(v) => map(v, |f| Value::Float(*f)),
        ColumnValues::Utf8(v) => map(v, |s| Value::Str(s.clone().into())),
        ColumnValues::Binary(v) | ColumnValues::FixedLenByteArray(v) => {
            map(v, |b| Value::Str(hex(b).into()))
        }
        ColumnValues::UInt8(v) => map(v, |u| Value::Int(i64::from(*u))),
        ColumnValues::UInt16(v) => map(v, |u| Value::Int(i64::from(*u))),
        ColumnValues::UInt32(v) => map(v, |u| Value::Int(i64::from(*u))),
        ColumnValues::UInt64(v) => v
            .iter()
            .map(|x| match x {
                None => Ok(Value::Null),
                Some(u) => i64::try_from(*u)
                    .map(Value::Int)
                    .map_err(|_| FileError::UnsignedOutOfRange(*u).to_string()),
            })
            .collect::<Result<_, _>>()?,
        ColumnValues::Map(v) => map(v, |entries| Value::Map(Arc::new(entries.clone()))),
        // Exact, as a flushed file's DECIMAL reads (#632).
        ColumnValues::Decimal(v) => {
            let scale = match column_type {
                ColumnType::Decimal { scale, .. } => scale,
                _ => 0,
            };
            map(v, |unscaled| Value::Decimal {
                unscaled: *unscaled,
                scale,
            })
        }
    })
}

/// `values` at the positions in `order` (each in range).
#[cfg(feature = "ingest")]
pub(crate) fn gather(values: &ColumnValues, order: &[usize]) -> ColumnValues {
    fn pick<T: Clone>(v: &[Option<T>], order: &[usize]) -> Vec<Option<T>> {
        order.iter().map(|&i| v.get(i).cloned().flatten()).collect()
    }
    match values {
        ColumnValues::Boolean(v) => ColumnValues::Boolean(pick(v, order)),
        ColumnValues::Int32(v) => ColumnValues::Int32(pick(v, order)),
        ColumnValues::Int64(v) => ColumnValues::Int64(pick(v, order)),
        ColumnValues::TimestampMicros(v) => ColumnValues::TimestampMicros(pick(v, order)),
        ColumnValues::TimestampNanos(v) => ColumnValues::TimestampNanos(pick(v, order)),
        ColumnValues::Float(v) => ColumnValues::Float(pick(v, order)),
        ColumnValues::Double(v) => ColumnValues::Double(pick(v, order)),
        ColumnValues::Utf8(v) => ColumnValues::Utf8(pick(v, order)),
        ColumnValues::Binary(v) => ColumnValues::Binary(pick(v, order)),
        ColumnValues::FixedLenByteArray(v) => ColumnValues::FixedLenByteArray(pick(v, order)),
        ColumnValues::UInt8(v) => ColumnValues::UInt8(pick(v, order)),
        ColumnValues::UInt16(v) => ColumnValues::UInt16(pick(v, order)),
        ColumnValues::UInt32(v) => ColumnValues::UInt32(pick(v, order)),
        ColumnValues::UInt64(v) => ColumnValues::UInt64(pick(v, order)),
        ColumnValues::Decimal(v) => ColumnValues::Decimal(pick(v, order)),
        ColumnValues::Map(v) => ColumnValues::Map(pick(v, order)),
    }
}

/// The values of `parts` (all of `column_type`) one after another.
#[cfg(feature = "ingest")]
pub(crate) fn concat(column_type: ColumnType, parts: &[&ColumnValues]) -> ColumnValues {
    let mut out = ColumnValues::empty(column_type);
    for part in parts {
        out.append((*part).clone());
    }
    out
}

/// Rows `a` and `b` of `values` in a tiered table's sort order: NULL
/// first, then by value (unsigned values unsigned, decimals by unscaled
/// value, floats in IEEE total order, bytes byte-wise).
#[cfg(feature = "ingest")]
pub(crate) fn compare_rows(values: &ColumnValues, a: usize, b: usize) -> Ordering {
    compare_across(values, a, values, b)
}

/// Row `a` of `x` against row `b` of `y` (two chunks of one column, so of
/// one type) in [`compare_rows`]' order; chunks of different types compare
/// equal.
#[cfg(feature = "ingest")]
pub(crate) fn compare_across(x: &ColumnValues, a: usize, y: &ColumnValues, b: usize) -> Ordering {
    fn by<T>(
        v: &[Option<T>],
        a: usize,
        w: &[Option<T>],
        b: usize,
        cmp: impl Fn(&T, &T) -> Ordering,
    ) -> Ordering {
        match (
            v.get(a).and_then(Option::as_ref),
            w.get(b).and_then(Option::as_ref),
        ) {
            (Some(p), Some(q)) => cmp(p, q),
            (p, q) => p.is_some().cmp(&q.is_some()),
        }
    }
    use ColumnValues as C;
    match (x, y) {
        (C::Boolean(v), C::Boolean(w)) => by(v, a, w, b, Ord::cmp),
        (C::Int32(v), C::Int32(w)) => by(v, a, w, b, Ord::cmp),
        (C::Int64(v), C::Int64(w))
        | (C::TimestampMicros(v), C::TimestampMicros(w))
        | (C::TimestampNanos(v), C::TimestampNanos(w)) => by(v, a, w, b, Ord::cmp),
        (C::Float(v), C::Float(w)) => by(v, a, w, b, f32::total_cmp),
        (C::Double(v), C::Double(w)) => by(v, a, w, b, f64::total_cmp),
        (C::Utf8(v), C::Utf8(w)) => by(v, a, w, b, Ord::cmp),
        (C::Binary(v), C::Binary(w)) | (C::FixedLenByteArray(v), C::FixedLenByteArray(w)) => {
            by(v, a, w, b, Ord::cmp)
        }
        (C::UInt8(v), C::UInt8(w)) => by(v, a, w, b, Ord::cmp),
        (C::UInt16(v), C::UInt16(w)) => by(v, a, w, b, Ord::cmp),
        (C::UInt32(v), C::UInt32(w)) => by(v, a, w, b, Ord::cmp),
        (C::UInt64(v), C::UInt64(w)) => by(v, a, w, b, Ord::cmp),
        (C::Decimal(v), C::Decimal(w)) => by(v, a, w, b, Ord::cmp),
        (C::Map(v), C::Map(w)) => by(v, a, w, b, Ord::cmp),
        _ => Ordering::Equal,
    }
}

/// Field `field` of a record table's file, read back as the
/// `column_type` it was written as (a day merge's input).
#[cfg(feature = "ingest")]
pub(crate) fn read_column(
    rg: &RowGroupReader<'_, '_>,
    field: &Field,
    column_type: ColumnType,
) -> Result<ColumnValues, FileError> {
    let index = field.leaves.start;
    fn bits32<T: TryFrom<u32>>(v: Vec<Option<i32>>) -> Vec<Option<T>> {
        v.into_iter()
            .map(|x| x.and_then(|x| T::try_from(x.cast_unsigned()).ok()))
            .collect()
    }
    Ok(match column_type {
        ColumnType::Boolean => ColumnValues::Boolean(rg.read_boolean_column(index)?),
        ColumnType::Int32 => ColumnValues::Int32(rg.read_int32_column(index)?),
        ColumnType::Int64 => ColumnValues::Int64(rg.read_int64_column(index)?),
        ColumnType::TimestampMicros => ColumnValues::TimestampMicros(rg.read_int64_column(index)?),
        ColumnType::TimestampNanos => ColumnValues::TimestampNanos(rg.read_int64_column(index)?),
        ColumnType::Float => ColumnValues::Float(rg.read_float_column(index)?),
        ColumnType::Double => ColumnValues::Double(rg.read_double_column(index)?),
        ColumnType::Utf8 => ColumnValues::Utf8(rg.read_string_column(index)?),
        ColumnType::Binary => ColumnValues::Binary(rg.read_binary_column(index)?),
        ColumnType::FixedLenByteArray(_) => {
            ColumnValues::FixedLenByteArray(rg.read_fixed_len_byte_array_column(index)?)
        }
        ColumnType::UInt8 => ColumnValues::UInt8(bits32(rg.read_int32_column(index)?)),
        ColumnType::UInt16 => ColumnValues::UInt16(bits32(rg.read_int32_column(index)?)),
        ColumnType::UInt32 => ColumnValues::UInt32(bits32(rg.read_int32_column(index)?)),
        ColumnType::UInt64 => ColumnValues::UInt64(
            rg.read_int64_column(index)?
                .into_iter()
                .map(|x| x.map(i64::cast_unsigned))
                .collect(),
        ),
        ColumnType::MapUtf8 => ColumnValues::Map(rg.read_map_utf8_column(field)?),
        ColumnType::Decimal { .. } => ColumnValues::Decimal(
            rg.read_decimal_column(index)?
                .into_iter()
                .map(|d| d.map(|d| d.unscaled))
                .collect(),
        ),
    })
}

#[cfg(test)]
mod tests;
