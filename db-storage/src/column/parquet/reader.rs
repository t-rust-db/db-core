//! PLAIN-encoded column value readers for Parquet DATA_PAGE (v1) pages.
//!
//! A data page body is laid out as:
//! `[definition levels (if max_def_level > 0)][PLAIN-encoded values]`
//! (repetition levels are omitted here — nested/repeated schemas are out of
//! scope for this reader). Definition levels use the RLE/Bit-Packed Hybrid
//! format with a 4-byte little-endian length prefix.

use crate::column::parquet::encoding::{self, EncodingError};
use crate::column::parquet::page::{DataPageHeader, Encoding};
use std::fmt;

#[derive(Debug)]
pub enum ReadError {
    Encoding(EncodingError),
    UnexpectedEof,
    InvalidUtf8,
    DictionaryIndexOutOfRange(u32),
    UnsupportedNestedPhysicalType(crate::column::parquet::footer::PhysicalType),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::Encoding(e) => write!(f, "{e}"),
            ReadError::UnexpectedEof => write!(f, "unexpected end of page data"),
            ReadError::InvalidUtf8 => write!(f, "BYTE_ARRAY value is not valid UTF-8"),
            ReadError::DictionaryIndexOutOfRange(i) => {
                write!(f, "dictionary index {i} out of range")
            }
            ReadError::UnsupportedNestedPhysicalType(t) => {
                write!(f, "unsupported physical type in a nested column: {t:?}")
            }
        }
    }
}

impl std::error::Error for ReadError {}

impl From<EncodingError> for ReadError {
    fn from(e: EncodingError) -> Self {
        ReadError::Encoding(e)
    }
}

pub type Result<T> = std::result::Result<T, ReadError>;

/// Split a data page body into `(definition_levels, value_bytes)`.
/// `definition_levels` is empty when `max_def_level == 0` (all values
/// present, no null tracking needed).
fn split_definition_levels<'a>(
    page_body: &'a [u8],
    header: &DataPageHeader,
    max_def_level: u32,
) -> Result<(Vec<u32>, &'a [u8])> {
    if max_def_level == 0 {
        return Ok((Vec::new(), page_body));
    }
    let (levels, rest) = read_level_section(page_body, max_def_level, header.num_values as usize)?;
    Ok((levels, rest))
}

/// One RLE/Bit-Packed Hybrid level section: a 4-byte little-endian length
/// prefix followed by that many bytes of level data.
fn read_level_section(buf: &[u8], max_level: u32, num_values: usize) -> Result<(Vec<u32>, &[u8])> {
    let len_bytes = buf.get(0..4).ok_or(ReadError::UnexpectedEof)?;
    let len = u32::from_le_bytes([len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]]) as usize;
    let levels_bytes = buf.get(4..4 + len).ok_or(ReadError::UnexpectedEof)?;
    let bit_width = encoding::bit_width_for_max_level(max_level);
    let levels = encoding::decode_hybrid_rle_bitpacked(levels_bytes, bit_width, num_values)?;
    Ok((levels, &buf[4 + len..]))
}

/// Split a nested/repeated data page body into
/// `(repetition_levels, definition_levels, value_bytes)`. Either level
/// vector is empty when its corresponding max level is 0 (e.g. a leaf with
/// no repeated ancestors has no repetition levels at all).
pub fn split_rep_def_levels<'a>(
    page_body: &'a [u8],
    header: &DataPageHeader,
    max_rep_level: u32,
    max_def_level: u32,
) -> Result<(Vec<u32>, Vec<u32>, &'a [u8])> {
    let num_values = header.num_values as usize;
    let mut buf = page_body;
    let rep_levels = if max_rep_level == 0 {
        Vec::new()
    } else {
        let (levels, rest) = read_level_section(buf, max_rep_level, num_values)?;
        buf = rest;
        levels
    };
    let def_levels = if max_def_level == 0 {
        Vec::new()
    } else {
        let (levels, rest) = read_level_section(buf, max_def_level, num_values)?;
        buf = rest;
        levels
    };
    Ok((rep_levels, def_levels, buf))
}

/// A leaf scalar value, decoded from a nested/repeated column's PLAIN data
/// (dictionary-encoded nested leaves aren't supported yet, see #89).
#[derive(Debug, Clone, PartialEq)]
pub enum LeafScalar {
    Int32(i32),
    Int64(i64),
    Float(f32),
    Double(f64),
    Bool(bool),
    Str(String),
}

/// Read one PLAIN-encoded scalar of `physical_type` from `buf`, advancing
/// it past the value. `bit_pos` tracks the bit cursor for `Boolean`, which
/// is bit-packed rather than byte-aligned.
pub fn read_plain_scalar(
    buf: &mut &[u8],
    physical_type: crate::column::parquet::footer::PhysicalType,
    bit_pos: &mut usize,
) -> Result<LeafScalar> {
    use crate::column::parquet::footer::PhysicalType;
    match physical_type {
        PhysicalType::Int32 => Ok(LeafScalar::Int32(read_plain_i32(buf)?)),
        PhysicalType::Int64 => Ok(LeafScalar::Int64(read_plain_i64(buf)?)),
        PhysicalType::Float => Ok(LeafScalar::Float(read_plain_f32(buf)?)),
        PhysicalType::Double => Ok(LeafScalar::Double(read_plain_f64(buf)?)),
        PhysicalType::ByteArray => Ok(LeafScalar::Str(read_plain_string(buf)?)),
        PhysicalType::Boolean => {
            let byte = *buf.first().ok_or(ReadError::UnexpectedEof)?;
            let bit = (byte >> (*bit_pos % 8)) & 1;
            *bit_pos += 1;
            if (*bit_pos).is_multiple_of(8) {
                *buf = &buf[1..];
            }
            Ok(LeafScalar::Bool(bit == 1))
        }
        other => Err(ReadError::UnsupportedNestedPhysicalType(other)),
    }
}

