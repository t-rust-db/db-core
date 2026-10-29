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

/// Deepest nesting of structs, lists and maps the decoder follows. Parquet's
/// own metadata nests about six levels; anything far deeper is hostile
/// input that would otherwise recurse until the stack overflows (#657).
pub const MAX_DEPTH: usize = 64;

/// Why a Thrift Compact Protocol buffer could not be decoded.
#[derive(Debug)]
pub enum ThriftError {
    /// The input ended in the middle of a value.
    UnexpectedEof,
    /// A varint ran past the 10 bytes a `u64` can need.
    InvalidVarint,
    /// A length, size or narrow-integer varint does not fit its type.
    InvalidLength(u64),
    /// A type nibble that is not a compact-protocol type id.
    UnknownType(u8),
    /// Structs, lists or maps nested deeper than [`MAX_DEPTH`].
    DepthExceeded,
}

impl fmt::Display for ThriftError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ThriftError::UnexpectedEof => write!(f, "unexpected end of input"),
            ThriftError::InvalidVarint => write!(f, "varint too long"),
            ThriftError::InvalidLength(n) => write!(f, "length or size out of range: {n}"),
            ThriftError::UnknownType(t) => write!(f, "unknown thrift compact type: 0x{t:02x}"),
            ThriftError::DepthExceeded => {
                write!(f, "thrift values nested deeper than {MAX_DEPTH} levels")
            }
        }
    }
}

impl std::error::Error for ThriftError {}

/// Result of a Thrift decode.
pub type Result<T> = std::result::Result<T, ThriftError>;

/// An `i16` field (field ids, I16 values) read as zigzag i32: out of range is
/// corrupt input, not a silent truncation.
fn narrow_i16(v: i32) -> Result<i16> {
    i16::try_from(v).map_err(|_| ThriftError::InvalidLength(u64::from(v.unsigned_abs())))
}

/// A generic, schema-less Thrift value. Parquet's `FileMetaData` struct is
/// decoded into this shape and then interpreted field-by-field.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A `bool`.
    Bool(bool),
    /// An `i8` (Thrift `byte`).
    Byte(i8),
    /// An `i16`.
    I16(i16),
    /// An `i32`.
    I32(i32),
    /// An `i64`.
    I64(i64),
    /// A `double`.
    Double(f64),
    /// A `binary` or `string`, as raw bytes.
    Binary(Vec<u8>),
    /// A `list` or `set`, in wire order.
    List(Vec<Value>),
    /// A `map`, as key/value pairs in wire order.
    Map(Vec<(Value, Value)>),
    /// Fields keyed by their Thrift field id.
    Struct(Vec<(i16, Value)>),
}

impl Value {
    /// Any integer value that fits an `i32`.
    pub fn as_i32(&self) -> Option<i32> {
        match self {
            Value::I16(v) => Some(*v as i32),
            Value::I32(v) => Some(*v),
            Value::I64(v) => i32::try_from(*v).ok(),
            Value::Byte(v) => Some(*v as i32),
            _ => None,
        }
    }

    /// Any integer value, widened to `i64`.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::I16(v) => Some(*v as i64),
            Value::I32(v) => Some(*v as i64),
            Value::I64(v) => Some(*v),
            Value::Byte(v) => Some(*v as i64),
            _ => None,
        }
    }

    /// A binary value that is valid UTF-8.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Binary(b) => std::str::from_utf8(b).ok(),
            _ => None,
        }
    }

    /// A binary value's bytes.
    pub fn as_binary(&self) -> Option<&[u8]> {
        match self {
            Value::Binary(b) => Some(b),
            _ => None,
        }
    }

    /// A `bool` value.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// A list (or set) value's elements.
    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }

    /// A struct value's `(field id, value)` pairs.
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

/// A cursor over a Thrift Compact Protocol buffer.
pub struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
    /// Structs, lists and maps currently open, capped at [`MAX_DEPTH`].
    depth: usize,
}

impl<'a> Decoder<'a> {
    /// A decoder positioned at the start of `buf`.
    pub fn new(buf: &'a [u8]) -> Self {
        Decoder {
            buf,
            pos: 0,
            depth: 0,
        }
    }

