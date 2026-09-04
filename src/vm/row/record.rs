//! On-disk record encoding/decoding, ported from sqlite-rs's
//! `record::{encode, decode}` (db-core#56/#59): the varint header
//! length, one varint serial type per column, then column bodies
//! back-to-back. Backs `Opcode::MakeRecord` (encode) and `Opcode::
//! Column`/the ephemeral-table cursor (decode).
//!
//! **Not ported**: `decode_record_upto`/`decode_record_only_into`/
//! `decode_single_column` (sorter-specific partial-decode fast paths --
//! no sorter exists in `vm::row` yet) and `parse_header`'s standalone
//! export (kept private here, an implementation detail of
//! `decode_record`/`decode_column`).

use super::value::{TextEncoding, Value};
use std::rc::Rc;

/// Errors from decoding a record payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordError {
    /// The record buffer ended before decoding could complete.
    UnexpectedEof { offset: usize },
    /// The declared header length is too small to contain the
    /// header-length varint itself.
    HeaderTooShort { declared: usize, varint_len: usize },
    /// A serial-type varint in the header read past the declared header
    /// length.
    HeaderOverrun { offset: usize, header_len: usize },
    /// Bytes remained in the buffer after all header-declared columns
    /// were decoded.
    TrailingData { trailing: usize },
    /// A text value's bytes were not valid UTF-8 under a UTF-8
    /// `TextEncoding`.
    InvalidUtf8,
    /// A text value's bytes were not valid UTF-16 under a UTF-16
    /// `TextEncoding`.
    InvalidUtf16,
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecordError::UnexpectedEof { offset } => {
                write!(f, "unexpected end of input at byte offset {offset}")
            }
            RecordError::HeaderTooShort {
                declared,
                varint_len,
            } => write!(
                f,
                "record header length {declared} is shorter than its own header-length varint ({varint_len} bytes)"
            ),
            RecordError::HeaderOverrun { offset, header_len } => write!(
                f,
                "record header entry at offset {offset} extends past the declared header length {header_len}"
            ),
            RecordError::TrailingData { trailing } => write!(
                f,
                "record has {trailing} unconsumed trailing byte(s) after decoding all columns"
            ),
            RecordError::InvalidUtf8 => write!(f, "invalid UTF-8 in text value"),
            RecordError::InvalidUtf16 => write!(f, "invalid UTF-16 in text value"),
        }
    }
}

impl std::error::Error for RecordError {}

/// Encodes a varint: big-endian, 7 bits per byte with a high-bit
/// continuation flag, up to 9 bytes -- always the minimal encoding (no
/// redundant continuation bytes).
#[allow(
    clippy::arithmetic_side_effects,
    reason = "groups/i/shift all range over the compile-time-constant 0..8, so these additions and the 7x multiply never overflow"
)]
pub(crate) fn write_varint_into(value: u64, out: &mut Vec<u8>) {
    if value < (1u64 << 56) {
        let mut groups = 1u32;
        while groups < 8 && value >= (1u64 << (7 * groups)) {
            groups += 1;
        }
        for i in 0..groups {
            let shift = 7 * (groups - 1 - i);
            #[allow(clippy::cast_possible_truncation)]
            let mut byte = ((value >> shift) & 0x7f) as u8;
            if i != groups - 1 {
                byte |= 0x80;
            }
            out.push(byte);
        }
    } else {
        let top56 = value >> 8;
        for i in 0..8 {
            let shift = 7 * (7 - i);
            #[allow(clippy::cast_possible_truncation)]
            let byte = (((top56 >> shift) & 0x7f) as u8) | 0x80;
            out.push(byte);
        }
        #[allow(clippy::cast_possible_truncation)]
        out.push((value & 0xff) as u8);
    }
}

/// The number of bytes [`write_varint_into`] would emit for `value`,
/// without emitting them -- sizes the record header without a
/// trial-encode loop.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "groups ranges over the compile-time-constant 0..8, so this addition never overflows"
)]
fn varint_len(value: u64) -> usize {
    if value < (1u64 << 56) {
        let mut groups = 1u32;
        while groups < 8 && value >= (1u64 << (7 * groups)) {
            groups += 1;
        }
        groups as usize
    } else {
        9
    }
}

