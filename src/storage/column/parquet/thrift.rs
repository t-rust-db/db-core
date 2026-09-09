//! Hand-rolled Thrift Compact Protocol decoder, sufficient for reading
//! Parquet footer metadata. No external Thrift dependency.
//!
//! Spec: https://github.com/apache/thrift/blob/master/doc/specs/thrift-compact-protocol.md

use std::fmt;

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
const CTYPE_SET: u8 = 0x0a;
const CTYPE_MAP: u8 = 0x0b;
const CTYPE_STRUCT: u8 = 0x0c;

#[derive(Debug)]
pub enum ThriftError {
    UnexpectedEof,
    InvalidVarint,
    UnknownType(u8),
}

impl fmt::Display for ThriftError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ThriftError::UnexpectedEof => write!(f, "unexpected end of input"),
            ThriftError::InvalidVarint => write!(f, "varint too long"),
            ThriftError::UnknownType(t) => write!(f, "unknown thrift compact type: 0x{t:02x}"),
        }
    }
}

impl std::error::Error for ThriftError {}

pub type Result<T> = std::result::Result<T, ThriftError>;

/// A generic, schema-less Thrift value. Parquet's `FileMetaData` struct is
/// decoded into this shape and then interpreted field-by-field.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    Byte(i8),
    I16(i16),
    I32(i32),
    I64(i64),
    Double(f64),
    Binary(Vec<u8>),
    List(Vec<Value>),
    Map(Vec<(Value, Value)>),
    /// Fields keyed by their Thrift field id.
    Struct(Vec<(i16, Value)>),
}

impl Value {
    pub fn as_i32(&self) -> Option<i32> {
        match self {
            Value::I16(v) => Some(*v as i32),
            Value::I32(v) => Some(*v),
            Value::I64(v) => Some(*v as i32),
            Value::Byte(v) => Some(*v as i32),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::I16(v) => Some(*v as i64),
            Value::I32(v) => Some(*v as i64),
            Value::I64(v) => Some(*v),
            Value::Byte(v) => Some(*v as i64),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Binary(b) => std::str::from_utf8(b).ok(),
            _ => None,
        }
    }