    /// Bytes consumed so far.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Bytes not yet consumed.
    fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    fn read_byte(&mut self) -> Result<u8> {
        let b = *self.buf.get(self.pos).ok_or(ThriftError::UnexpectedEof)?;
        self.pos = self.pos.checked_add(1).ok_or(ThriftError::UnexpectedEof)?;
        Ok(b)
    }

    /// Run `f` one nesting level (struct, list or map) deeper, or fail
    /// past [`MAX_DEPTH`].
    fn nested<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        if self.depth >= MAX_DEPTH {
            return Err(ThriftError::DepthExceeded);
        }
        self.depth = self
            .depth
            .checked_add(1)
            .ok_or(ThriftError::DepthExceeded)?;
        let out = f(self);
        self.depth = self.depth.saturating_sub(1);
        out
    }

    /// A collection size read from the input, checked against the bytes
    /// left: every element takes at least `min_elem_bytes`, so a larger
    /// size is corrupt and is rejected before anything is allocated.
    fn checked_size(&self, size: u64, min_elem_bytes: usize) -> Result<usize> {
        let n = usize::try_from(size).map_err(|_| ThriftError::InvalidLength(size))?;
        let max = self.remaining().checked_div(min_elem_bytes).unwrap_or(0);
        if n > max {
            return Err(ThriftError::InvalidLength(size));
        }
        Ok(n)
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

    /// Unsigned LEB128 varint, at most 10 bytes; bits past 64 are dropped.
    fn read_varint(&mut self) -> Result<u64> {
        let mut result: u64 = 0;
        for shift in (0..64u32).step_by(7) {
            let b = self.read_byte()?;
            result |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(result);
            }
        }
        Err(ThriftError::InvalidVarint)
    }

    fn read_zigzag_i32(&mut self) -> Result<i32> {
        let raw = self.read_varint()?;
        let v = u32::try_from(raw).map_err(|_| ThriftError::InvalidLength(raw))?;
        // Zigzag: the magnitude half always fits; the sign is -1 or 0.
        let half = i32::try_from(v >> 1).unwrap_or(i32::MAX);
        Ok(if v & 1 == 1 { !half } else { half })
    }

    fn read_zigzag_i64(&mut self) -> Result<i64> {
        let v = self.read_varint()?;
        let half = i64::try_from(v >> 1).unwrap_or(i64::MAX);
        Ok(if v & 1 == 1 { !half } else { half })
    }

    fn read_double(&mut self) -> Result<f64> {
        let bytes = self.read_bytes(8)?;
        let mut arr = [0u8; 8];
        arr.copy_from_slice(bytes);
        Ok(f64::from_le_bytes(arr))
    }

    fn read_binary(&mut self) -> Result<Vec<u8>> {
        let raw = self.read_varint()?;
        let len = usize::try_from(raw).map_err(|_| ThriftError::InvalidLength(raw))?;
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
            CTYPE_BYTE => Ok(Value::Byte(i8::from_be_bytes([self.read_byte()?]))),
            CTYPE_I16 => Ok(Value::I16(narrow_i16(self.read_zigzag_i32()?)?)),
            CTYPE_I32 => Ok(Value::I32(self.read_zigzag_i32()?)),
            CTYPE_I64 => Ok(Value::I64(self.read_zigzag_i64()?)),
            CTYPE_DOUBLE => Ok(Value::Double(self.read_double()?)),
            CTYPE_BINARY => Ok(Value::Binary(self.read_binary()?)),
            CTYPE_LIST | CTYPE_SET => self.nested(Self::read_list),
            CTYPE_MAP => self.nested(Self::read_map),
            CTYPE_STRUCT => self.read_struct().map(Value::Struct),
            other => Err(ThriftError::UnknownType(other)),
        }
    }

    fn read_list(&mut self) -> Result<Value> {
        let header = self.read_byte()?;
        let mut size = u64::from(header >> 4);
        let elem_type = decode_element_type(header & 0x0f)?;
        if size == 15 {
            size = self.read_varint()?;
        }
        // Every element takes at least one byte on the wire.
        let size = self.checked_size(size, 1)?;
        let mut items = Vec::with_capacity(size);
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
        // Every entry takes at least one byte for its key and one for its value.
        let size = self.checked_size(size, 2)?;
        let mut items = Vec::with_capacity(size);
        for _ in 0..size {
            let k = self.read_value(key_type, None)?;
            let v = self.read_value(val_type, None)?;
            items.push((k, v));
        }
        Ok(Value::Map(items))
    }

    /// Read a struct: a sequence of field headers/values terminated by STOP.
    /// Fails with [`ThriftError::DepthExceeded`] past [`MAX_DEPTH`] levels.
    pub fn read_struct(&mut self) -> Result<Vec<(i16, Value)>> {
        self.nested(Self::read_struct_fields)
    }

    /// [`Self::read_struct`]'s body, run one nesting level deeper.
    fn read_struct_fields(&mut self) -> Result<Vec<(i16, Value)>> {
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
                narrow_i16(self.read_zigzag_i32()?)?
            } else {
                last_field_id
                    .checked_add(i16::from(delta))
                    .ok_or(ThriftError::InvalidLength(u64::from(delta)))?
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

#[cfg(test)]
mod tests_289 {
    use super::*;

    /// #289: zigzag decoding without reinterpreting casts, all four corners.
    #[test]
    fn zigzag_i32_and_i64_decode_all_corners() {
        fn z32(raw: u64) -> i32 {
            let bytes = encode_varint_test(raw);
            let mut r = Decoder::new(&bytes);
            r.read_zigzag_i32().unwrap()
        }
        fn z64(raw: u64) -> i64 {
            let bytes = encode_varint_test(raw);
            let mut r = Decoder::new(&bytes);
            r.read_zigzag_i64().unwrap()
        }
        assert_eq!(z32(0), 0);
        assert_eq!(z32(1), -1);
        assert_eq!(z32(2), 1);
        assert_eq!(z32(u64::from(u32::MAX)), i32::MIN);
        assert_eq!(z32(u64::from(u32::MAX - 1)), i32::MAX);
        assert_eq!(z64(0), 0);
        assert_eq!(z64(1), -1);
        assert_eq!(z64(u64::MAX), i64::MIN);
        assert_eq!(z64(u64::MAX - 1), i64::MAX);
        // A zigzag i32 whose varint does not fit u32 is corrupt input.
        let bytes = encode_varint_test(1 << 40);
        let mut r = Decoder::new(&bytes);
        assert!(matches!(
            r.read_zigzag_i32(),
            Err(ThriftError::InvalidLength(_))
        ));
    }

    #[test]
    fn narrow_i16_rejects_out_of_range_field_ids() {
        assert_eq!(narrow_i16(300).unwrap(), 300);
        assert_eq!(narrow_i16(-300).unwrap(), -300);
        assert!(matches!(
            narrow_i16(40_000),
            Err(ThriftError::InvalidLength(40_000))
        ));
    }

    fn encode_varint_test(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(byte);
                return out;
            }
            out.push(byte | 0x80);
        }
    }
}