/// Walk `definition_levels` (or, if empty, assume every value is present),
/// pulling one value from `values` via `read_value` for each non-null slot.
fn assemble<T>(
    header: &DataPageHeader,
    definition_levels: &[u32],
    max_def_level: u32,
    mut values: &[u8],
    mut read_value: impl FnMut(&mut &[u8]) -> Result<T>,
) -> Result<Vec<Option<T>>> {
    let num_values = header.num_values as usize;
    let mut out = Vec::with_capacity(num_values);
    if definition_levels.is_empty() {
        for _ in 0..num_values {
            out.push(Some(read_value(&mut values)?));
        }
    } else {
        for present in encoding::null_mask(definition_levels, max_def_level) {
            if present {
                out.push(Some(read_value(&mut values)?));
            } else {
                out.push(None);
            }
        }
    }
    Ok(out)
}

/// Like [`take`], but for a fixed-width field: returns the bytes as an
/// array so the `from_le_bytes` callers need no fallible conversion.
fn take_array<const N: usize>(buf: &mut &[u8]) -> Result<[u8; N]> {
    take(buf, N)?
        .try_into()
        .map_err(|_| ReadError::UnexpectedEof)
}

fn take<'a>(buf: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
    if buf.len() < n {
        return Err(ReadError::UnexpectedEof);
    }
    let (head, tail) = buf.split_at(n);
    *buf = tail;
    Ok(head)
}

fn read_plain_i64(buf: &mut &[u8]) -> Result<i64> {
    Ok(i64::from_le_bytes(take_array(buf)?))
}

fn read_plain_f64(buf: &mut &[u8]) -> Result<f64> {
    Ok(f64::from_le_bytes(take_array(buf)?))
}

fn read_plain_string(buf: &mut &[u8]) -> Result<String> {
    let len = u32::from_le_bytes(take_array(buf)?) as usize;
    let bytes = take(buf, len)?;
    String::from_utf8(bytes.to_vec()).map_err(|_| ReadError::InvalidUtf8)
}

fn read_plain_i32(buf: &mut &[u8]) -> Result<i32> {
    Ok(i32::from_le_bytes(take_array(buf)?))
}

fn read_plain_f32(buf: &mut &[u8]) -> Result<f32> {
    Ok(f32::from_le_bytes(take_array(buf)?))
}

fn read_plain_fixed_len_byte_array(buf: &mut &[u8], len: usize) -> Result<Vec<u8>> {
    Ok(take(buf, len)?.to_vec())
}

/// A raw INT96 value (legacy nanosecond-precision timestamp encoding some
/// writers still emit): a Julian day number plus nanoseconds within that day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Int96 {
    pub julian_day: i32,
    pub time_nanos: i64,
}

fn read_plain_int96(buf: &mut &[u8]) -> Result<Int96> {
    let time_nanos = i64::from_le_bytes(take_array(buf)?);
    let julian_day = i32::from_le_bytes(take_array(buf)?);
    Ok(Int96 {
        julian_day,
        time_nanos,
    })
}

/// Decode a PLAIN-encoded INT64 dictionary page body into its `num_values` entries.
pub fn decode_dictionary_int64(dictionary_body: &[u8], num_values: i32) -> Result<Vec<i64>> {
    let mut buf = dictionary_body;
    (0..num_values).map(|_| read_plain_i64(&mut buf)).collect()
}

/// Decode a PLAIN-encoded DOUBLE dictionary page body into its `num_values` entries.
pub fn decode_dictionary_double(dictionary_body: &[u8], num_values: i32) -> Result<Vec<f64>> {
    let mut buf = dictionary_body;
    (0..num_values).map(|_| read_plain_f64(&mut buf)).collect()
}

/// Decode a bit-packed BOOLEAN dictionary page body into its `num_values` entries.
pub fn decode_dictionary_boolean(dictionary_body: &[u8], num_values: i32) -> Result<Vec<bool>> {
    (0..num_values as usize)
        .map(|i| {
            let byte = *dictionary_body.get(i / 8).ok_or(ReadError::UnexpectedEof)?;
            Ok((byte >> (i % 8)) & 1 == 1)
        })
        .collect()
}

/// Decode a PLAIN-encoded BYTE_ARRAY (string) dictionary page body into its `num_values` entries.
pub fn decode_dictionary_string(dictionary_body: &[u8], num_values: i32) -> Result<Vec<String>> {
    let mut buf = dictionary_body;
    (0..num_values)
        .map(|_| read_plain_string(&mut buf))
        .collect()
}

