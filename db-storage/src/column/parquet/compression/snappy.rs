//! Hand-rolled Snappy block-format decompressor (the raw, unframed format
//! Parquet uses — not the "framed" stream format).
//!
//! Spec: https://github.com/google/snappy/blob/main/format_description.txt

use std::fmt;

#[derive(Debug)]
pub enum SnappyError {
    UnexpectedEof,
    InvalidVarint,
    InvalidCopyOffset,
    SizeMismatch { expected: usize, actual: usize },
}

impl fmt::Display for SnappyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SnappyError::UnexpectedEof => write!(f, "unexpected end of snappy input"),
            SnappyError::InvalidVarint => write!(f, "invalid snappy varint"),
            SnappyError::InvalidCopyOffset => write!(
                f,
                "snappy copy references data before the start of the buffer"
            ),
            SnappyError::SizeMismatch { expected, actual } => {
                write!(
                    f,
                    "snappy decompressed size mismatch: expected {expected}, got {actual}"
                )
            }
        }
    }
}

impl std::error::Error for SnappyError {}

type Result<T> = std::result::Result<T, SnappyError>;

/// Read a Snappy-format unsigned varint (little-endian base-128, 7 bits
/// of payload per byte, continuation bit in the high bit).
fn read_varint(data: &[u8], pos: &mut usize) -> Result<u64> {
    let mut result = 0u64;
    let mut shift = 0u32;
    loop {
        if shift >= 35 {
            return Err(SnappyError::InvalidVarint);
        }
        let byte = *data.get(*pos).ok_or(SnappyError::UnexpectedEof)?;
        *pos += 1;
        result |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    Ok(result)
}

/// Decompress a raw Snappy block. `uncompressed_size` is only used to
/// pre-size the output buffer; the actual length comes from the
/// preamble and is validated against it.
pub fn decompress(data: &[u8], uncompressed_size: usize) -> Result<Vec<u8>> {
    let mut pos = 0usize;
    let declared_len = read_varint(data, &mut pos)? as usize;
    let mut out = Vec::with_capacity(declared_len.max(uncompressed_size));

    while pos < data.len() {
        let tag = *data.get(pos).ok_or(SnappyError::UnexpectedEof)?;
        pos += 1;
        match tag & 0x03 {
            0 => {
                // Literal: length encoded in the tag's upper 6 bits, or
                // in 1-4 following little-endian bytes when that field
                // is >= 60 (tag value 60+n means "length follows in the
                // next n+1 bytes").
                let len_tag = (tag >> 2) as usize;
                let len = if len_tag < 60 {
                    len_tag + 1
                } else {
                    let extra_bytes = len_tag - 59;
                    let mut len = 0usize;
                    for i in 0..extra_bytes {
                        let b = *data.get(pos + i).ok_or(SnappyError::UnexpectedEof)?;
                        len |= (b as usize) << (8 * i);
                    }
                    pos += extra_bytes;
                    len + 1
                };
                let bytes = data.get(pos..pos + len).ok_or(SnappyError::UnexpectedEof)?;
                out.extend_from_slice(bytes);
                pos += len;
            }
            1 => {
                // Copy with 1-byte offset: length in bits 2-4 (+4), offset
                // is 3 bits from the tag (top) plus 1 following byte.
                let len = ((tag >> 2) & 0x07) as usize + 4;
                let offset_hi = ((tag >> 5) & 0x07) as usize;
                let offset_lo = *data.get(pos).ok_or(SnappyError::UnexpectedEof)? as usize;
                pos += 1;
                let offset = (offset_hi << 8) | offset_lo;
                copy_from_offset(&mut out, offset, len)?;
            }
            2 => {
                // Copy with 2-byte little-endian offset, length in the
                // tag's upper 6 bits (+1).
                let len = (tag >> 2) as usize + 1;
                let lo = *data.get(pos).ok_or(SnappyError::UnexpectedEof)? as usize;
                let hi = *data.get(pos + 1).ok_or(SnappyError::UnexpectedEof)? as usize;
                pos += 2;
                let offset = lo | (hi << 8);
                copy_from_offset(&mut out, offset, len)?;
            }
            _ => {
                // Copy with 4-byte little-endian offset (tag & 0x03 == 3).
                let len = (tag >> 2) as usize + 1;
                let bytes: [u8; 4] = data
                    .get(pos..pos + 4)
                    .and_then(|b| b.try_into().ok())
                    .ok_or(SnappyError::UnexpectedEof)?;
                let offset = u32::from_le_bytes(bytes) as usize;
                pos += 4;
                copy_from_offset(&mut out, offset, len)?;
            }
        }
    }

    if out.len() != declared_len {
        return Err(SnappyError::SizeMismatch {
            expected: declared_len,
            actual: out.len(),
        });
    }
    Ok(out)
}

/// Append `len` bytes to `out`, copied from `offset` bytes before the
/// current end — self-overlapping copies (offset < len) are valid and
/// must be copied byte-by-byte (a `memcpy` would read past the source
/// before it's written).
fn copy_from_offset(out: &mut Vec<u8>, offset: usize, len: usize) -> Result<()> {
    if offset == 0 || offset > out.len() {
        return Err(SnappyError::InvalidCopyOffset);
    }
    let start = out.len() - offset;
    for i in 0..len {
        let byte = out[start + i];
        out.push(byte);
    }
    Ok(())
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

    #[test]
    fn snappy_roundtrips_literal_only_block() {
        let mut compressed = vec![5u8];
        compressed.push(4u8 << 2); // literal, length-1=4 => length 5
        compressed.extend_from_slice(b"hello");
        let out = decompress(&compressed, 5).unwrap();
        assert_eq!(out, b"hello");
    }

    #[test]
    fn snappy_roundtrips_with_1byte_copy() {
        // "abcaabca": literal "abca" (4 bytes), then a 1-byte-offset copy of
        // length 4 at offset 4 (self-overlapping: source == destination range).
        let mut compressed = vec![8u8]; // declared length = 8
        compressed.push(3u8 << 2); // literal, length_tag=3 => length 4
        compressed.extend_from_slice(b"abca");
        let len_field = 0u8; // copy length 4 => (len - 4) = 0
        let offset = 4usize;
        let tag = (((offset >> 8) as u8) << 5) | (len_field << 2) | 0x01;
        compressed.push(tag);
        compressed.push((offset & 0xff) as u8);
        let out = decompress(&compressed, 8).unwrap();
        assert_eq!(out, b"abcaabca");
    }

    #[test]
    fn snappy_rejects_copy_offset_beyond_start() {
        let mut compressed = vec![4u8];
        compressed.push(0x01u8); // copy, len=4, offset=0
        compressed.push(0);
        let err = decompress(&compressed, 4).unwrap_err();
        assert!(matches!(err, SnappyError::InvalidCopyOffset));
    }
}