/// #657: hostile lengths, sizes and nesting fail fast, without a panic,
/// stack overflow or an allocation sized by the claimed length.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]
mod tests_657 {
    use super::*;

    fn varint(out: &mut Vec<u8>, mut v: u64) {
        loop {
            let b = u8::try_from(v & 0x7f).unwrap();
            v >>= 7;
            if v == 0 {
                out.push(b);
                return;
            }
            out.push(b | 0x80);
        }
    }

    #[test]
    fn absurd_binary_length_is_an_error() {
        for len in [10, 1 << 31, 1 << 62, u64::MAX] {
            let mut buf = vec![0x18]; // field 1: binary
            varint(&mut buf, len);
            buf.extend_from_slice(b"abc");
            assert!(decode_struct(&buf).is_err(), "length {len}");
        }
    }

    #[test]
    fn absurd_list_size_is_rejected_before_allocating() {
        for size in [5, 1 << 20, u64::from(u32::MAX), u64::MAX] {
            let mut buf = vec![0x19, 0xf5]; // field 1: list<i32>, escaped size
            varint(&mut buf, size);
            buf.extend_from_slice(&[0x02, 0x04, 0x00]);
            assert!(
                matches!(decode_struct(&buf), Err(ThriftError::InvalidLength(n)) if n == size),
                "size {size}"
            );
        }
    }