// 24-bit and 48-bit signed integer ranges, per sqlite3VdbeSerialType's
// integer-width selection (smallest serial type that losslessly holds
// the value).
const I24_MIN: i64 = -(1 << 23);
const I24_MAX: i64 = (1 << 23) - 1;
const I48_MIN: i64 = -(1 << 47);
const I48_MAX: i64 = (1 << 47) - 1;

fn integer_serial_type(i: i64) -> u64 {
    if i == 0 {
        8
    } else if i == 1 {
        9
    } else if i8::try_from(i).is_ok() {
        1
    } else if i16::try_from(i).is_ok() {
        2
    } else if (I24_MIN..=I24_MAX).contains(&i) {
        3
    } else if i32::try_from(i).is_ok() {
        4
    } else if (I48_MIN..=I48_MAX).contains(&i) {
        5
    } else {
        6
    }
}

fn integer_body_len(serial_type: u64) -> usize {
    match serial_type {
        1 => 1,
        2 => 2,
        3 => 3,
        4 => 4,
        5 => 5,
        6 => 8,
        _ => 0, // 8/9: zero-byte constants
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn write_integer_body_into(i: i64, serial_type: u64, out: &mut Vec<u8>) {
    match serial_type {
        1 => out.push(i as u8),
        2 => out.extend_from_slice(&(i as i16).to_be_bytes()),
        3 => out.extend_from_slice(&i.to_be_bytes()[5..8]),
        4 => out.extend_from_slice(&(i as i32).to_be_bytes()),
        5 => out.extend_from_slice(&i.to_be_bytes()[2..8]),
        6 => out.extend_from_slice(&i.to_be_bytes()),
        _ => {} // 8/9: zero-byte constants
    }
}

/// Byte length of `s` once encoded under `encoding`, without allocating
/// the encoded bytes -- sizes a TEXT column's serial type.
fn encoded_text_len(s: &str, encoding: TextEncoding) -> usize {
    match encoding {
        TextEncoding::Utf8 => s.len(),
        TextEncoding::Utf16Le | TextEncoding::Utf16Be => s.encode_utf16().count().saturating_mul(2),
    }
}

fn write_text_body_into(s: &str, encoding: TextEncoding, out: &mut Vec<u8>) {
    match encoding {
        TextEncoding::Utf8 => out.extend_from_slice(s.as_bytes()),
        TextEncoding::Utf16Le => {
            out.extend(s.encode_utf16().flat_map(|u| u.to_le_bytes()));
        }
        TextEncoding::Utf16Be => {
            out.extend(s.encode_utf16().flat_map(|u| u.to_be_bytes()));
        }
    }
}

fn blob_serial_type(len: usize) -> u64 {
    12u64.saturating_add(2u64.saturating_mul(len as u64))
}

fn text_serial_type(len: usize) -> u64 {
    13u64.saturating_add(2u64.saturating_mul(len as u64))
}

/// A value's serial type and encoded body length, per the record-format
/// doc: the smallest integer width that losslessly holds an INTEGER,
/// the 8-byte IEEE-754 form for REAL, and the `12+2*len`/`13+2*len`
/// scheme for BLOB/TEXT.
fn serial_type_and_body_len(value: &Value, encoding: TextEncoding) -> (u64, usize) {
    match value {
        Value::Null => (0, 0),
        Value::Integer(i) => {
            let st = integer_serial_type(*i);
            (st, integer_body_len(st))
        }
        Value::Real(_) => (7, 8),
        Value::Blob(b) => (blob_serial_type(b.len()), b.len()),
        Value::Text(s) => {
            let len = encoded_text_len(s, encoding);
            (text_serial_type(len), len)
        }
    }
}

fn write_body_into(value: &Value, serial_type: u64, encoding: TextEncoding, out: &mut Vec<u8>) {
    match value {
        Value::Null => {}
        Value::Integer(i) => write_integer_body_into(*i, serial_type, out),
        Value::Real(r) => out.extend_from_slice(&r.to_be_bytes()),
        Value::Blob(b) => out.extend_from_slice(b),
        Value::Text(s) => write_text_body_into(s, encoding, out),
    }
}

/// Encodes column values into a record payload: a varint header length,
/// one varint serial type per column, then the column bodies
/// back-to-back. Backs `Opcode::MakeRecord`.
pub fn encode_record(values: &[Value], encoding: TextEncoding) -> Vec<u8> {
    let mut out = Vec::new();
    let mut serial_types: Vec<(u64, usize)> = values
        .iter()
        .map(|v| serial_type_and_body_len(v, encoding))
        .collect();

    let mut header_body_len = 0usize;
    let mut bodies_len = 0usize;
    for (st, len) in &serial_types {
        header_body_len = header_body_len.saturating_add(varint_len(*st));
        bodies_len = bodies_len.saturating_add(*len);
    }

    // header_len includes its own varint's length; grow until the
    // varint's own encoded size is consistent with the declared length.
    let mut header_len = header_body_len.saturating_add(1);
    #[allow(clippy::cast_possible_truncation)]
    while varint_len(header_len as u64).saturating_add(header_body_len) != header_len {
        header_len = header_len.saturating_add(1);
    }

    out.reserve(header_len.saturating_add(bodies_len));
    #[allow(clippy::cast_possible_truncation)]
    write_varint_into(header_len as u64, &mut out);
    for (st, _) in &serial_types {
        write_varint_into(*st, &mut out);
    }
    for (value, (st, _)) in values.iter().zip(serial_types.drain(..)) {
        write_body_into(value, st, encoding, &mut out);
    }
    out
}

/// Decodes a SQLite varint: big-endian, 7 bits per byte with a high-bit
/// continuation flag, up to 9 bytes. Returns the decoded value and the
/// number of bytes consumed.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "i ranges over the compile-time-constant 0..8, so i + 1 never overflows"
)]
fn decode_varint(buf: &[u8]) -> Result<(u64, usize), RecordError> {
    let mut result: u64 = 0;
    for i in 0..8 {
        let byte = *buf.get(i).ok_or(RecordError::UnexpectedEof { offset: i })?;
        result = (result << 7) | u64::from(byte & 0x7f);
        if byte & 0x80 == 0 {
            return Ok((result, i + 1));
        }
    }
    let byte = *buf.get(8).ok_or(RecordError::UnexpectedEof { offset: 8 })?;
    result = (result << 8) | u64::from(byte);
    Ok((result, 9))
}