/// Decode a PLAIN-encoded INT32 dictionary page body into its `num_values` entries.
pub fn decode_dictionary_int32(dictionary_body: &[u8], num_values: i32) -> Result<Vec<i32>> {
    let mut buf = dictionary_body;
    (0..num_values).map(|_| read_plain_i32(&mut buf)).collect()
}

/// Decode a PLAIN-encoded FLOAT dictionary page body into its `num_values` entries.
pub fn decode_dictionary_float(dictionary_body: &[u8], num_values: i32) -> Result<Vec<f32>> {
    let mut buf = dictionary_body;
    (0..num_values).map(|_| read_plain_f32(&mut buf)).collect()
}

/// Decode a PLAIN-encoded FIXED_LEN_BYTE_ARRAY dictionary page body into its `num_values` entries.
pub fn decode_dictionary_fixed_len_byte_array(
    dictionary_body: &[u8],
    num_values: i32,
    type_length: usize,
) -> Result<Vec<Vec<u8>>> {
    let mut buf = dictionary_body;
    (0..num_values)
        .map(|_| read_plain_fixed_len_byte_array(&mut buf, type_length))
        .collect()
}

/// Read the dictionary-index stream of a dictionary-encoded data page:
/// a 1-byte bit-width followed by the RLE/Bit-Packed Hybrid encoding of
/// `num_indices` index values (one per non-null value in the page).
fn read_dictionary_indices(data: &[u8], num_indices: usize) -> Result<Vec<u32>> {
    let bit_width = *data.first().ok_or(ReadError::UnexpectedEof)? as u32;
    let rest = data.get(1..).ok_or(ReadError::UnexpectedEof)?;
    Ok(encoding::decode_hybrid_rle_bitpacked(
        rest,
        bit_width,
        num_indices,
    )?)
}

/// Map `definition_levels` (as in [`assemble`]) to dictionary-indexed values,
/// pulling one index from `indices` for each non-null slot and resolving it
/// against `dictionary`.
fn assemble_dictionary<T: Clone>(
    header: &DataPageHeader,
    definition_levels: &[u32],
    max_def_level: u32,
    indices: &[u32],
    dictionary: &[T],
) -> Result<Vec<Option<T>>> {
    let num_values = header.num_values as usize;
    let mut out = Vec::with_capacity(num_values);
    let mut idx_iter = indices.iter();
    let presents: Vec<bool> = if definition_levels.is_empty() {
        vec![true; num_values]
    } else {
        encoding::null_mask(definition_levels, max_def_level)
    };
    for present in presents {
        if present {
            let idx = *idx_iter.next().ok_or(ReadError::UnexpectedEof)?;
            let value = dictionary
                .get(idx as usize)
                .cloned()
                .ok_or(ReadError::DictionaryIndexOutOfRange(idx))?;
            out.push(Some(value));
        } else {
            out.push(None);
        }
    }
    Ok(out)
}

/// Count of non-null (present) slots described by `definition_levels`, or
/// `header.num_values` when there are no definition levels (all present).
fn present_count(header: &DataPageHeader, definition_levels: &[u32], max_def_level: u32) -> usize {
    if definition_levels.is_empty() {
        header.num_values as usize
    } else {
        encoding::null_mask(definition_levels, max_def_level)
            .into_iter()
            .filter(|&p| p)
            .count()
    }
}

/// Read a dictionary-encoded data page's raw dictionary indices, *without*
/// resolving them against the dictionary — for callers (like a query
/// engine's `GROUP BY`/equality comparisons) that only need to compare
/// values, where an index compare-and-group is far cheaper than allocating
/// and comparing e.g. a `String` per row. Pair the result with the
/// dictionary from `decode_dictionary_*` to resolve a specific row on
/// demand.
pub fn read_dictionary_index_column(
    page_body: &[u8],
    header: &DataPageHeader,
    max_def_level: u32,
) -> Result<Vec<Option<u32>>> {
    if header.num_values == 0 {
        return Ok(Vec::new());
    }
    let (levels, index_bytes) = split_definition_levels(page_body, header, max_def_level)?;
    let indices =
        read_dictionary_indices(index_bytes, present_count(header, &levels, max_def_level))?;
    let presents: Vec<bool> = if levels.is_empty() {
        vec![true; header.num_values as usize]
    } else {
        encoding::null_mask(&levels, max_def_level)
    };
    let mut idx_iter = indices.into_iter();
    presents
        .into_iter()
        .map(|present| {
            if present {
                Ok(Some(idx_iter.next().ok_or(ReadError::UnexpectedEof)?))
            } else {
                Ok(None)
            }
        })
        .collect()
}

