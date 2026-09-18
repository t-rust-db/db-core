//! Hand-rolled Snappy block-format decompressor (the raw, unframed format
//! Parquet uses — not the "framed" stream format).
//!
//! Spec: https://github.com/google/snappy/blob/main/format_description.txt

use std::fmt;

#[derive(Debug)]
pub enum SnappyError {
    UnexpectedEof,
    InvalidVarint,
    /// The declared uncompressed length does not fit `usize`.
    InvalidLength(u64),
    InvalidCopyOffset,
    SizeMismatch {
        expected: usize,
        actual: usize,
    },
}

impl fmt::Display for SnappyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SnappyError::UnexpectedEof => write!(f, "unexpected end of snappy input"),
            SnappyError::InvalidVarint => write!(f, "invalid snappy varint"),
            SnappyError::InvalidLength(n) => write!(f, "declared snappy length out of range: {n}"),
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

/// Width of the fixed-size copy the hot paths use for any literal or copy
/// of at most this many bytes: one 16-byte load and store instead of a
/// length-dependent `memcpy` call. Real Parquet pages are dominated by
/// such short ops (#495: `PLAIN` doubles decode as ~10M literals of ~2
/// bytes and ~10M copies of 5-7 bytes per 80 MB), so the per-op cost is
/// what decides throughput, not bytes moved.
const WIDE: usize = 16;

/// Decompress a raw Snappy block. `uncompressed_size` is the page
/// header's declared size, used to pre-size the output; the block's own
/// length preamble is what the output is validated against.
///
/// The output is allocated once at its final length plus [`WIDE`] bytes
/// of slack and written through `&mut [u8]`, so no op pays a capacity
/// check, and short ops copy a fixed `WIDE` bytes: the bytes past the
/// op's own length land in slack or in positions a later op overwrites
/// (a copy only ever reads below its write point, so it never observes
/// them). A block that would write past its declared length fails at that
/// op instead of after the whole block.
#[allow(
    clippy::indexing_slicing,
    reason = "every fixed-width slice is guarded by the `<= len()` comparison on the line above it"
)]
pub fn decompress(data: &[u8], uncompressed_size: usize) -> Result<Vec<u8>> {
    let mut pos = 0usize;
    let declared = read_varint(data, &mut pos)?;
    let declared_len =
        usize::try_from(declared).map_err(|_| SnappyError::InvalidLength(declared))?;
    let mut out = vec![0u8; declared_len.max(uncompressed_size).saturating_add(WIDE)];
    let mut dst = 0usize;

    while pos < data.len() {
        let tag = *data.get(pos).ok_or(SnappyError::UnexpectedEof)?;
        pos += 1;
        let (len, offset) = match tag & 0x03 {
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
                if len <= WIDE && pos + WIDE <= data.len() && dst + WIDE <= out.len() {
                    let wide: [u8; WIDE] = data[pos..pos + WIDE]
                        .try_into()
                        .map_err(|_| SnappyError::UnexpectedEof)?;
                    out[dst..dst + WIDE].copy_from_slice(&wide);
                } else {
                    let bytes = data.get(pos..pos + len).ok_or(SnappyError::UnexpectedEof)?;
                    out.get_mut(dst..dst + len)
                        .ok_or(SnappyError::SizeMismatch {
                            expected: declared_len,
                            actual: dst + len,
                        })?
                        .copy_from_slice(bytes);
                }
                dst += len;
                pos += len;
                continue;
            }
            1 => {
                // Copy with 1-byte offset: length in bits 2-4 (+4), offset
                // is 3 bits from the tag (top) plus 1 following byte.
                let len = ((tag >> 2) & 0x07) as usize + 4;
                let offset_hi = ((tag >> 5) & 0x07) as usize;
                let offset_lo = *data.get(pos).ok_or(SnappyError::UnexpectedEof)? as usize;
                pos += 1;
                (len, (offset_hi << 8) | offset_lo)
            }
            2 => {
                // Copy with 2-byte little-endian offset, length in the
                // tag's upper 6 bits (+1).
                let len = (tag >> 2) as usize + 1;
                let lo = *data.get(pos).ok_or(SnappyError::UnexpectedEof)? as usize;
                let hi = *data.get(pos + 1).ok_or(SnappyError::UnexpectedEof)? as usize;
                pos += 2;
                (len, lo | (hi << 8))
            }
            _ => {
                // Copy with 4-byte little-endian offset (tag & 0x03 == 3).
                let len = (tag >> 2) as usize + 1;
                let bytes: [u8; 4] = data
                    .get(pos..pos + 4)
                    .and_then(|b| b.try_into().ok())
                    .ok_or(SnappyError::UnexpectedEof)?;
                pos += 4;
                (len, u32::from_le_bytes(bytes) as usize)
            }
        };
        if offset == 0 || offset > dst {
            return Err(SnappyError::InvalidCopyOffset);
        }
        let src = dst - offset;
        if len <= WIDE && offset >= len && dst + WIDE <= out.len() {
            // `src < dst`, so `src + WIDE <= out.len()` follows from the
            // `dst` check; `offset >= len` means the `len` bytes that
            // matter were all written before this op.
            let wide: [u8; WIDE] = out[src..src + WIDE]
                .try_into()
                .map_err(|_| SnappyError::UnexpectedEof)?;
            out[dst..dst + WIDE].copy_from_slice(&wide);
        } else {
            copy_from_offset(&mut out, dst, offset, len)?;
        }
        dst += len;
    }

    if dst != declared_len {
        return Err(SnappyError::SizeMismatch {
            expected: declared_len,
            actual: dst,
        });
    }
    out.truncate(declared_len);
    Ok(out)
}

