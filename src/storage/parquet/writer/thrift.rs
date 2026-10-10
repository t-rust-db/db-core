//! Thrift Compact Protocol encoder, the inverse of
//! [`crate::storage::parquet::thrift`]'s decoder over the same generic
//! [`Value`] tree, so every encoding is checked by decoding it back.
//!
//! Spec: https://github.com/apache/thrift/blob/master/doc/specs/thrift-compact-protocol.md

use std::fmt;

use crate::storage::parquet::thrift::Value;

const CTYPE_STOP: u8 = 0x00;
const CTYPE_BOOLEAN_TRUE: u8 = 0x01;
const CTYPE_BOOLEAN_FALSE: u8 = 0x02;
const CTYPE_BYTE: u8 = 0x03;
const CTYPE_I16: u8 = 0x04;
const CTYPE_I32: u8 = 0x05;
const CTYPE_I64: u8 = 0x06;
const CTYPE_DOUBLE: u8 = 0x07;
const CTYPE_BINARY: u8 = 0x08;
const CTYPE_LIST: u8 = 0x09;
const CTYPE_MAP: u8 = 0x0b;
const CTYPE_STRUCT: u8 = 0x0c;

/// A [`Value`] tree the compact protocol cannot represent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    /// A list or map whose elements are not all of one wire type.
    MixedCollection,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::MixedCollection => {
                write!(f, "list or map elements of more than one thrift type")
            }
        }
    }
}

impl std::error::Error for EncodeError {}

/// Encode a top-level struct (the shape `decode_struct` reads back).
pub(crate) fn encode_struct(fields: &[(i16, Value)]) -> Result<Vec<u8>, EncodeError> {
    let mut out = Vec::new();
    write_struct(&mut out, fields)?;
    Ok(out)
}

/// The wire type of a value as a collection element. Booleans in a
/// collection use the generic bool id; in a struct field they fold their
/// value into the field header instead (see [`write_struct`]).
fn element_type(value: &Value) -> u8 {
    match value {
        Value::Bool(_) => CTYPE_BOOLEAN_TRUE,
        Value::Byte(_) => CTYPE_BYTE,
        Value::I16(_) => CTYPE_I16,
        Value::I32(_) => CTYPE_I32,
        Value::I64(_) => CTYPE_I64,
        Value::Double(_) => CTYPE_DOUBLE,
        Value::Binary(_) => CTYPE_BINARY,
        Value::List(_) => CTYPE_LIST,
        Value::Map(_) => CTYPE_MAP,
        Value::Struct(_) => CTYPE_STRUCT,
    }
}

/// Fields are written in the order given. A field whose id is 1..=15 above
/// the previous one takes the short header; any other id (first field above
/// 15, a decreasing id, a negative id) takes the long form, so any order
/// decodes correctly.
fn write_struct(out: &mut Vec<u8>, fields: &[(i16, Value)]) -> Result<(), EncodeError> {
    let mut last: i16 = 0;
    for (id, value) in fields {
        let ctype = match value {
            Value::Bool(true) => CTYPE_BOOLEAN_TRUE,
            Value::Bool(false) => CTYPE_BOOLEAN_FALSE,
            other => element_type(other),
        };
        match short_delta(last, *id) {
            Some(delta) => out.push(delta.wrapping_shl(4) | ctype),
            None => {
                out.push(ctype);
                write_varint(out, zigzag_i32(i32::from(*id)));
            }
        }
        last = *id;
        if !matches!(value, Value::Bool(_)) {
            write_value(out, value)?;
        }
    }
    out.push(CTYPE_STOP);
    Ok(())
}

/// The header delta when `id` follows `last` by 1..=15.
fn short_delta(last: i16, id: i16) -> Option<u8> {
    let delta = id.checked_sub(last)?;
    if (1..=15).contains(&delta) {
        u8::try_from(delta).ok()
    } else {
        None
    }
}