/// Read an INT64 column from a single data page, either PLAIN- or
/// DELTA_BINARY_PACKED-encoded (dispatched on `header.encoding`).
pub fn read_int64_column(
    page_body: &[u8],
    header: &DataPageHeader,
    max_def_level: u32,
) -> Result<Vec<Option<i64>>> {
    if header.num_values == 0 {
        return Ok(Vec::new());
    }
    let (levels, values) = split_definition_levels(page_body, header, max_def_level)?;
    if header.encoding == Encoding::DeltaBinaryPacked {
        let decoded = encoding::decode_delta_binary_packed(values)?;
        let mut decoded = decoded.into_iter();
        let presents: Vec<bool> = if levels.is_empty() {
            vec![true; header.num_values as usize]
        } else {
            encoding::null_mask(&levels, max_def_level)
        };
        presents
            .into_iter()
            .map(|present| {
                if present {
                    Ok(Some(decoded.next().ok_or(ReadError::UnexpectedEof)?))
                } else {
                    Ok(None)
                }
            })
            .collect()
    } else {
        assemble(header, &levels, max_def_level, values, read_plain_i64)
    }
}

/// Read a PLAIN_DICTIONARY/RLE_DICTIONARY-encoded INT64 column from a single
/// data page, given its already-decoded dictionary.
pub fn read_int64_column_dictionary(
    page_body: &[u8],
    header: &DataPageHeader,
    max_def_level: u32,
    dictionary: &[i64],
) -> Result<Vec<Option<i64>>> {
    if header.num_values == 0 {
        return Ok(Vec::new());
    }
    let (levels, index_bytes) = split_definition_levels(page_body, header, max_def_level)?;
    let indices =
        read_dictionary_indices(index_bytes, present_count(header, &levels, max_def_level))?;
    assemble_dictionary(header, &levels, max_def_level, &indices, dictionary)
}

/// Read a PLAIN-encoded DOUBLE column from a single data page.
pub fn read_double_column(
    page_body: &[u8],
    header: &DataPageHeader,
    max_def_level: u32,
) -> Result<Vec<Option<f64>>> {
    if header.num_values == 0 {
        return Ok(Vec::new());
    }
    let (levels, values) = split_definition_levels(page_body, header, max_def_level)?;
    assemble(header, &levels, max_def_level, values, read_plain_f64)
}

/// Read a PLAIN_DICTIONARY/RLE_DICTIONARY-encoded DOUBLE column from a single
/// data page, given its already-decoded dictionary.
pub fn read_double_column_dictionary(
    page_body: &[u8],
    header: &DataPageHeader,
    max_def_level: u32,
    dictionary: &[f64],
) -> Result<Vec<Option<f64>>> {
    if header.num_values == 0 {
        return Ok(Vec::new());
    }
    let (levels, index_bytes) = split_definition_levels(page_body, header, max_def_level)?;
    let indices =
        read_dictionary_indices(index_bytes, present_count(header, &levels, max_def_level))?;
    assemble_dictionary(header, &levels, max_def_level, &indices, dictionary)
}

/// Read a bit-packed BOOLEAN column from a single data page. Booleans are
/// packed 8-per-byte, LSB first, counting only the non-null values.
pub fn read_boolean_column(
    page_body: &[u8],
    header: &DataPageHeader,
    max_def_level: u32,
) -> Result<Vec<Option<bool>>> {
    if header.num_values == 0 {
        return Ok(Vec::new());
    }
    let (levels, values) = split_definition_levels(page_body, header, max_def_level)?;
    let mut bit_pos = 0usize;
    let read_bit = move |buf: &mut &[u8]| -> Result<bool> {
        let byte = *values.get(bit_pos / 8).ok_or(ReadError::UnexpectedEof)?;
        let bit = (byte >> (bit_pos % 8)) & 1;
        bit_pos += 1;
        let _ = buf; // values are consumed via the shared bit cursor, not `buf`
        Ok(bit == 1)
    };
    assemble(header, &levels, max_def_level, &[][..], read_bit)
}

/// Read a PLAIN_DICTIONARY/RLE_DICTIONARY-encoded BOOLEAN column from a
/// single data page, given its already-decoded dictionary.
pub fn read_boolean_column_dictionary(
    page_body: &[u8],
    header: &DataPageHeader,
    max_def_level: u32,
    dictionary: &[bool],
) -> Result<Vec<Option<bool>>> {
    if header.num_values == 0 {
        return Ok(Vec::new());
    }
    let (levels, index_bytes) = split_definition_levels(page_body, header, max_def_level)?;
    let indices =
        read_dictionary_indices(index_bytes, present_count(header, &levels, max_def_level))?;
    assemble_dictionary(header, &levels, max_def_level, &indices, dictionary)
}

/// Read a PLAIN-encoded BYTE_ARRAY (string) column: each value is a 4-byte
/// little-endian length prefix followed by that many UTF-8 bytes.
pub fn read_string_column(
    page_body: &[u8],
    header: &DataPageHeader,
    max_def_level: u32,
) -> Result<Vec<Option<String>>> {
    if header.num_values == 0 {
        return Ok(Vec::new());
    }
    let (levels, values) = split_definition_levels(page_body, header, max_def_level)?;
    assemble(header, &levels, max_def_level, values, read_plain_string)
}

/// Read a PLAIN_DICTIONARY/RLE_DICTIONARY-encoded BYTE_ARRAY (string) column
/// from a single data page, given its already-decoded dictionary.
pub fn read_string_column_dictionary(
    page_body: &[u8],
    header: &DataPageHeader,
    max_def_level: u32,
    dictionary: &[String],
) -> Result<Vec<Option<String>>> {
    if header.num_values == 0 {
        return Ok(Vec::new());
    }
    let (levels, index_bytes) = split_definition_levels(page_body, header, max_def_level)?;
    let indices =
        read_dictionary_indices(index_bytes, present_count(header, &levels, max_def_level))?;
    assemble_dictionary(header, &levels, max_def_level, &indices, dictionary)
}

