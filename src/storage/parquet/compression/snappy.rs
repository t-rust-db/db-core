//! Hand-rolled Snappy block-format decompressor (the raw, unframed format
//! Parquet uses — not the "framed" stream format).
//!
//! Spec: https://github.com/google/snappy/blob/main/format_description.txt

use std::fmt;

/// A malformed Snappy block. The input is untrusted page data, so every
/// way it can be inconsistent is an error, never a panic.
#[derive(Debug)]
pub enum SnappyError {
    /// The block ends inside a varint, an op's operands, or a literal.
    UnexpectedEof,
    /// The length preamble is longer than the five bytes a 32-bit length
    /// needs.
    InvalidVarint,
    /// The declared uncompressed length does not fit `usize`, or is more
    /// than the block's ops could ever expand to (see [`MAX_EXPANSION`]).
    InvalidLength(u64),
    /// A copy with offset zero, or one reaching before the start of the
    /// output.
    InvalidCopyOffset,
    /// The ops decode to more or fewer bytes than the block declares.
    SizeMismatch {
        /// The length the block declares.
        expected: usize,
        /// The length the ops produced, or tried to write up to.
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
/// of payload per byte, continuation bit in the high bit) from the front
/// of `rest`, advancing it past the varint.
fn read_varint(rest: &mut &[u8]) -> Result<u64> {
    let mut result = 0u64;
    let mut shift = 0u32;
    loop {
        if shift >= 35 {
            return Err(SnappyError::InvalidVarint);
        }
        let (&byte, tail) = rest.split_first().ok_or(SnappyError::UnexpectedEof)?;
        *rest = tail;
        result |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
        shift = shift.saturating_add(7);
    }
}

/// Width of the fixed-size copy the hot paths use for any literal or copy
/// of at most this many bytes: one 16-byte load and store instead of a
/// length-dependent `memcpy` call. Real Parquet pages are dominated by
/// such short ops (#495: `PLAIN` doubles decode as ~10M literals of ~2
/// bytes and ~10M copies of 5-7 bytes per 80 MB), so the per-op cost is
/// what decides throughput, not bytes moved.
const WIDE: usize = 16;

/// The most output any Snappy op produces per input byte, rounded up: a
/// 3-byte copy with a 2-byte offset writes at most 64 bytes (64 / 3 < 22),
/// a 2-byte copy at most 11, a 5-byte copy at most 64, and a literal never
/// more than its own length. A block declaring more than its op bytes
/// times this cannot decode to that length, so it is rejected before the
/// output is allocated: a few header bytes cannot claim a 4 GB buffer.
const MAX_EXPANSION: usize = 22;

/// Decompress a raw Snappy block, validated against the block's own
/// length preamble.
///
/// The output is allocated once at its final length plus [`WIDE`] bytes
/// of slack and written through `&mut [u8]`, so no op pays a capacity
/// check, and short ops copy a fixed `WIDE` bytes: the bytes past the
/// op's own length land in slack or in positions a later op overwrites
/// (a copy only ever reads below its write point, so it never observes
/// them). A block that would write past its declared length fails at that
/// op instead of after the whole block.
pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    let mut rest = data;
    let declared = read_varint(&mut rest)?;
    let declared_len = usize::try_from(declared)
        .ok()
        .filter(|&n| n <= rest.len().saturating_mul(MAX_EXPANSION))
        .ok_or(SnappyError::InvalidLength(declared))?;
    let mut out = vec![0u8; declared_len.saturating_add(WIDE)];
    let mut dst = 0usize;

    while let Some((&tag, tail)) = rest.split_first() {
        rest = tail;
        // The length fields below are at most 6 bits wide, so the
        // `saturating_add`s of their +1/+4 bias never saturate.
        let (len, offset) = match tag & 0x03 {
            0 => {
                let len = literal_len(tag, &mut rest)?;
                if len <= WIDE && copy_wide(&mut out, dst, rest) {
                    // `copy_wide` found at least `WIDE >= len` bytes both
                    // in `rest` and in `out[dst..]`: neither step can fail.
                    rest = rest.get(len..).unwrap_or_default();
                    dst = dst.saturating_add(len);
                    continue;
                }
                let (bytes, tail) = rest
                    .split_at_checked(len)
                    .ok_or(SnappyError::UnexpectedEof)?;
                let end = advance(dst, len, declared_len)?;
                out.get_mut(dst..end)
                    .ok_or(SnappyError::SizeMismatch {
                        expected: declared_len,
                        actual: end,
                    })?
                    .copy_from_slice(bytes);
                rest = tail;
                dst = end;
                continue;
            }
            1 => {
                // Copy with 1-byte offset: length in bits 2-4 (+4), offset
                // is 3 bits from the tag (top) plus 1 following byte.
                let (&lo, tail) = rest.split_first().ok_or(SnappyError::UnexpectedEof)?;
                rest = tail;
                let len = usize::from((tag >> 2) & 0x07).saturating_add(4);
                (len, (usize::from(tag >> 5) << 8) | usize::from(lo))
            }
            2 => {
                // Copy with 2-byte little-endian offset, length in the
                // tag's upper 6 bits (+1).
                let (bytes, tail) = rest
                    .split_first_chunk::<2>()
                    .ok_or(SnappyError::UnexpectedEof)?;
                rest = tail;
                let len = usize::from(tag >> 2).saturating_add(1);
                (len, usize::from(u16::from_le_bytes(*bytes)))
            }
            _ => {
                // Copy with 4-byte little-endian offset (tag & 0x03 == 3).
                let (bytes, tail) = rest
                    .split_first_chunk::<4>()
                    .ok_or(SnappyError::UnexpectedEof)?;
                rest = tail;
                let len = usize::from(tag >> 2).saturating_add(1);
                let offset = usize::try_from(u32::from_le_bytes(*bytes))
                    .map_err(|_| SnappyError::InvalidCopyOffset)?;
                (len, offset)
            }
        };
        if offset == 0 || offset > dst {
            return Err(SnappyError::InvalidCopyOffset);
        }
        let src = dst
            .checked_sub(offset)
            .ok_or(SnappyError::InvalidCopyOffset)?;
        // `offset >= len`: the `len` bytes that matter were all written
        // before this op, so the fixed-width load sees only final bytes.
        if len <= WIDE && offset >= len && copy_wide_within(&mut out, src, dst) {
            // `out[dst..]` had `WIDE >= len` bytes: cannot saturate.
            dst = dst.saturating_add(len);
        } else {
            copy_from_offset(&mut out, dst, offset, len)?;
            dst = advance(dst, len, declared_len)?;
        }
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

/// A literal op's length: the tag's upper 6 bits plus one, or, for a tag
/// value of 60-63, the 1-4 little-endian bytes that follow it (plus one),
/// advancing `rest` past them.
#[inline(always)]
fn literal_len(tag: u8, rest: &mut &[u8]) -> Result<usize> {
    let len_tag = tag >> 2;
    let extra = match len_tag {
        // At most 59 + 1: cannot saturate.
        0..=59 => return Ok(usize::from(len_tag).saturating_add(1)),
        60 => 1,
        61 => 2,
        62 => 3,
        _ => 4,
    };
    let (bytes, tail) = rest
        .split_at_checked(extra)
        .ok_or(SnappyError::UnexpectedEof)?;
    *rest = tail;
    let minus_one = bytes
        .iter()
        .rev()
        .fold(0usize, |acc, &b| (acc << 8) | usize::from(b));
    // Only a 4-byte length on a 32-bit target can overflow here, and no
    // input that long fits in memory alongside it.
    minus_one.checked_add(1).ok_or(SnappyError::UnexpectedEof)
}

/// `dst + len`, the write point after an op that has already been
/// checked to fit the output; overflow is reported as a size mismatch.
#[inline(always)]
fn advance(dst: usize, len: usize, declared_len: usize) -> Result<usize> {
    dst.checked_add(len).ok_or(SnappyError::SizeMismatch {
        expected: declared_len,
        actual: usize::MAX,
    })
}

/// Copy the first [`WIDE`] bytes of `src` to `out[dst..dst + WIDE]` when
/// both have that many bytes and return `true`; otherwise write nothing
/// and return `false`, leaving the op to the exact-length path.
#[inline(always)]
fn copy_wide(out: &mut [u8], dst: usize, src: &[u8]) -> bool {
    match (
        src.first_chunk::<WIDE>(),
        out.get_mut(dst..).and_then(<[u8]>::first_chunk_mut::<WIDE>),
    ) {
        (Some(from), Some(to)) => {
            *to = *from;
            true
        }
        _ => false,
    }
}

/// [`copy_wide`] within `out`, from `out[src..]` to `out[dst..]`.
#[inline(always)]
fn copy_wide_within(out: &mut [u8], src: usize, dst: usize) -> bool {
    let Some(chunk) = out
        .get(src..)
        .and_then(<[u8]>::first_chunk::<WIDE>)
        .copied()
    else {
        return false;
    };
    copy_wide(out, dst, &chunk)
}

/// Write `len` bytes at `out[dst..]`, copied from `offset` bytes before
/// `dst` -- the exact-length path for copies the fixed-width fast path in
/// [`decompress`] can't take: longer than [`WIDE`], too close to the end
/// of the buffer, or self-overlapping (`offset < len`, valid Snappy: it
/// repeats the last `offset` bytes as a pattern). An overlapping copy
/// first writes the `offset` source bytes once, then doubles what it has
/// written until `len` bytes are done: the written prefix is a whole
/// number of pattern periods, so copying it forward continues the pattern.
fn copy_from_offset(out: &mut [u8], dst: usize, offset: usize, len: usize) -> Result<()> {
    if offset == 0 || offset > dst {
        return Err(SnappyError::InvalidCopyOffset);
    }
    let src = dst
        .checked_sub(offset)
        .ok_or(SnappyError::InvalidCopyOffset)?;
    let mismatch = SnappyError::SizeMismatch {
        expected: out.len(),
        actual: dst.saturating_add(len),
    };
    let (head, tail) = out
        .split_at_mut_checked(dst)
        .ok_or(SnappyError::InvalidCopyOffset)?;
    let target = tail.get_mut(..len).ok_or(mismatch)?;
    let pattern = head.get(src..).ok_or(SnappyError::InvalidCopyOffset)?;
    let first = pattern.len().min(len);
    if let (Some(to), Some(from)) = (target.get_mut(..first), pattern.get(..first)) {
        to.copy_from_slice(from);
    }
    let mut filled = first;
    while filled < len {
        let Some((done, todo)) = target.split_at_mut_checked(filled) else {
            break;
        };
        for (to, from) in todo.iter_mut().zip(done.iter()) {
            *to = *from;
        }
        filled = filled.saturating_mul(2).min(len);
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
        let out = decompress(&compressed).unwrap();
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
        let out = decompress(&compressed).unwrap();
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
        let out = decompress(&compressed).unwrap();
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
        let out = decompress(&compressed).unwrap();
        assert_eq!(out, b"01234567890123");
    }

    #[test]
    fn snappy_rejects_copy_offset_beyond_start() {
        let mut compressed = vec![4u8];
        compressed.push(0x01u8); // copy, len=4, offset=0
        compressed.push(0);
        let err = decompress(&compressed).unwrap_err();
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
        let out = decompress(&compressed).unwrap();
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
        let out = decompress(&compressed).unwrap();
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
        let out = decompress(&compressed).unwrap();
        assert_eq!(out, literal);
    }

    #[test]
    fn an_over_long_varint_is_invalid() {
        // Five continuation bytes with no terminator: `read_varint`'s
        // `shift >= 35` guard trips before a sixth byte is even read.
        let compressed = vec![0x80, 0x80, 0x80, 0x80, 0x80];
        let err = decompress(&compressed).unwrap_err();
        assert!(matches!(err, SnappyError::InvalidVarint));
    }

    #[test]
    fn a_truncated_varint_is_unexpected_eof() {
        // A single continuation byte with nothing after it.
        let compressed = vec![0x80];
        let err = decompress(&compressed).unwrap_err();
        assert!(matches!(err, SnappyError::UnexpectedEof));
    }

    #[test]
    fn a_short_decode_is_a_size_mismatch() {
        // Declares length 5 but the block only encodes a 4-byte literal.
        let mut compressed = vec![5u8];
        compressed.push(3u8 << 2);
        compressed.extend_from_slice(b"abcd");
        let err = decompress(&compressed).unwrap_err();
        assert!(matches!(
            err,
            SnappyError::SizeMismatch {
                expected: 5,
                actual: 4
            }
        ));
    }

    /// #657: a 6-byte block claiming ~4 GB. Before, the output buffer was
    /// allocated at the claimed size (or the page header's, whichever was
    /// larger) before a single op was read.
    #[test]
    fn a_length_no_block_this_short_can_reach_is_rejected_before_allocating() {
        let compressed = vec![0xff, 0xff, 0xff, 0xff, 0x0f, 0x00];
        let err = decompress(&compressed).unwrap_err();
        assert!(matches!(err, SnappyError::InvalidLength(0xffff_ffff)));
    }

    /// The bound is the most any op can expand to (a 3-byte copy writing
    /// 64 bytes), so a block of maximal copies still decodes.
    #[test]
    fn a_block_of_maximal_copies_is_within_the_expansion_bound() {
        let copies = 100;
        let declared = 1 + 64 * copies;
        let mut compressed = Vec::new();
        let mut v = declared;
        while v >= 0x80 {
            compressed.push((v as u8) | 0x80);
            v >>= 7;
        }
        compressed.push(v as u8);
        compressed.extend_from_slice(&[0, b'z']);
        for _ in 0..copies {
            compressed.extend_from_slice(&[(63 << 2) | 0x02, 1, 0]);
        }
        let out = decompress(&compressed).unwrap();
        assert_eq!(out, vec![b'z'; declared]);
    }

    /// Literal lengths in 1-4 trailing bytes, cut short.
    #[test]
    fn a_literal_length_cut_short_is_unexpected_eof() {
        for tag in [60u8, 61, 62, 63] {
            let compressed = vec![10, tag << 2];
            assert!(matches!(
                decompress(&compressed).unwrap_err(),
                SnappyError::UnexpectedEof
            ));
        }
    }

    /// A 4-byte-length literal decodes like a short one.
    #[test]
    fn a_literal_with_a_four_byte_length_decodes() {
        let compressed = vec![3, 63 << 2, 2, 0, 0, 0, b'a', b'b', b'c'];
        assert_eq!(decompress(&compressed).unwrap(), b"abc");
    }

    /// Copy ops whose operands are cut short.
    #[test]
    fn copy_operands_cut_short_are_unexpected_eof() {
        // Each copy kind with one operand byte fewer than it needs.
        for compressed in [
            vec![10, 0, b'a', 0x01],
            vec![10, 0, b'a', 0x02, 1],
            vec![10, 0, b'a', 0x03, 1, 0, 0],
        ] {
            assert!(matches!(
                decompress(&compressed).unwrap_err(),
                SnappyError::UnexpectedEof
            ));
        }
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

    // storage_parquet_compression_snappy_decompress_5a62d17b: `len <= WIDE && copy_wide(&mut out, dst, rest)`
    // (`copy_wide` is false when fewer than WIDE input bytes remain or the
    // output slack is used up)
    #[test]
    fn mcdc__storage_parquet_compression_snappy_decompress_5a62d17b__v1_short_literal_with_input_and_output_slack_takes_the_wide_path(
    ) {
        // "abc" is followed by a 20-byte literal, so 16 input bytes exist
        // past it and the output has its WIDE slack: the fixed-width copy
        // must still yield exactly the 3 literal bytes.
        let tail: Vec<u8> = (100..120u8).collect();
        let data = block(23, &[&literal(b"abc"), &literal(&tail)]);
        let out = decompress(&data).unwrap();
        assert_eq!(&out[..3], b"abc");
        assert_eq!(&out[3..], &tail[..]);
    }

    #[test]
    fn mcdc__storage_parquet_compression_snappy_decompress_5a62d17b__v2_literal_longer_than_wide_takes_the_exact_path(
    ) {
        let bytes: Vec<u8> = (0..(WIDE as u8 + 4)).collect();
        let data = block(WIDE as u8 + 4, &[&literal(&bytes)]);
        assert_eq!(decompress(&data).unwrap(), bytes);
    }

    #[test]
    fn mcdc__storage_parquet_compression_snappy_decompress_5a62d17b__v3_short_literal_at_the_end_of_input_takes_the_exact_path(
    ) {
        // Fewer than WIDE input bytes remain after the tag: the wide load
        // would run off `data`, so the exact copy is used.
        let data = block(5, &[&literal(b"hello")]);
        assert_eq!(decompress(&data).unwrap(), b"hello");
    }

    #[test]
    fn mcdc__storage_parquet_compression_snappy_decompress_5a62d17b__v4_short_literal_past_the_output_slack_takes_the_exact_path_and_the_block_is_rejected(
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
        let err = decompress(&data).unwrap_err();
        assert!(matches!(
            err,
            SnappyError::SizeMismatch { expected: 18, .. }
        ));
    }

    // storage_parquet_compression_snappy_decompress_d541d5eb: `offset == 0 || offset > dst`
    #[test]
    fn mcdc__storage_parquet_compression_snappy_decompress_d541d5eb__v1_offset_zero() {
        let data = block(6, &[&literal(b"ab"), &copy2(4, 0)]);
        assert!(matches!(
            decompress(&data).unwrap_err(),
            SnappyError::InvalidCopyOffset
        ));
    }

    #[test]
    fn mcdc__storage_parquet_compression_snappy_decompress_d541d5eb__v2_offset_before_the_start() {
        let data = block(6, &[&literal(b"ab"), &copy2(4, 3)]);
        assert!(matches!(
            decompress(&data).unwrap_err(),
            SnappyError::InvalidCopyOffset
        ));
    }

    #[test]
    fn mcdc__storage_parquet_compression_snappy_decompress_d541d5eb__v3_offset_within_the_output() {
        let data = block(4, &[&literal(b"ab"), &copy2(2, 2)]);
        assert_eq!(decompress(&data).unwrap(), b"abab");
    }

    // storage_parquet_compression_snappy_decompress_0bc0cf35: `len <= WIDE && offset >= len && copy_wide_within(&mut out, src, dst)`
    #[test]
    fn mcdc__storage_parquet_compression_snappy_decompress_0bc0cf35__v1_short_non_overlapping_copy_with_slack_takes_the_wide_path(
    ) {
        let data = block(8, &[&literal(b"abcd"), &copy2(4, 4)]);
        assert_eq!(decompress(&data).unwrap(), b"abcdabcd");
    }

    #[test]
    fn mcdc__storage_parquet_compression_snappy_decompress_0bc0cf35__v2_copy_longer_than_wide_takes_copy_within(
    ) {
        let bytes: Vec<u8> = (0..20u8).collect();
        let data = block(40, &[&literal(&bytes), &copy2(20, 20)]);
        let out = decompress(&data).unwrap();
        assert_eq!(&out[..20], &bytes[..]);
        assert_eq!(&out[20..], &bytes[..]);
    }

    #[test]
    fn mcdc__storage_parquet_compression_snappy_decompress_0bc0cf35__v3_overlapping_copy_takes_the_pattern_path(
    ) {
        let data = block(9, &[&literal(b"xyz"), &copy2(6, 3)]);
        assert_eq!(decompress(&data).unwrap(), b"xyzxyzxyz");
    }

    #[test]
    fn mcdc__storage_parquet_compression_snappy_decompress_0bc0cf35__v4_short_copy_past_the_output_slack_takes_the_exact_path_and_the_block_is_rejected(
    ) {
        // 8 declared (out = 24): an 8-byte literal, a 4-byte copy at dst 8
        // (8 + 16 <= 24, wide), then a 4-byte copy at dst 12 (12 + 16 >
        // 24, exact). Both copies are correct; the block is over-long.
        let data = block(8, &[&literal(b"01234567"), &copy2(4, 4), &copy2(4, 4)]);
        let err = decompress(&data).unwrap_err();
        assert!(matches!(
            err,
            SnappyError::SizeMismatch {
                expected: 8,
                actual: 16
            }
        ));
    }

    // storage_parquet_compression_snappy_copy_from_offset_d541d5eb: `offset == 0 || offset > dst`
    #[test]
    fn mcdc__storage_parquet_compression_snappy_copy_from_offset_d541d5eb__v1_offset_zero() {
        let mut out = vec![b'a', b'b', 0];
        assert!(copy_from_offset(&mut out, 2, 0, 1).is_err());
    }

    #[test]
    fn mcdc__storage_parquet_compression_snappy_copy_from_offset_d541d5eb__v2_offset_beyond_out_len(
    ) {
        let mut out = vec![b'a', b'b', 0];
        assert!(copy_from_offset(&mut out, 2, 3, 1).is_err());
    }

    #[test]
    fn mcdc__storage_parquet_compression_snappy_copy_from_offset_d541d5eb__v3_offset_within_range_succeeds(
    ) {
        let mut out = vec![b'a', b'b', 0];
        assert!(copy_from_offset(&mut out, 2, 2, 1).is_ok());
        assert_eq!(out, vec![b'a', b'b', b'a']);
    }
}