/// `decode_varint`, but against `buf` starting at absolute offset `pos`,
/// with errors reporting the absolute offset.
fn decode_varint_at(buf: &[u8], pos: usize) -> Result<(u64, usize), RecordError> {
    let slice = buf
        .get(pos..)
        .ok_or(RecordError::UnexpectedEof { offset: pos })?;
    decode_varint(slice).map_err(|e| match e {
        RecordError::UnexpectedEof { offset } => RecordError::UnexpectedEof {
            offset: pos.saturating_add(offset),
        },
        other => other,
    })
}

fn take(buf: &[u8], pos: usize, len: usize) -> Result<&[u8], RecordError> {
    let end = pos
        .checked_add(len)
        .ok_or(RecordError::UnexpectedEof { offset: pos })?;
    buf.get(pos..end)
        .ok_or(RecordError::UnexpectedEof { offset: pos })
}

fn take_array<const N: usize>(buf: &[u8], pos: usize) -> Result<[u8; N], RecordError> {
    take(buf, pos, N)?
        .try_into()
        .map_err(|_| RecordError::UnexpectedEof { offset: pos })
}

/// Number of body bytes a serial type occupies, without decoding the
/// value it holds.
fn serial_type_len(serial_type: u64) -> usize {
    match serial_type {
        0 | 8 | 9 | 10 | 11 => 0,
        1 => 1,
        2 => 2,
        3 => 3,
        4 => 4,
        5 => 6,
        6 | 7 => 8,
        n if n % 2 == 0 => (n.wrapping_sub(12) / 2) as usize,
        n => (n.wrapping_sub(13) / 2) as usize,
    }
}