    #[test]
    fn list_size_matching_the_remaining_bytes_still_decodes() {
        // Three one-byte elements and the closing STOP.
        let buf = [0x19, 0x35, 0x02, 0x04, 0x06, 0x00];
        let (fields, _) = decode_struct(&buf).unwrap();
        assert_eq!(
            fields,
            vec![(
                1,
                Value::List(vec![Value::I32(1), Value::I32(2), Value::I32(3)])
            )]
        );
    }

    #[test]
    fn absurd_map_size_is_rejected_before_allocating() {
        for size in [3, 1 << 40, u64::MAX] {
            let mut buf = vec![0x1b]; // field 1: map
            varint(&mut buf, size);
            buf.extend_from_slice(&[0x55, 0x02, 0x04, 0x00]); // map<i32, i32>
            assert!(
                matches!(decode_struct(&buf), Err(ThriftError::InvalidLength(n)) if n == size),
                "size {size}"
            );
        }
    }

    #[test]
    fn field_id_delta_overflow_is_an_error() {
        // Field id 32767 (long form), then a delta that would pass i16::MAX.
        let mut buf = vec![0x05];
        varint(&mut buf, 65534); // zigzag(32767)
        buf.push(0x02);
        buf.extend_from_slice(&[0x15, 0x02, 0x00]);
        assert!(matches!(
            decode_struct(&buf),
            Err(ThriftError::InvalidLength(1))
        ));
    }

    #[test]
    fn overlong_varint_is_an_error() {
        let mut buf = vec![0x16]; // field 1: i64
        buf.extend_from_slice(&[0xff; 10]);
        buf.push(0x01);
        assert!(matches!(
            decode_struct(&buf),
            Err(ThriftError::InvalidVarint)
        ));
        // Ten bytes is the most a u64 can need, and still decodes.
        let mut buf = vec![0x16];
        buf.extend_from_slice(&[0xff; 9]);
        buf.extend_from_slice(&[0x01, 0x00]);
        let (fields, _) = decode_struct(&buf).unwrap();
        assert_eq!(fields, vec![(1, Value::I64(i64::MIN))]);
    }

    /// `depth` structs, each the first field of its parent.
    fn nested_structs(depth: usize) -> Vec<u8> {
        let mut buf = vec![0x1c; depth - 1];
        buf.extend(vec![0x00; depth]);
        buf
    }

    #[test]
    fn struct_nesting_is_capped() {
        assert!(decode_struct(&nested_structs(MAX_DEPTH)).is_ok());
        assert!(matches!(
            decode_struct(&nested_structs(MAX_DEPTH + 1)),
            Err(ThriftError::DepthExceeded)
        ));
        assert!(matches!(
            decode_struct(&nested_structs(1_000_000)),
            Err(ThriftError::DepthExceeded)
        ));
    }

    #[test]
    fn list_nesting_is_capped() {
        // field 1: list<list>, then list<list> of size 1 again and again.
        let mut buf = vec![0x19];
        buf.extend(vec![0x19; 1_000_000]);
        assert!(matches!(
            decode_struct(&buf),
            Err(ThriftError::DepthExceeded)
        ));
    }

    #[test]
    fn map_nesting_is_capped() {
        // field 1: map, then map<map, i32> of size 1 again and again.
        let mut buf = vec![0x1b];
        for _ in 0..1_000_000 {
            buf.extend_from_slice(&[0x01, 0xb5]);
        }
        assert!(matches!(
            decode_struct(&buf),
            Err(ThriftError::DepthExceeded)
        ));
    }

    #[test]
    fn depth_resets_between_sibling_structs() {
        // Two siblings, each nested to the cap, both decode.
        let inner = nested_structs(MAX_DEPTH - 1);
        let mut buf = vec![0x1c];
        buf.extend_from_slice(&inner);
        buf.push(0x1c);
        buf.extend_from_slice(&inner);
        buf.push(0x00);
        assert!(decode_struct(&buf).is_ok());
    }
}