/// Read a PLAIN-encoded INT32 column from a single data page.
pub fn read_int32_column(
    page_body: &[u8],
    header: &DataPageHeader,
    max_def_level: u32,
) -> Result<Vec<Option<i32>>> {
    if header.num_values == 0 {
        return Ok(Vec::new());
    }
    let (levels, values) = split_definition_levels(page_body, header, max_def_level)?;
    assemble(header, &levels, max_def_level, values, read_plain_i32)
}

/// Read a PLAIN_DICTIONARY/RLE_DICTIONARY-encoded INT32 column from a single
/// data page, given its already-decoded dictionary.
pub fn read_int32_column_dictionary(
    page_body: &[u8],
    header: &DataPageHeader,
    max_def_level: u32,
    dictionary: &[i32],
) -> Result<Vec<Option<i32>>> {
    if header.num_values == 0 {
        return Ok(Vec::new());
    }
    let (levels, index_bytes) = split_definition_levels(page_body, header, max_def_level)?;
    let indices =
        read_dictionary_indices(index_bytes, present_count(header, &levels, max_def_level))?;
    assemble_dictionary(header, &levels, max_def_level, &indices, dictionary)
}

/// Read a PLAIN-encoded FLOAT column from a single data page.
pub fn read_float_column(
    page_body: &[u8],
    header: &DataPageHeader,
    max_def_level: u32,
) -> Result<Vec<Option<f32>>> {
    if header.num_values == 0 {
        return Ok(Vec::new());
    }
    let (levels, values) = split_definition_levels(page_body, header, max_def_level)?;
    assemble(header, &levels, max_def_level, values, read_plain_f32)
}

/// Read a PLAIN_DICTIONARY/RLE_DICTIONARY-encoded FLOAT column from a single
/// data page, given its already-decoded dictionary.
pub fn read_float_column_dictionary(
    page_body: &[u8],
    header: &DataPageHeader,
    max_def_level: u32,
    dictionary: &[f32],
) -> Result<Vec<Option<f32>>> {
    if header.num_values == 0 {
        return Ok(Vec::new());
    }
    let (levels, index_bytes) = split_definition_levels(page_body, header, max_def_level)?;
    let indices =
        read_dictionary_indices(index_bytes, present_count(header, &levels, max_def_level))?;
    assemble_dictionary(header, &levels, max_def_level, &indices, dictionary)
}

/// Read a PLAIN-encoded FIXED_LEN_BYTE_ARRAY column from a single data page;
/// `type_length` is the schema's fixed byte width for this column.
pub fn read_fixed_len_byte_array_column(
    page_body: &[u8],
    header: &DataPageHeader,
    max_def_level: u32,
    type_length: usize,
) -> Result<Vec<Option<Vec<u8>>>> {
    if header.num_values == 0 {
        return Ok(Vec::new());
    }
    let (levels, values) = split_definition_levels(page_body, header, max_def_level)?;
    assemble(header, &levels, max_def_level, values, |buf| {
        read_plain_fixed_len_byte_array(buf, type_length)
    })
}

/// Read a PLAIN_DICTIONARY/RLE_DICTIONARY-encoded FIXED_LEN_BYTE_ARRAY column
/// from a single data page, given its already-decoded dictionary.
pub fn read_fixed_len_byte_array_column_dictionary(
    page_body: &[u8],
    header: &DataPageHeader,
    max_def_level: u32,
    dictionary: &[Vec<u8>],
) -> Result<Vec<Option<Vec<u8>>>> {
    if header.num_values == 0 {
        return Ok(Vec::new());
    }
    let (levels, index_bytes) = split_definition_levels(page_body, header, max_def_level)?;
    let indices =
        read_dictionary_indices(index_bytes, present_count(header, &levels, max_def_level))?;
    assemble_dictionary(header, &levels, max_def_level, &indices, dictionary)
}

/// Read a PLAIN-encoded INT96 column from a single data page.
pub fn read_int96_column(
    page_body: &[u8],
    header: &DataPageHeader,
    max_def_level: u32,
) -> Result<Vec<Option<Int96>>> {
    if header.num_values == 0 {
        return Ok(Vec::new());
    }
    let (levels, values) = split_definition_levels(page_body, header, max_def_level)?;
    assemble(header, &levels, max_def_level, values, read_plain_int96)
}

/// Decode a PLAIN-encoded INT96 dictionary page body into its `num_values` entries.
pub fn decode_dictionary_int96(dictionary_body: &[u8], num_values: i32) -> Result<Vec<Int96>> {
    let mut buf = dictionary_body;
    (0..num_values)
        .map(|_| read_plain_int96(&mut buf))
        .collect()
}