/// Decodes one column body given its serial type. Returns the value and
/// the number of body bytes it occupies.
fn decode_serial_value(
    serial_type: u64,
    buf: &[u8],
    pos: usize,
    encoding: TextEncoding,
) -> Result<(Value, usize), RecordError> {
    match serial_type {
        0 => Ok((Value::Null, 0)),
        1 => {
            let [b0] = take_array(buf, pos)?;
            Ok((Value::Integer(i64::from(b0 as i8)), 1))
        }
        2 => {
            let b = take_array(buf, pos)?;
            Ok((Value::Integer(i64::from(i16::from_be_bytes(b))), 2))
        }
        3 => {
            let [b0, b1, b2] = take_array(buf, pos)?;
            let mut v = (i64::from(b0) << 16) | (i64::from(b1) << 8) | i64::from(b2);
            if b0 & 0x80 != 0 {
                v = v.wrapping_sub(1 << 24); // sign-extend 24-bit
            }
            Ok((Value::Integer(v), 3))
        }
        4 => {
            let b = take_array(buf, pos)?;
            Ok((Value::Integer(i64::from(i32::from_be_bytes(b))), 4))
        }
        5 => {
            let bytes: [u8; 6] = take_array(buf, pos)?;
            let [b0, ..] = bytes;
            let mut v: i64 = 0;
            for byte in bytes {
                v = (v << 8) | i64::from(byte);
            }
            if b0 & 0x80 != 0 {
                v = v.wrapping_sub(1 << 48); // sign-extend 48-bit
            }
            Ok((Value::Integer(v), 6))
        }
        6 => {
            let b = take_array(buf, pos)?;
            Ok((Value::Integer(i64::from_be_bytes(b)), 8))
        }
        7 => {
            let b = take_array(buf, pos)?;
            let value = f64::from_be_bytes(b);
            // A NaN float payload decodes as NULL, matching SQLite's
            // sqlite3VdbeSerialGet.
            if value.is_nan() {
                Ok((Value::Null, 8))
            } else {
                Ok((Value::Real(value), 8))
            }
        }
        8 => Ok((Value::Integer(0), 0)),
        9 => Ok((Value::Integer(1), 0)),
        // Types 10/11 are reserved/internal and never appear in a
        // well-formed database, but decode as NULL rather than erroring.
        10 | 11 => Ok((Value::Null, 0)),
        n if n % 2 == 0 => {
            let len = (n.wrapping_sub(12) / 2) as usize;
            let bytes = take(buf, pos, len)?;
            Ok((Value::Blob(bytes.into()), len))
        }
        n => {
            let len = (n.wrapping_sub(13) / 2) as usize;
            let bytes = take(buf, pos, len)?;
            let text = decode_text(bytes, encoding)?;
            Ok((Value::Text(text), len))
        }
    }
}

fn decode_text(bytes: &[u8], encoding: TextEncoding) -> Result<Rc<str>, RecordError> {
    match encoding {
        TextEncoding::Utf8 => std::str::from_utf8(bytes)
            .map(Rc::from)
            .map_err(|_| RecordError::InvalidUtf8),
        TextEncoding::Utf16Le => decode_utf16(bytes, u16::from_le_bytes).map(Rc::from),
        TextEncoding::Utf16Be => decode_utf16(bytes, u16::from_be_bytes).map(Rc::from),
    }
}

fn decode_utf16(bytes: &[u8], unit_from_bytes: fn([u8; 2]) -> u16) -> Result<String, RecordError> {
    if !bytes.len().is_multiple_of(2) {
        return Err(RecordError::InvalidUtf16);
    }
    let units = bytes.chunks_exact(2).map(|c| unit_from_bytes([c[0], c[1]]));
    char::decode_utf16(units)
        .collect::<Result<String, _>>()
        .map_err(|_| RecordError::InvalidUtf16)
}

