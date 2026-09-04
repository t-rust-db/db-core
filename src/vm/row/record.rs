//! On-disk record encoding, ported from sqlite-rs's `record::encode`
//! (db-core#56, follow-up to #51): the varint header length, one varint
//! serial type per column, then column bodies back-to-back. Backs
//! `Opcode::MakeRecord`.
//!
//! **Encode-only.** Record *decoding* (`record::decode`, 833 lines,
//! sqlite-rs's largest record-module file) is deferred to the next
//! phase, along with ephemeral-cursor support (which needs decode for
//! `Insert`) -- see db-core#56's own scope note.

use super::value::{TextEncoding, Value};

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
}