fn write_value(out: &mut Vec<u8>, value: &Value) -> Result<(), EncodeError> {
    match value {
        Value::Bool(v) => out.push(if *v {
            CTYPE_BOOLEAN_TRUE
        } else {
            CTYPE_BOOLEAN_FALSE
        }),
        Value::Byte(v) => out.extend_from_slice(&v.to_le_bytes()),
        Value::I16(v) => write_varint(out, zigzag_i32(i32::from(*v))),
        Value::I32(v) => write_varint(out, zigzag_i32(*v)),
        Value::I64(v) => write_varint(out, zigzag_i64(*v)),
        Value::Double(v) => out.extend_from_slice(&v.to_le_bytes()),
        Value::Binary(bytes) => {
            write_len(out, bytes.len());
            out.extend_from_slice(bytes);
        }
        Value::List(items) => write_list(out, items)?,
        Value::Map(entries) => write_map(out, entries)?,
        Value::Struct(fields) => write_struct(out, fields)?,
    }
    Ok(())
}

/// One wire type for all of `values`; an empty collection has none, and
/// any type id decodes to the same empty value.
fn common_type(mut types: impl Iterator<Item = u8>) -> Result<u8, EncodeError> {
    let Some(ctype) = types.next() else {
        return Ok(CTYPE_STRUCT);
    };
    if types.all(|t| t == ctype) {
        Ok(ctype)
    } else {
        Err(EncodeError::MixedCollection)
    }
}

fn write_list(out: &mut Vec<u8>, items: &[Value]) -> Result<(), EncodeError> {
    let ctype = common_type(items.iter().map(element_type))?;
    match u8::try_from(items.len()) {
        Ok(n) if n < 15 => out.push(n.wrapping_shl(4) | ctype),
        _ => {
            out.push(0xf0 | ctype);
            write_len(out, items.len());
        }
    }
    for item in items {
        write_value(out, item)?;
    }
    Ok(())
}

fn write_map(out: &mut Vec<u8>, entries: &[(Value, Value)]) -> Result<(), EncodeError> {
    write_len(out, entries.len());
    if entries.is_empty() {
        return Ok(());
    }
    let key_type = common_type(entries.iter().map(|(k, _)| element_type(k)))?;
    let value_type = common_type(entries.iter().map(|(_, v)| element_type(v)))?;
    out.push(key_type.wrapping_shl(4) | value_type);
    for (k, v) in entries {
        write_value(out, k)?;
        write_value(out, v)?;
    }
    Ok(())
}

fn write_len(out: &mut Vec<u8>, len: usize) {
    write_varint(out, u64::try_from(len).unwrap_or(u64::MAX));
}

/// Unsigned LEB128.
pub(crate) fn write_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let low = u8::try_from(v & 0x7f).unwrap_or(0);
        v = v.wrapping_shr(7);
        if v == 0 {
            out.push(low);
            return;
        }
        out.push(low | 0x80);
    }
}

fn zigzag_i32(v: i32) -> u64 {
    let z = v.wrapping_shl(1) ^ v.wrapping_shr(31);
    u64::from(u32::from_le_bytes(z.to_le_bytes()))
}