/// Write `len` bytes at `out[dst..]`, copied from `offset` bytes before
/// `dst` -- the exact-length path for copies the fixed-width fast path in
/// [`decompress`] can't take: longer than [`WIDE`], too close to the end
/// of the buffer, or self-overlapping (`offset < len`, valid Snappy: it
/// repeats the last `offset` bytes as a pattern). An overlapping copy is
/// done in runs that never read past what has already been written; each
/// run may take at most the bytes between the source start and the
/// current write point, which grows with every run, so the pattern
/// doubles per iteration.
fn copy_from_offset(out: &mut [u8], dst: usize, offset: usize, len: usize) -> Result<()> {
    if offset == 0 || offset > dst {
        return Err(SnappyError::InvalidCopyOffset);
    }
    let end = dst + len;
    if end > out.len() {
        return Err(SnappyError::SizeMismatch {
            expected: out.len(),
            actual: end,
        });
    }
    let src = dst - offset;
    if offset >= len {
        out.copy_within(src..src + len, dst);
        return Ok(());
    }
    let mut done = 0usize;
    while done < len {
        let run = (offset + done).min(len - done);
        out.copy_within(src..src + run, dst + done);
        done += run;
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
    fn an_overlapping_copy_repeats_the_pattern_across_doubling_runs() {
        // literal "abc", then a 1-byte-offset copy of length 11 at offset
        // 3: the source is only 3 bytes long when the copy starts, so the
        // output must be the pattern repeated (runs of 3, 6, then 2).
        let mut compressed = vec![14u8];
        compressed.push(2u8 << 2);
        compressed.extend_from_slice(b"abc");
        let len_field = 11u8 - 4;
        compressed.push((len_field << 2) | 0x01);
        compressed.push(3u8);
        let out = decompress(&compressed, 14).unwrap();
        assert_eq!(out, b"abcabcabcabcab");
    }

    #[test]
    fn a_non_overlapping_copy_reaches_back_past_the_copied_length() {
        // literal "0123456789", then copy length 4 from offset 10: bytes
        // "0123" -- the memcpy branch, source fully behind the copy.
        let mut compressed = vec![14u8];
        compressed.push(9u8 << 2);
        compressed.extend_from_slice(b"0123456789");
        compressed.push((3u8 << 2) | 0x02);
        compressed.extend_from_slice(&10u16.to_le_bytes());
        let out = decompress(&compressed, 14).unwrap();
        assert_eq!(out, b"01234567890123");
    }

    #[test]
    fn snappy_rejects_copy_offset_beyond_start() {
        let mut compressed = vec![4u8];
        compressed.push(0x01u8); // copy, len=4, offset=0
        compressed.push(0);
        let err = decompress(&compressed, 4).unwrap_err();
        assert!(matches!(err, SnappyError::InvalidCopyOffset));
    }

    #[test]
    fn snappy_roundtrips_with_2byte_copy() {
        // "WXYZWXYZ": literal "WXYZ" (4 bytes), then a 2-byte-offset copy
        // of length 4 at offset 4 (self-overlapping, tag & 0x03 == 2).
        let mut compressed = vec![8u8]; // declared length = 8
        compressed.push(3u8 << 2); // literal, length_tag=3 => length 4
        compressed.extend_from_slice(b"WXYZ");
        let tag = (3u8 << 2) | 0x02; // copy-2byte, len_tag=3 => length 4
        compressed.push(tag);
        compressed.extend_from_slice(&4u16.to_le_bytes());
        let out = decompress(&compressed, 8).unwrap();
        assert_eq!(out, b"WXYZWXYZ");
    }

    #[test]
    fn snappy_roundtrips_with_4byte_copy() {
        // "ABCDABCD": literal "ABCD" (4 bytes), then a 4-byte-offset copy
        // of length 4 at offset 4 (self-overlapping, tag & 0x03 == 3).
        let mut compressed = vec![8u8];
        compressed.push(3u8 << 2);
        compressed.extend_from_slice(b"ABCD");
        let tag = (3u8 << 2) | 0x03; // copy-4byte, len_tag=3 => length 4
        compressed.push(tag);
        compressed.extend_from_slice(&4u32.to_le_bytes());
        let out = decompress(&compressed, 8).unwrap();
        assert_eq!(out, b"ABCDABCD");
    }

    #[test]
    fn snappy_roundtrips_a_long_literal_needing_an_extra_length_byte() {
        // A literal of 65 bytes: the tag's 6-bit length field can only
        // reach 60 (tag value 60 => "1 extra byte" per the format), so
        // 60+ needs `extra_bytes = len_tag - 59` following bytes holding
        // `len - 1` little-endian.
        let literal: Vec<u8> = (0..65u8).collect();
        let mut compressed = vec![65u8]; // declared length = 65
        compressed.push(60u8 << 2); // len_tag = 60 => 1 extra length byte
        compressed.push(64u8); // len - 1 = 64 => len = 65
        compressed.extend_from_slice(&literal);
        let out = decompress(&compressed, 65).unwrap();
        assert_eq!(out, literal);
    }

    #[test]
    fn an_over_long_varint_is_invalid() {
        // Five continuation bytes with no terminator: `read_varint`'s
        // `shift >= 35` guard trips before a sixth byte is even read.
        let compressed = vec![0x80, 0x80, 0x80, 0x80, 0x80];
        let err = decompress(&compressed, 0).unwrap_err();
        assert!(matches!(err, SnappyError::InvalidVarint));
    }

    #[test]
    fn a_truncated_varint_is_unexpected_eof() {
        // A single continuation byte with nothing after it.
        let compressed = vec![0x80];
        let err = decompress(&compressed, 0).unwrap_err();
        assert!(matches!(err, SnappyError::UnexpectedEof));
    }

    #[test]
    fn a_short_decode_is_a_size_mismatch() {
        // Declares length 5 but the block only encodes a 4-byte literal.
        let mut compressed = vec![5u8];
        compressed.push(3u8 << 2);
        compressed.extend_from_slice(b"abcd");
        let err = decompress(&compressed, 5).unwrap_err();
        assert!(matches!(
            err,
            SnappyError::SizeMismatch {
                expected: 5,
                actual: 4
            }
        ));
    }
}

#[cfg(test)]
#[allow(non_snake_case, clippy::unwrap_used)]
mod mcdc_vectors {
    //! Tagged MC/DC vectors for this file's multi-leaf decisions
    //! (`mcdc__<id>__vN`, joined to `tests/mcdc/obligations.json`
    //! by `make test-mcdc`; db-core#299 MC/DC backfill).

    use super::copy_from_offset;

    use super::{decompress, SnappyError, WIDE};

    /// A raw block: declared length, then the given ops verbatim.
    fn block(declared: u8, ops: &[&[u8]]) -> Vec<u8> {
        let mut out = vec![declared];
        for op in ops {
            out.extend_from_slice(op);
        }
        out
    }

    fn literal(bytes: &[u8]) -> Vec<u8> {
        let mut op = vec![((bytes.len() - 1) as u8) << 2];
        op.extend_from_slice(bytes);
        op
    }

    /// A copy with a 2-byte offset (tag & 0x03 == 2): `len` in 1..=64.
    fn copy2(len: usize, offset: u16) -> Vec<u8> {
        let mut op = vec![(((len - 1) as u8) << 2) | 0x02];
        op.extend_from_slice(&offset.to_le_bytes());
        op
    }

    // storage_column_parquet_compression_snappy_decompress_116ba85b: `len <= WIDE && pos + WIDE <= data.len() && dst + WIDE <= out.len()`
    #[test]
    fn mcdc__storage_column_parquet_compression_snappy_decompress_116ba85b__v1_short_literal_with_input_and_output_slack_takes_the_wide_path(
    ) {
        // "abc" is followed by a 20-byte literal, so 16 input bytes exist
        // past it and the output has its WIDE slack: the fixed-width copy
        // must still yield exactly the 3 literal bytes.
        let tail: Vec<u8> = (100..120u8).collect();
        let data = block(23, &[&literal(b"abc"), &literal(&tail)]);
        let out = decompress(&data, 0).unwrap();
        assert_eq!(&out[..3], b"abc");
        assert_eq!(&out[3..], &tail[..]);
    }

    #[test]
    fn mcdc__storage_column_parquet_compression_snappy_decompress_116ba85b__v2_literal_longer_than_wide_takes_the_exact_path(
    ) {
        let bytes: Vec<u8> = (0..(WIDE as u8 + 4)).collect();
        let data = block(WIDE as u8 + 4, &[&literal(&bytes)]);
        assert_eq!(decompress(&data, 0).unwrap(), bytes);
    }

    #[test]
    fn mcdc__storage_column_parquet_compression_snappy_decompress_116ba85b__v3_short_literal_at_the_end_of_input_takes_the_exact_path(
    ) {
        // Fewer than WIDE input bytes remain after the tag: the wide load
        // would run off `data`, so the exact copy is used.
        let data = block(5, &[&literal(b"hello")]);
        assert_eq!(decompress(&data, 5).unwrap(), b"hello");
    }

    #[test]
    fn mcdc__storage_column_parquet_compression_snappy_decompress_116ba85b__v4_short_literal_past_the_output_slack_takes_the_exact_path_and_the_block_is_rejected(
    ) {
        // Only an over-long block can push `dst` past `declared + WIDE`:
        // 18 declared, then 18 + 4 + 2 + 17 bytes of literals. The 2-byte
        // literal at dst 22 has 16 input bytes after it but no output
        // slack left (22 + 16 > 34), so it goes the exact way, and the
        // block fails on the 17-byte literal that follows.
        let a: Vec<u8> = (0..18u8).collect();
        let pad: Vec<u8> = (0..17u8).collect();
        let data = block(
            18,
            &[
                &literal(&a),
                &literal(b"wxyz"),
                &literal(b"pq"),
                &literal(&pad),
            ],
        );
        let err = decompress(&data, 0).unwrap_err();
        assert!(matches!(
            err,
            SnappyError::SizeMismatch { expected: 18, .. }
        ));
    }

    // storage_column_parquet_compression_snappy_decompress_d541d5eb: `offset == 0 || offset > dst`
    #[test]
    fn mcdc__storage_column_parquet_compression_snappy_decompress_d541d5eb__v1_offset_zero() {
        let data = block(6, &[&literal(b"ab"), &copy2(4, 0)]);
        assert!(matches!(
            decompress(&data, 6).unwrap_err(),
            SnappyError::InvalidCopyOffset
        ));
    }

    #[test]
    fn mcdc__storage_column_parquet_compression_snappy_decompress_d541d5eb__v2_offset_before_the_start(
    ) {
        let data = block(6, &[&literal(b"ab"), &copy2(4, 3)]);
        assert!(matches!(
            decompress(&data, 6).unwrap_err(),
            SnappyError::InvalidCopyOffset
        ));
    }

    #[test]
    fn mcdc__storage_column_parquet_compression_snappy_decompress_d541d5eb__v3_offset_within_the_output(
    ) {
        let data = block(4, &[&literal(b"ab"), &copy2(2, 2)]);
        assert_eq!(decompress(&data, 4).unwrap(), b"abab");
    }

    // storage_column_parquet_compression_snappy_decompress_8a5f3137: `len <= WIDE && offset >= len && dst + WIDE <= out.len()`
    #[test]
    fn mcdc__storage_column_parquet_compression_snappy_decompress_8a5f3137__v1_short_non_overlapping_copy_with_slack_takes_the_wide_path(
    ) {
        let data = block(8, &[&literal(b"abcd"), &copy2(4, 4)]);
        assert_eq!(decompress(&data, 8).unwrap(), b"abcdabcd");
    }

    #[test]
    fn mcdc__storage_column_parquet_compression_snappy_decompress_8a5f3137__v2_copy_longer_than_wide_takes_copy_within(
    ) {
        let bytes: Vec<u8> = (0..20u8).collect();
        let data = block(40, &[&literal(&bytes), &copy2(20, 20)]);
        let out = decompress(&data, 40).unwrap();
        assert_eq!(&out[..20], &bytes[..]);
        assert_eq!(&out[20..], &bytes[..]);
    }

    #[test]
    fn mcdc__storage_column_parquet_compression_snappy_decompress_8a5f3137__v3_overlapping_copy_takes_the_pattern_path(
    ) {
        let data = block(9, &[&literal(b"xyz"), &copy2(6, 3)]);
        assert_eq!(decompress(&data, 9).unwrap(), b"xyzxyzxyz");
    }

    #[test]
    fn mcdc__storage_column_parquet_compression_snappy_decompress_8a5f3137__v4_short_copy_past_the_output_slack_takes_the_exact_path_and_the_block_is_rejected(
    ) {
        // 8 declared (out = 24): an 8-byte literal, a 4-byte copy at dst 8
        // (8 + 16 <= 24, wide), then a 4-byte copy at dst 12 (12 + 16 >
        // 24, exact). Both copies are correct; the block is over-long.
        let data = block(8, &[&literal(b"01234567"), &copy2(4, 4), &copy2(4, 4)]);
        let err = decompress(&data, 0).unwrap_err();
        assert!(matches!(
            err,
            SnappyError::SizeMismatch {
                expected: 8,
                actual: 16
            }
        ));
    }

    // storage_column_parquet_compression_snappy_copy_from_offset_d541d5eb: `offset == 0 || offset > dst`
    #[test]
    fn mcdc__storage_column_parquet_compression_snappy_copy_from_offset_d541d5eb__v1_offset_zero() {
        let mut out = vec![b'a', b'b', 0];
        assert!(copy_from_offset(&mut out, 2, 0, 1).is_err());
    }

    #[test]
    fn mcdc__storage_column_parquet_compression_snappy_copy_from_offset_d541d5eb__v2_offset_beyond_out_len(
    ) {
        let mut out = vec![b'a', b'b', 0];
        assert!(copy_from_offset(&mut out, 2, 3, 1).is_err());
    }

    #[test]
    fn mcdc__storage_column_parquet_compression_snappy_copy_from_offset_d541d5eb__v3_offset_within_range_succeeds(
    ) {
        let mut out = vec![b'a', b'b', 0];
        assert!(copy_from_offset(&mut out, 2, 2, 1).is_ok());
        assert_eq!(out, vec![b'a', b'b', b'a']);
    }
}