    pub fn as_binary(&self) -> Option<&[u8]> {
        match self {
            Value::Binary(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_struct(&self) -> Option<&[(i16, Value)]> {
        match self {
            Value::Struct(fields) => Some(fields),
            _ => None,
        }
    }

    /// Look up a field by id within a `Struct` value.
    pub fn field(&self, id: i16) -> Option<&Value> {
        self.as_struct()?
            .iter()
            .find(|(fid, _)| *fid == id)
            .map(|(_, v)| v)
    }
}

pub struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Decoder { buf, pos: 0 }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    fn read_byte(&mut self) -> Result<u8> {
        let b = *self.buf.get(self.pos).ok_or(ThriftError::UnexpectedEof)?;
        self.pos += 1;
        Ok(b)
    }

    fn read_bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or(ThriftError::UnexpectedEof)?;
        let slice = self
            .buf
            .get(self.pos..end)
            .ok_or(ThriftError::UnexpectedEof)?;
        self.pos = end;
        Ok(slice)
    }

    /// Unsigned LEB128 varint.
    fn read_varint(&mut self) -> Result<u64> {
        let mut result: u64 = 0;
        let mut shift = 0u32;
        loop {
            if shift >= 70 {
                return Err(ThriftError::InvalidVarint);
            }
            let b = self.read_byte()?;
            result |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        Ok(result)
    }

    fn read_zigzag_i32(&mut self) -> Result<i32> {
        let v = self.read_varint()? as u32;
        Ok(((v >> 1) as i32) ^ -((v & 1) as i32))
    }

    fn read_zigzag_i64(&mut self) -> Result<i64> {
        let v = self.read_varint()?;
        Ok(((v >> 1) as i64) ^ -((v & 1) as i64))
    }

    fn read_double(&mut self) -> Result<f64> {
        let bytes = self.read_bytes(8)?;
        let mut arr = [0u8; 8];
        arr.copy_from_slice(bytes);
        Ok(f64::from_le_bytes(arr))
    }

    fn read_binary(&mut self) -> Result<Vec<u8>> {
        let len = self.read_varint()? as usize;
        Ok(self.read_bytes(len)?.to_vec())
    }

    /// Read one value of the given compact wire type. In a struct field, the
    /// boolean's value is folded into the field header itself (TRUE/FALSE
    /// variants), supplied here via `bool_value`; inside a list/set/map the
    /// element type is always the generic "bool" id and the value follows as
    /// its own byte (1 = true, 2 = false), so `bool_value` is `None` there.
    fn read_value(&mut self, ctype: u8, bool_value: Option<bool>) -> Result<Value> {
        match ctype {
            CTYPE_BOOLEAN_TRUE => match bool_value {
                Some(v) => Ok(Value::Bool(v)),
                None => Ok(Value::Bool(self.read_byte()? == CTYPE_BOOLEAN_TRUE)),
            },
            CTYPE_BOOLEAN_FALSE => Ok(Value::Bool(bool_value.unwrap_or(false))),
            CTYPE_BYTE => Ok(Value::Byte(self.read_byte()? as i8)),
            CTYPE_I16 => Ok(Value::I16(self.read_zigzag_i32()? as i16)),
            CTYPE_I32 => Ok(Value::I32(self.read_zigzag_i32()?)),
            CTYPE_I64 => Ok(Value::I64(self.read_zigzag_i64()?)),
            CTYPE_DOUBLE => Ok(Value::Double(self.read_double()?)),
            CTYPE_BINARY => Ok(Value::Binary(self.read_binary()?)),
            CTYPE_LIST | CTYPE_SET => self.read_list(),
            CTYPE_MAP => self.read_map(),
            CTYPE_STRUCT => self.read_struct().map(Value::Struct),
            other => Err(ThriftError::UnknownType(other)),
        }
    }

    fn read_list(&mut self) -> Result<Value> {
        let header = self.read_byte()?;
        let mut size = (header >> 4) as u64;
        let elem_type = decode_element_type(header & 0x0f)?;
        if size == 15 {
            size = self.read_varint()?;
        }
        let mut items = Vec::with_capacity(size as usize);
        for _ in 0..size {
            items.push(self.read_value(elem_type, None)?);
        }
        Ok(Value::List(items))
    }

    fn read_map(&mut self) -> Result<Value> {
        let size = self.read_varint()?;
        if size == 0 {
            return Ok(Value::Map(Vec::new()));
        }
        let types = self.read_byte()?;
        let key_type = decode_element_type(types >> 4)?;
        let val_type = decode_element_type(types & 0x0f)?;
        let mut items = Vec::with_capacity(size as usize);
        for _ in 0..size {
            let k = self.read_value(key_type, None)?;
            let v = self.read_value(val_type, None)?;
            items.push((k, v));
        }
        Ok(Value::Map(items))
    }

    /// Read a struct: a sequence of field headers/values terminated by STOP.
    pub fn read_struct(&mut self) -> Result<Vec<(i16, Value)>> {
        let mut fields = Vec::new();
        let mut last_field_id: i16 = 0;
        loop {
            let header = self.read_byte()?;
            if header == CTYPE_STOP {
                break;
            }
            let delta = (header >> 4) & 0x0f;
            let ctype = header & 0x0f;
            let field_id = if delta == 0 {
                self.read_zigzag_i32()? as i16
            } else {
                last_field_id + delta as i16
            };
            last_field_id = field_id;

            let bool_value = match ctype {
                CTYPE_BOOLEAN_TRUE => Some(true),
                CTYPE_BOOLEAN_FALSE => Some(false),
                _ => None,
            };
            let value = self.read_value(ctype, bool_value)?;
            fields.push((field_id, value));
        }
        Ok(fields)
    }
}

/// List/set/map element types are encoded without the boolean split used in
/// struct fields (there's no true/false distinction — booleans in
/// collections use type id 1 for "bool").
fn decode_element_type(nibble: u8) -> Result<u8> {
    match nibble {
        0x01 => Ok(CTYPE_BOOLEAN_TRUE), // bool element; value bit follows as a real byte via read_bool path
        CTYPE_BYTE | CTYPE_I16 | CTYPE_I32 | CTYPE_I64 | CTYPE_DOUBLE | CTYPE_BINARY
        | CTYPE_LIST | CTYPE_SET | CTYPE_MAP | CTYPE_STRUCT => Ok(nibble),
        other => Err(ThriftError::UnknownType(other)),
    }
}

/// Decode a top-level Thrift Compact struct from a byte slice, returning the
/// fields and the number of bytes consumed.
pub fn decode_struct(buf: &[u8]) -> Result<(Vec<(i16, Value)>, usize)> {
    let mut decoder = Decoder::new(buf);
    let fields = decoder.read_struct()?;
    Ok((fields, decoder.position()))
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

    fn zigzag_i32(v: i32) -> u64 {
        ((v << 1) ^ (v >> 31)) as u32 as u64
    }

    fn write_varint(out: &mut Vec<u8>, mut v: u64) {
        loop {
            let mut b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                b |= 0x80;
            }
            out.push(b);
            if v == 0 {
                break;
            }
        }
    }

    /// Field header with small delta (1-15) and a scalar type.
    fn field_header(delta: u8, ctype: u8) -> u8 {
        (delta << 4) | ctype
    }

    #[test]
    fn decodes_simple_struct() {
        // struct { 1: i32 = 42, 2: binary = "hi" }
        let mut buf = Vec::new();
        buf.push(field_header(1, CTYPE_I32));
        write_varint(&mut buf, zigzag_i32(42));
        buf.push(field_header(1, CTYPE_BINARY));
        write_varint(&mut buf, 2);
        buf.extend_from_slice(b"hi");
        buf.push(CTYPE_STOP);

        let (fields, consumed) = decode_struct(&buf).unwrap();
        assert_eq!(consumed, buf.len());
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0], (1, Value::I32(42)));
        assert_eq!(fields[1], (2, Value::Binary(b"hi".to_vec())));
    }

    #[test]
    fn decodes_nested_struct() {
        // struct { 1: struct { 1: bool = true } }
        let inner = vec![field_header(1, CTYPE_BOOLEAN_TRUE), CTYPE_STOP];

        let mut buf = Vec::new();
        buf.push(field_header(1, CTYPE_STRUCT));
        buf.extend_from_slice(&inner);
        buf.push(CTYPE_STOP);

        let (fields, _) = decode_struct(&buf).unwrap();
        assert_eq!(fields.len(), 1);
        match &fields[0].1 {
            Value::Struct(inner_fields) => {
                assert_eq!(inner_fields, &vec![(1, Value::Bool(true))]);
            }
            other => panic!("expected nested struct, got {other:?}"),
        }
    }

    #[test]
    fn decodes_list_of_structs() {
        // struct { 1: list<struct> = [ {1: i32=1}, {1: i32=2} ] }
        let mut elem1 = Vec::new();
        elem1.push(field_header(1, CTYPE_I32));
        write_varint(&mut elem1, zigzag_i32(1));
        elem1.push(CTYPE_STOP);

        let mut elem2 = Vec::new();
        elem2.push(field_header(1, CTYPE_I32));
        write_varint(&mut elem2, zigzag_i32(2));
        elem2.push(CTYPE_STOP);

        let mut buf = Vec::new();
        buf.push(field_header(1, CTYPE_LIST));
        // list header: size=2 (fits in nibble), elem type STRUCT
        buf.push((2u8 << 4) | CTYPE_STRUCT);
        buf.extend_from_slice(&elem1);
        buf.extend_from_slice(&elem2);
        buf.push(CTYPE_STOP);

        let (fields, _) = decode_struct(&buf).unwrap();
        let list = fields[0].1.as_list().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].field(1), Some(&Value::I32(1)));
        assert_eq!(list[1].field(1), Some(&Value::I32(2)));
    }

    #[test]
    fn decodes_large_list_with_extended_size() {
        // list with 16 elements forces the size==15 escape + varint size
        let mut buf = Vec::new();
        buf.push(field_header(1, CTYPE_LIST));
        buf.push((15u8 << 4) | CTYPE_I32);
        write_varint(&mut buf, 16);
        for i in 0..16 {
            write_varint(&mut buf, zigzag_i32(i));
        }
        buf.push(CTYPE_STOP);

        let (fields, _) = decode_struct(&buf).unwrap();
        let list = fields[0].1.as_list().unwrap();
        assert_eq!(list.len(), 16);
        assert_eq!(list[15], Value::I32(15));
    }

    #[test]
    fn decodes_map() {
        // struct { 1: map<binary, i32> = {"a": 1} }
        let mut buf = Vec::new();
        buf.push(field_header(1, CTYPE_MAP));
        write_varint(&mut buf, 1); // size
        buf.push((CTYPE_BINARY << 4) | CTYPE_I32);
        write_varint(&mut buf, 1);
        buf.push(b'a');
        write_varint(&mut buf, zigzag_i32(1));
        buf.push(CTYPE_STOP);

        let (fields, _) = decode_struct(&buf).unwrap();
        match &fields[0].1 {
            Value::Map(entries) => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].0.as_str(), Some("a"));
                assert_eq!(entries[0].1, Value::I32(1));
            }
            other => panic!("expected map, got {other:?}"),
        }
    }

    #[test]
    fn malformed_input_returns_error_not_panic() {
        // A field header claiming struct type but truncated before STOP.
        let buf = vec![field_header(1, CTYPE_STRUCT)];
        let result = decode_struct(&buf);
        assert!(result.is_err());

        // Unknown type nibble.
        let buf = vec![field_header(1, 0x0f)];
        let result = decode_struct(&buf);
        assert!(result.is_err());

        // Truncated varint (all continuation bits set, never terminates).
        let buf = vec![field_header(1, CTYPE_I32), 0x80];
        let result = decode_struct(&buf);
        assert!(result.is_err());

        // Empty input.
        let result = decode_struct(&[]);
        assert!(result.is_err());
    }
}