fn zigzag_i64(v: i64) -> u64 {
    let z = v.wrapping_shl(1) ^ v.wrapping_shr(63);
    u64::from_le_bytes(z.to_le_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::parquet::thrift::decode_struct;
    use crate::storage::parquet::writer::tests::Rng;

    fn round_trip(fields: Vec<(i16, Value)>) {
        let bytes = encode_struct(&fields).unwrap();
        let (decoded, consumed) = decode_struct(&bytes).unwrap();
        assert_eq!(decoded, fields);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn every_scalar_round_trips() {
        round_trip(vec![
            (1, Value::Bool(true)),
            (2, Value::Bool(false)),
            (3, Value::Byte(-7)),
            (4, Value::I16(i16::MIN)),
            (5, Value::I32(i32::MIN)),
            (6, Value::I64(i64::MAX)),
            (7, Value::Double(-1.5)),
            (8, Value::Binary(b"PAR1".to_vec())),
        ]);
    }

    #[test]
    fn long_form_field_ids_round_trip() {
        // First id above 15, a decreasing id, a negative id, and i16 extremes.
        round_trip(vec![
            (16, Value::I32(1)),
            (3, Value::I32(2)),
            (-4, Value::Bool(true)),
            (i16::MAX, Value::I32(3)),
            (i16::MIN, Value::I32(4)),
        ]);
    }

    #[test]
    fn collections_round_trip() {
        let long: Vec<Value> = (0..40).map(Value::I64).collect();
        round_trip(vec![
            (1, Value::List(vec![Value::Bool(true), Value::Bool(false)])),
            (2, Value::List(long)),
            (3, Value::List(Vec::new())),
            (4, Value::Map(Vec::new())),
            (
                5,
                Value::Map(vec![(Value::Binary(b"k".to_vec()), Value::I32(1))]),
            ),
            (
                6,
                Value::Struct(vec![(1, Value::Struct(vec![(2, Value::I32(9))]))]),
            ),
        ]);
    }

    #[test]
    fn mixed_list_is_refused() {
        let err = encode_struct(&[(1, Value::List(vec![Value::I32(1), Value::I64(2)]))]);
        assert_eq!(err, Err(EncodeError::MixedCollection));
        assert_eq!(
            EncodeError::MixedCollection.to_string(),
            "list or map elements of more than one thrift type"
        );
    }

    #[test]
    fn mixed_map_keys_or_values_are_refused() {
        let keys = Value::Map(vec![
            (Value::I32(1), Value::I32(1)),
            (Value::I64(2), Value::I32(2)),
        ]);
        assert!(encode_struct(&[(1, keys)]).is_err());
        let values = Value::Map(vec![
            (Value::I32(1), Value::I32(1)),
            (Value::I32(2), Value::I64(2)),
        ]);
        assert!(encode_struct(&[(1, values)]).is_err());
    }

    /// A random value at most `depth` collections deep. Lists are
    /// homogeneous (one element repeated, like the reader requires); doubles
    /// are never NaN, which does not equal itself (its bits are covered by
    /// the I64 case).
    fn random_value(rng: &mut Rng, depth: u32) -> Value {
        let kinds = if depth == 0 { 7 } else { 9 };
        match rng.below(kinds) {
            0 => Value::Bool(rng.next() & 1 == 1),
            1 => Value::Byte(i8::from_le_bytes([rng.next() as u8])),
            2 => Value::I16(rng.next() as i16),
            3 => Value::I32(rng.next() as i32),
            4 => Value::I64(rng.next() as i64),
            5 => {
                let d = f64::from_bits(rng.next());
                Value::Double(if d.is_nan() { 0.5 } else { d })
            }
            6 => Value::Binary((0..rng.below(20)).map(|_| rng.next() as u8).collect()),
            7 => {
                let item = random_value(rng, depth - 1);
                Value::List(std::iter::repeat_n(item, rng.below(20) as usize).collect())
            }
            _ => Value::Struct(random_fields(rng, depth - 1)),
        }
    }

    fn random_fields(rng: &mut Rng, depth: u32) -> Vec<(i16, Value)> {
        (0..rng.below(6))
            .map(|_| {
                // Mostly small ascending-ish ids (short headers), some anything.
                let id = if rng.below(4) == 0 {
                    rng.next() as i16
                } else {
                    rng.below(20) as i16
                };
                (id, random_value(rng, depth))
            })
            .collect()
    }

    #[test]
    fn random_structs_round_trip() {
        let mut rng = Rng::new(0x7468_7269_6674);
        for _ in 0..3000 {
            round_trip(random_fields(&mut rng, 3));
        }
    }
}