/// Walks a record payload's header once, returning each column's serial
/// type paired with the byte offset (into `payload`) of that column's
/// body -- never decodes any column body.
fn parse_header(payload: &[u8]) -> Result<Vec<(u64, usize)>, RecordError> {
    let (header_len, n) = decode_varint_at(payload, 0)?;
    let header_len = header_len as usize;
    if header_len < n {
        return Err(RecordError::HeaderTooShort {
            declared: header_len,
            varint_len: n,
        });
    }

    let mut entries = Vec::new();
    let mut pos = n;
    let mut body_pos = header_len;
    while pos < header_len {
        let (serial_type, len) = decode_varint_at(payload, pos)?;
        if pos.saturating_add(len) > header_len {
            return Err(RecordError::HeaderOverrun {
                offset: pos,
                header_len,
            });
        }
        pos = pos.saturating_add(len);
        entries.push((serial_type, body_pos));
        body_pos = body_pos.saturating_add(serial_type_len(serial_type));
    }
    Ok(entries)
}

/// Decodes a record (the payload of a table cell) into column values,
/// per the record-format doc: varint header length, then one varint
/// serial type per column, then the column bodies back-to-back. Never
/// panics -- any truncation or malformed serial type returns `Err`.
pub fn decode_record(payload: &[u8], encoding: TextEncoding) -> Result<Vec<Value>, RecordError> {
    let (header_len, _) = decode_varint_at(payload, 0)?;
    let mut body_pos = header_len as usize;
    let entries = parse_header(payload)?;
    let mut values = Vec::with_capacity(entries.len());
    for (serial_type, offset) in &entries {
        let (value, len) = decode_serial_value(*serial_type, payload, *offset, encoding)?;
        values.push(value);
        body_pos = offset.saturating_add(len);
    }
    if body_pos != payload.len() {
        return Err(RecordError::TrailingData {
            trailing: payload.len().saturating_sub(body_pos),
        });
    }
    Ok(values)
}