/// Read a PLAIN_DICTIONARY/RLE_DICTIONARY-encoded INT96 column from a single
/// data page, given its already-decoded dictionary.
pub fn read_int96_column_dictionary(
    page_body: &[u8],
    header: &DataPageHeader,
    max_def_level: u32,
    dictionary: &[Int96],
) -> Result<Vec<Option<Int96>>> {
    if header.num_values == 0 {
        return Ok(Vec::new());
    }
    let (levels, index_bytes) = split_definition_levels(page_body, header, max_def_level)?;
    let indices =
        read_dictionary_indices(index_bytes, present_count(header, &levels, max_def_level))?;
    assemble_dictionary(header, &levels, max_def_level, &indices, dictionary)
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
    use crate::column::parquet::page::Encoding;

    fn header(num_values: i32) -> DataPageHeader {
        DataPageHeader {
            num_values,
            encoding: Encoding::Plain,
            definition_level_encoding: Encoding::Rle,
            repetition_level_encoding: Encoding::Rle,
        }
    }

    /// Encode levels as one RLE run per value (inefficient but simple),
    /// supporting arbitrary `bit_width` — used to exercise `max_def_level > 1`.
    fn encode_def_levels_rle(levels: &[u32], bit_width: u32) -> Vec<u8> {
        let value_bytes = bit_width.div_ceil(8) as usize;
        let mut body = Vec::new();
        for &level in levels {
            body.push((1u64 << 1) as u8); // RLE run of length 1
            for i in 0..value_bytes {
                body.push(((level >> (8 * i)) & 0xff) as u8);
            }
        }
        let mut out = Vec::new();
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    fn encode_def_levels(levels: &[u32]) -> Vec<u8> {
        // Simple encoder: one bit-packed run covering all levels, bit_width=1.
        let n = levels.len();
        let num_groups = n.div_ceil(8);
        let mut body = Vec::new();
        body.push(((num_groups as u64) << 1 | 1) as u8);
        for g in 0..num_groups {
            let mut byte = 0u8;
            for bit in 0..8 {
                let idx = g * 8 + bit;
                if idx < n && levels[idx] == 1 {
                    byte |= 1 << bit;
                }
            }
            body.push(byte);
        }
        let mut out = Vec::new();
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn reads_int64_no_nulls() {
        let h = header(3);
        let mut page = Vec::new();
        for v in [1i64, 2, 3] {
            page.extend_from_slice(&v.to_le_bytes());
        }
        let result = read_int64_column(&page, &h, 0).unwrap();
        assert_eq!(result, vec![Some(1), Some(2), Some(3)]);
    }

    #[test]
    fn reads_int64_with_nulls() {
        let h = header(4);
        let levels = [1, 0, 1, 1];
        let mut page = encode_def_levels(&levels);
        for v in [10i64, 20, 30] {
            page.extend_from_slice(&v.to_le_bytes());
        }
        let result = read_int64_column(&page, &h, 1).unwrap();
        assert_eq!(result, vec![Some(10), None, Some(20), Some(30)]);
    }

    #[test]
    fn reads_int64_empty_column() {
        let h = header(0);
        let result = read_int64_column(&[], &h, 1).unwrap();
        assert_eq!(result, Vec::<Option<i64>>::new());
    }

    #[test]
    fn reads_int64_all_nulls() {
        let h = header(3);
        let levels = [0, 0, 0];
        let page = encode_def_levels(&levels);
        let result = read_int64_column(&page, &h, 1).unwrap();
        assert_eq!(result, vec![None, None, None]);
    }

    #[test]
    fn reads_double_with_nulls() {
        let h = header(3);
        let levels = [1, 0, 1];
        let mut page = encode_def_levels(&levels);
        page.extend_from_slice(&1.5f64.to_le_bytes());
        page.extend_from_slice(&2.5f64.to_le_bytes());
        let result = read_double_column(&page, &h, 1).unwrap();
        assert_eq!(result, vec![Some(1.5), None, Some(2.5)]);
    }

    #[test]
    fn reads_double_empty_column() {
        let h = header(0);
        let result = read_double_column(&[], &h, 1).unwrap();
        assert_eq!(result, Vec::<Option<f64>>::new());
    }

    #[test]
    fn reads_double_all_nulls() {
        let h = header(2);
        let levels = [0, 0];
        let page = encode_def_levels(&levels);
        let result = read_double_column(&page, &h, 1).unwrap();
        assert_eq!(result, vec![None, None]);
    }

    #[test]
    fn reads_boolean_with_nulls() {
        let h = header(4);
        let levels = [1, 0, 1, 1];
        let mut page = encode_def_levels(&levels);
        // 3 non-null booleans: true, false, true -> bits 1,0,1 LSB-first
        page.push(0b0000_0101);
        let result = read_boolean_column(&page, &h, 1).unwrap();
        assert_eq!(result, vec![Some(true), None, Some(false), Some(true)]);
    }

    #[test]
    fn reads_boolean_no_nulls() {
        let h = header(8);
        let page = vec![0b1010_1010u8];
        let result = read_boolean_column(&page, &h, 0).unwrap();
        assert_eq!(
            result,
            vec![
                Some(false),
                Some(true),
                Some(false),
                Some(true),
                Some(false),
                Some(true),
                Some(false),
                Some(true)
            ]
        );
    }

    #[test]
    fn reads_boolean_empty_column() {
        let h = header(0);
        let result = read_boolean_column(&[], &h, 1).unwrap();
        assert_eq!(result, Vec::<Option<bool>>::new());
    }

    #[test]
    fn reads_boolean_all_nulls() {
        let h = header(3);
        let levels = [0, 0, 0];
        let page = encode_def_levels(&levels);
        let result = read_boolean_column(&page, &h, 1).unwrap();
        assert_eq!(result, vec![None, None, None]);
    }

    #[test]
    fn reads_string_with_nulls() {
        let h = header(3);
        let levels = [1, 0, 1];
        let mut page = encode_def_levels(&levels);
        for s in ["hi", "bye"] {
            page.extend_from_slice(&(s.len() as u32).to_le_bytes());
            page.extend_from_slice(s.as_bytes());
        }
        let result = read_string_column(&page, &h, 1).unwrap();
        assert_eq!(
            result,
            vec![Some("hi".to_string()), None, Some("bye".to_string())]
        );
    }

    #[test]
    fn reads_string_empty_column() {
        let h = header(0);
        let result = read_string_column(&[], &h, 1).unwrap();
        assert_eq!(result, Vec::<Option<String>>::new());
    }

    #[test]
    fn reads_string_all_nulls() {
        let h = header(2);
        let levels = [0, 0];
        let page = encode_def_levels(&levels);
        let result = read_string_column(&page, &h, 1).unwrap();
        assert_eq!(result, vec![None, None]);
    }

    #[test]
    fn rejects_invalid_utf8() {
        let h = header(1);
        let mut page = Vec::new();
        let bad = [0xff, 0xfe];
        page.extend_from_slice(&(bad.len() as u32).to_le_bytes());
        page.extend_from_slice(&bad);
        let result = read_string_column(&page, &h, 0);
        assert!(matches!(result, Err(ReadError::InvalidUtf8)));
    }

    /// Encode `indices` as a dictionary-index stream: 1-byte bit-width
    /// followed by one bit-packed run covering all indices.
    fn encode_dictionary_indices(indices: &[u32], bit_width: u32) -> Vec<u8> {
        let n = indices.len();
        let num_groups = n.div_ceil(8);
        let mut out = vec![bit_width as u8];
        out.push(((num_groups as u64) << 1 | 1) as u8);

        let total_bits = num_groups * 8 * bit_width as usize;
        let mut bytes = vec![0u8; total_bits.div_ceil(8)];
        let mut bit_pos = 0usize;
        for g in 0..num_groups {
            for i in 0..8 {
                let value = indices.get(g * 8 + i).copied().unwrap_or(0);
                for bit in 0..bit_width as usize {
                    if (value >> bit) & 1 == 1 {
                        let global_bit = bit_pos + bit;
                        bytes[global_bit / 8] |= 1 << (global_bit % 8);
                    }
                }
                bit_pos += bit_width as usize;
            }
        }
        out.extend_from_slice(&bytes);
        out
    }

    #[test]
    fn reads_int64_dictionary_no_nulls() {
        let h = header(4);
        let dictionary = vec![100i64, 200, 300];
        let page = encode_dictionary_indices(&[0, 1, 2, 1], 2);
        let result = read_int64_column_dictionary(&page, &h, 0, &dictionary).unwrap();
        assert_eq!(result, vec![Some(100), Some(200), Some(300), Some(200)]);
    }

    #[test]
    fn reads_int64_dictionary_with_nulls() {
        let h = header(4);
        let dictionary = vec![100i64, 200, 300];
        let levels = [1, 0, 1, 1];
        let mut page = encode_def_levels(&levels);
        page.extend_from_slice(&encode_dictionary_indices(&[0, 2, 1], 2));
        let result = read_int64_column_dictionary(&page, &h, 1, &dictionary).unwrap();
        assert_eq!(result, vec![Some(100), None, Some(300), Some(200)]);
    }

    #[test]
    fn reads_double_dictionary_no_nulls() {
        let h = header(3);
        let dictionary = vec![1.5f64, 2.5, 3.5];
        let page = encode_dictionary_indices(&[2, 0, 1], 2);
        let result = read_double_column_dictionary(&page, &h, 0, &dictionary).unwrap();
        assert_eq!(result, vec![Some(3.5), Some(1.5), Some(2.5)]);
    }

    #[test]
    fn reads_boolean_dictionary_no_nulls() {
        let h = header(4);
        let dictionary = vec![true, false];
        let page = encode_dictionary_indices(&[0, 1, 1, 0], 1);
        let result = read_boolean_column_dictionary(&page, &h, 0, &dictionary).unwrap();
        assert_eq!(
            result,
            vec![Some(true), Some(false), Some(false), Some(true)]
        );
    }

    #[test]
    fn reads_string_dictionary_with_nulls() {
        let h = header(3);
        let dictionary = vec!["north".to_string(), "south".to_string()];
        let levels = [1, 0, 1];
        let mut page = encode_def_levels(&levels);
        page.extend_from_slice(&encode_dictionary_indices(&[1, 0], 1));
        let result = read_string_column_dictionary(&page, &h, 1, &dictionary).unwrap();
        assert_eq!(
            result,
            vec![Some("south".to_string()), None, Some("north".to_string())]
        );
    }

    #[test]
    fn read_dictionary_index_column_returns_raw_indices_with_nulls() {
        let h = header(4);
        let levels = [1, 0, 1, 1];
        let mut page = encode_def_levels(&levels);
        page.extend_from_slice(&encode_dictionary_indices(&[2, 0, 1], 2));
        let result = read_dictionary_index_column(&page, &h, 1).unwrap();
        assert_eq!(result, vec![Some(2), None, Some(0), Some(1)]);
    }

    #[test]
    fn dictionary_index_out_of_range_errors() {
        let h = header(1);
        let dictionary = vec![1i64];
        let page = encode_dictionary_indices(&[5], 3);
        let result = read_int64_column_dictionary(&page, &h, 0, &dictionary);
        assert!(matches!(
            result,
            Err(ReadError::DictionaryIndexOutOfRange(5))
        ));
    }

    #[test]
    fn decodes_dictionary_page_bodies() {
        let mut int64_body = Vec::new();
        for v in [10i64, 20, 30] {
            int64_body.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(
            decode_dictionary_int64(&int64_body, 3).unwrap(),
            vec![10, 20, 30]
        );

        let mut double_body = Vec::new();
        for v in [1.5f64, 2.5] {
            double_body.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(
            decode_dictionary_double(&double_body, 2).unwrap(),
            vec![1.5, 2.5]
        );

        let bool_body = [0b0000_0101u8];
        assert_eq!(
            decode_dictionary_boolean(&bool_body, 3).unwrap(),
            vec![true, false, true]
        );

        let mut string_body = Vec::new();
        for s in ["north", "south"] {
            string_body.extend_from_slice(&(s.len() as u32).to_le_bytes());
            string_body.extend_from_slice(s.as_bytes());
        }
        assert_eq!(
            decode_dictionary_string(&string_body, 2).unwrap(),
            vec!["north".to_string(), "south".to_string()]
        );
    }

    #[test]
    fn reads_int64_with_max_def_level_above_one() {
        // Nested-schema style: only level == max_def_level (2) is present.
        let h = header(4);
        let levels = [2, 1, 0, 2];
        let mut page = encode_def_levels_rle(&levels, 2);
        for v in [10i64, 20] {
            page.extend_from_slice(&v.to_le_bytes());
        }
        let result = read_int64_column(&page, &h, 2).unwrap();
        assert_eq!(result, vec![Some(10), None, None, Some(20)]);
    }

    #[test]
    fn reads_int32_no_nulls() {
        let h = header(3);
        let mut page = Vec::new();
        for v in [1i32, -2, 3] {
            page.extend_from_slice(&v.to_le_bytes());
        }
        let result = read_int32_column(&page, &h, 0).unwrap();
        assert_eq!(result, vec![Some(1), Some(-2), Some(3)]);
    }

    #[test]
    fn reads_int32_dictionary_no_nulls() {
        let h = header(3);
        let dictionary = vec![100i32, 200, 300];
        let page = encode_dictionary_indices(&[2, 0, 1], 2);
        let result = read_int32_column_dictionary(&page, &h, 0, &dictionary).unwrap();
        assert_eq!(result, vec![Some(300), Some(100), Some(200)]);
    }

    #[test]
    fn reads_float_no_nulls() {
        let h = header(2);
        let mut page = Vec::new();
        for v in [1.5f32, -2.5] {
            page.extend_from_slice(&v.to_le_bytes());
        }
        let result = read_float_column(&page, &h, 0).unwrap();
        assert_eq!(result, vec![Some(1.5), Some(-2.5)]);
    }

    #[test]
    fn reads_fixed_len_byte_array_with_nulls() {
        let h = header(3);
        let levels = [1, 0, 1];
        let mut page = encode_def_levels(&levels);
        page.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
        let result = read_fixed_len_byte_array_column(&page, &h, 1, 2).unwrap();
        assert_eq!(
            result,
            vec![Some(vec![0xAA, 0xBB]), None, Some(vec![0xCC, 0xDD])]
        );
    }

    #[test]
    fn reads_fixed_len_byte_array_dictionary() {
        let h = header(2);
        let dictionary: Vec<Vec<u8>> = vec![vec![1, 2, 3], vec![4, 5, 6]];
        let page = encode_dictionary_indices(&[1, 0], 1);
        let result =
            read_fixed_len_byte_array_column_dictionary(&page, &h, 0, &dictionary).unwrap();
        assert_eq!(result, vec![Some(vec![4, 5, 6]), Some(vec![1, 2, 3])]);
    }

    #[test]
    fn reads_int96_no_nulls() {
        let h = header(2);
        let mut page = Vec::new();
        page.extend_from_slice(&123i64.to_le_bytes());
        page.extend_from_slice(&456i32.to_le_bytes());
        page.extend_from_slice(&(-7i64).to_le_bytes());
        page.extend_from_slice(&8i32.to_le_bytes());
        let result = read_int96_column(&page, &h, 0).unwrap();
        assert_eq!(
            result,
            vec![
                Some(Int96 {
                    julian_day: 456,
                    time_nanos: 123
                }),
                Some(Int96 {
                    julian_day: 8,
                    time_nanos: -7
                })
            ]
        );
    }
}