/// Decodes only column `idx` of a record's payload -- the header
/// entries are walked to compute body sizes/offsets, but only `idx`'s
/// body is decoded, unlike [`decode_record`]. Returns `Value::Null` for
/// an out-of-range `idx`.
pub fn decode_column(
    payload: &[u8],
    idx: usize,
    encoding: TextEncoding,
) -> Result<Value, RecordError> {
    let entries = parse_header(payload)?;
    match entries.get(idx) {
        Some(&(serial_type, offset)) => {
            let (value, _) = decode_serial_value(serial_type, payload, offset, encoding)?;
            Ok(value)
        }
        None => Ok(Value::Null),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_varint(value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        write_varint_into(value, &mut out);
        out
    }

    #[test]
    fn integer_widths_pick_smallest_serial_type() {
        let cases: &[(i64, u64)] = &[
            (0, 8),
            (1, 9),
            (2, 1),
            (i64::from(i8::MIN), 1),
            (i64::from(i8::MAX) + 1, 2),
            (i64::from(i16::MIN), 2),
            (i64::from(i16::MAX) + 1, 3),
            (I24_MIN, 3),
            (I24_MAX + 1, 4),
            (i64::from(i32::MIN), 4),
            (i64::from(i32::MAX) + 1, 5),
            (I48_MIN, 5),
            (I48_MAX + 1, 6),
            (i64::MAX, 6),
            (i64::MIN, 6),
        ];
        for (v, expected_st) in cases {
            let (st, _) = serial_type_and_body_len(&Value::Integer(*v), TextEncoding::Utf8);
            assert_eq!(
                st, *expected_st,
                "value {v} expected serial type {expected_st}"
            );
        }
    }

    #[test]
    fn matches_spec_003_header_shape_for_a_multi_column_row() {
        let values = vec![Value::Integer(42), Value::Text("abc".to_string().into())];
        let payload = encode_record(&values, TextEncoding::Utf8);
        // header_len(1) + serial_type(42 -> type 1, 1 byte) + serial_type(abc -> 13+2*3=19, 1 byte) = 3
        assert_eq!(payload[0], 3);
        assert_eq!(payload[1], 1); // type 1: i8
        assert_eq!(payload[2], 19); // type 13+2*3
        assert_eq!(payload[3], 42);
        assert_eq!(&payload[4..7], b"abc");
    }

    #[test]
    fn null_column_has_zero_serial_type_and_no_body() {
        let payload = encode_record(&[Value::Null], TextEncoding::Utf8);
        assert_eq!(payload, vec![2, 0]); // header_len=2, serial_type=0, no body
    }

    #[test]
    fn empty_record_is_just_the_header_length() {
        let payload = encode_record(&[], TextEncoding::Utf8);
        assert_eq!(payload, vec![1]);
    }

    #[test]
    fn real_encodes_as_eight_byte_ieee754() {
        let payload = encode_record(&[Value::Real(1.5)], TextEncoding::Utf8);
        assert_eq!(payload[0], 2);
        assert_eq!(payload[1], 7);
        assert_eq!(&payload[2..10], &1.5f64.to_be_bytes());
    }

    #[test]
    fn blob_serial_type_is_twelve_plus_twice_the_length() {
        let payload = encode_record(&[Value::Blob(vec![0xde, 0xad].into())], TextEncoding::Utf8);
        assert_eq!(payload[1], 12 + 2 * 2);
        assert_eq!(&payload[2..4], &[0xde, 0xad]);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__encode_33__v1_groups_grows() {
        assert_eq!(encode_varint(128).len(), 2);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__encode_33__v2_groups_stays_one() {
        assert_eq!(encode_varint(5).len(), 1);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__encode_33__v3_groups_caps_at_eight() {
        assert_eq!(encode_varint((1u64 << 56) - 1).len(), 8);
    }

    #[test]
    fn nine_byte_varint_form_kicks_in_past_fifty_six_bits() {
        assert_eq!(encode_varint(u64::MAX).len(), 9);
    }

    fn record_bytes(serial_types_and_bodies: &[(u64, &[u8])]) -> Vec<u8> {
        let mut header = Vec::new();
        for (st, _) in serial_types_and_bodies {
            write_varint_into(*st, &mut header);
        }
        let mut header_len = header.len() + 1;
        loop {
            let hl_bytes = encode_varint(header_len as u64);
            if hl_bytes.len() + header.len() == header_len {
                let mut out = hl_bytes;
                out.extend(&header);
                for (_, body) in serial_types_and_bodies {
                    out.extend(*body);
                }
                return out;
            }
            header_len += 1;
        }
    }

    #[test]
    fn round_trips_through_decode_record() {
        let values = vec![
            Value::Null,
            Value::Integer(0),
            Value::Integer(1),
            Value::Integer(-1),
            Value::Integer(i64::MAX),
            Value::Integer(i64::MIN),
            Value::Real(1.5),
            Value::Text("hello".to_string().into()),
            Value::Text(String::new().into()),
            Value::Blob(vec![0xde, 0xad, 0xbe, 0xef].into()),
            Value::Blob(Vec::new().into()),
        ];
        let payload = encode_record(&values, TextEncoding::Utf8);
        assert_eq!(decode_record(&payload, TextEncoding::Utf8), Ok(values));
    }

    #[test]
    fn decode_real_nan_decodes_as_null() {
        let payload = record_bytes(&[(7, &f64::NAN.to_be_bytes())]);
        assert_eq!(
            decode_record(&payload, TextEncoding::Utf8),
            Ok(vec![Value::Null])
        );
    }

    #[test]
    fn decode_reserved_serial_types_decode_as_null() {
        let payload = record_bytes(&[(10, &[])]);
        assert_eq!(
            decode_record(&payload, TextEncoding::Utf8),
            Ok(vec![Value::Null])
        );
        let payload = record_bytes(&[(11, &[])]);
        assert_eq!(
            decode_record(&payload, TextEncoding::Utf8),
            Ok(vec![Value::Null])
        );
    }

    #[test]
    fn decode_text_utf16le_and_utf16be() {
        let s = "hé";
        let units: Vec<u16> = s.encode_utf16().collect();
        let le: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
        let be: Vec<u8> = units.iter().flat_map(|u| u.to_be_bytes()).collect();

        let payload_le = record_bytes(&[(13 + 2 * le.len() as u64, &le)]);
        assert_eq!(
            decode_record(&payload_le, TextEncoding::Utf16Le),
            Ok(vec![Value::Text(s.to_string().into())])
        );
        let payload_be = record_bytes(&[(13 + 2 * be.len() as u64, &be)]);
        assert_eq!(
            decode_record(&payload_be, TextEncoding::Utf16Be),
            Ok(vec![Value::Text(s.to_string().into())])
        );
    }

    #[test]
    fn decode_header_len_exactly_equal_to_its_own_varint_len_is_a_valid_empty_record() {
        let payload = vec![0x01];
        assert_eq!(decode_record(&payload, TextEncoding::Utf8), Ok(vec![]));
    }

    #[test]
    fn decode_header_shorter_than_its_own_varint_errors() {
        let payload = vec![0x80, 0x00];
        assert_eq!(
            decode_record(&payload, TextEncoding::Utf8),
            Err(RecordError::HeaderTooShort {
                declared: 0,
                varint_len: 2
            })
        );
    }

    #[test]
    fn decode_header_entry_overrunning_declared_length_errors() {
        let payload = vec![0x02, 0x81, 0x00];
        assert_eq!(
            decode_record(&payload, TextEncoding::Utf8),
            Err(RecordError::HeaderOverrun {
                offset: 1,
                header_len: 2
            })
        );
    }

    #[test]
    fn decode_trailing_bytes_after_last_column_error() {
        let mut payload = record_bytes(&[(0, &[])]);
        payload.push(0xff);
        assert_eq!(
            decode_record(&payload, TextEncoding::Utf8),
            Err(RecordError::TrailingData { trailing: 1 })
        );
    }

    #[test]
    fn decode_invalid_utf8_errors_not_panics() {
        let invalid = [0xff, 0xfe];
        let payload = record_bytes(&[(13 + 2 * invalid.len() as u64, &invalid)]);
        assert_eq!(
            decode_record(&payload, TextEncoding::Utf8),
            Err(RecordError::InvalidUtf8)
        );
    }

    #[test]
    fn decode_column_matches_decode_record_at_every_index() {
        let payload = record_bytes(&[
            (1, &[42]),
            (0, &[]),
            (13 + 2 * 5, b"hello"),
            (7, &2.5f64.to_be_bytes()),
        ]);
        let full = decode_record(&payload, TextEncoding::Utf8).unwrap();
        for (idx, expected) in full.iter().enumerate() {
            assert_eq!(
                decode_column(&payload, idx, TextEncoding::Utf8),
                Ok(expected.clone())
            );
        }
    }

    #[test]
    fn decode_column_out_of_range_is_null() {
        let payload = record_bytes(&[(1, &[42])]);
        assert_eq!(
            decode_column(&payload, 5, TextEncoding::Utf8),
            Ok(Value::Null)
        );
    }

    #[test]
    fn decode_truncated_record_at_every_offset_errors_not_panics() {
        let payload = record_bytes(&[
            (1, &[42]),
            (13 + 2 * 5, b"hello"),
            (7, &2.5f64.to_be_bytes()),
        ]);
        for cut in 0..payload.len() {
            let result = decode_record(&payload[..cut], TextEncoding::Utf8);
            assert!(
                result.is_err(),
                "truncating to {cut} bytes should error, got {result:?}"
            );
        }
        assert!(decode_record(&payload, TextEncoding::Utf8).is_ok());
    }
}
