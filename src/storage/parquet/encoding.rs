//! Shared low-level encodings used inside Parquet data pages: the
//! RLE/Bit-Packed Hybrid format used for definition (and repetition)
//! levels.

use std::fmt;

/// A malformed RLE/Bit-Packed Hybrid or DELTA_BINARY_PACKED stream. Page
/// data is untrusted, so every inconsistency is an error, never a panic.
#[derive(Debug)]
pub enum EncodingError {
    /// The stream ends inside a varint, a run, or a miniblock.
    UnexpectedEof,
    /// A varint longer than the ten bytes a `u64` needs.
    InvalidVarint,
    /// A block size or count varint does not fit `usize` or exceeds its
    /// sanity limit, or a run's byte size overflows `usize`.
    InvalidLength(u64),
    /// A bit width wider than the values it packs: 32 bits for levels and
    /// dictionary indices, 64 for DELTA_BINARY_PACKED deltas.
    InvalidBitWidth(u32),
}

impl fmt::Display for EncodingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodingError::UnexpectedEof => write!(f, "unexpected end of input"),
            EncodingError::InvalidVarint => write!(f, "varint too long"),
            EncodingError::InvalidLength(n) => write!(f, "length out of range: {n}"),
            EncodingError::InvalidBitWidth(w) => write!(f, "bit width out of range: {w}"),
        }
    }
}

impl std::error::Error for EncodingError {}

/// Result alias for the page-level decoders.
pub type Result<T> = std::result::Result<T, EncodingError>;

/// The most values reserved up front from a caller- or header-supplied
/// count alone. RLE runs and zero-width miniblocks legitimately expand a
/// few bytes into any number of values, so the count cannot be checked
/// against the input size; past this the output grows as values are
/// actually decoded, instead of a forged count claiming gigabytes at once.
const MAX_PREALLOC_VALUES: usize = 1 << 20;

/// The largest DELTA_BINARY_PACKED block accepted. The spec sets no
/// maximum, but writers use 128 (parquet-mr, Arrow) to a few thousand
/// values; a forged block size is what lets zero-width miniblocks turn a
/// few bytes into an arbitrary number of values, so it is capped.
const MAX_DELTA_BLOCK_SIZE: u64 = 1 << 16;

/// The most values a DELTA_BINARY_PACKED page can hold: a page's value
/// count is an `i32` in its header.
const MAX_DELTA_VALUES: u64 = i32::MAX as u64;

/// A varint used as a length/count that must not exceed `max`.
fn bounded(v: u64, max: u64) -> Result<usize> {
    if v > max {
        return Err(EncodingError::InvalidLength(v));
    }
    length(v)
}

/// A varint used as a length/count: must fit `usize`.
fn length(v: u64) -> Result<usize> {
    usize::try_from(v).map_err(|_| EncodingError::InvalidLength(v))
}

/// Zigzag decoding without a reinterpreting cast: the magnitude half always
/// fits (`raw >> 1 < 2^63`), the sign is `-1` or `0` (negating 0 or 1
/// cannot wrap).
fn zigzag_decode(raw: u64) -> i64 {
    let half = i64::try_from(raw >> 1).unwrap_or(i64::MAX);
    half ^ i64::from(raw & 1 == 1).wrapping_neg()
}

/// The two's-complement reinterpretation the DELTA_BINARY_PACKED encoding
/// defines for a 64-bit-wide delta (#289): the one place it is spelled out.
#[allow(
    clippy::cast_possible_wrap,
    reason = "bit-for-bit reinterpretation the encoding specifies; not a range conversion"
)]
fn u64_bits_as_i64(raw: u64) -> i64 {
    raw as i64
}

/// Read an unsigned LEB128 varint from the front of `rest`, advancing it
/// past the varint.
fn read_unsigned_varint(rest: &mut &[u8]) -> Result<u64> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        if shift >= 70 {
            return Err(EncodingError::InvalidVarint);
        }
        let (&b, tail) = rest.split_first().ok_or(EncodingError::UnexpectedEof)?;
        *rest = tail;
        result |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Ok(result);
        }
        shift = shift.saturating_add(7);
    }
}

/// Unpack consecutive `bit_width`-bit values (1..=64), LSB-first, from
/// `bytes`, passing each whole value to `emit`; trailing bits too few for
/// a whole value are dropped. A byte at a time is shifted into an
/// accumulator, so there is no per-bit work and no indexing.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "`bits` stays in 0..72: it grows by 8 only while below `bit_width` <= 64, and shrinks by `bit_width` only when at least that"
)]
#[allow(
    clippy::cast_possible_truncation,
    reason = "the value is masked to `bit_width` <= 64 bits before narrowing to u64"
)]
#[inline(always)]
fn unpack_bits(bytes: &[u8], bit_width: u32, mut emit: impl FnMut(u64)) {
    if !(1..=64).contains(&bit_width) {
        return;
    }
    let mask = u64::MAX >> (64 - bit_width);
    if bit_width <= 56 {
        // A u64 accumulator holds up to 55 pending bits plus one byte.
        let mut acc: u64 = 0;
        let mut bits: u32 = 0;
        for &b in bytes {
            acc |= u64::from(b) << bits;
            bits += 8;
            while bits >= bit_width {
                emit(acc & mask);
                acc >>= bit_width;
                bits -= bit_width;
            }
        }
    } else {
        let mut acc: u128 = 0;
        let mut bits: u32 = 0;
        for &b in bytes {
            acc |= u128::from(b) << bits;
            bits += 8;
            while bits >= bit_width {
                emit(acc as u64 & mask);
                acc >>= bit_width;
                bits -= bit_width;
            }
        }
    }
}

/// The 8 values of one bit-packed group of `bit_width` (1..=8) bits each,
/// held LSB-first in `g`.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "`i * bit_width` is at most 7 * 8"
)]
#[allow(
    clippy::cast_possible_truncation,
    reason = "`i < 8`, and each value is masked to `bit_width` <= 8 bits"
)]
#[inline(always)]
fn group_values(g: u64, bit_width: u32) -> [u32; 8] {
    let mask = !(u64::MAX << bit_width);
    std::array::from_fn(|i| ((g >> (i as u32 * bit_width)) & mask) as u32)
}

/// Decode `num_values` levels from the RLE/Bit-Packed Hybrid format used for
/// Parquet definition and repetition levels (and dictionary indices).
/// `bit_width` is the number of bits needed to represent the largest
/// possible level value, at most 32.
///
/// Spec: https://parquet.apache.org/docs/file-format/data-pages/encodings/#rle
pub fn decode_hybrid_rle_bitpacked(
    data: &[u8],
    bit_width: u32,
    num_values: usize,
) -> Result<Vec<u32>> {
    if bit_width > 32 {
        return Err(EncodingError::InvalidBitWidth(bit_width));
    }
    let mut out = Vec::with_capacity(num_values.min(MAX_PREALLOC_VALUES));
    if bit_width == 0 {
        out.resize(num_values, 0);
        return Ok(out);
    }
    let width = bit_width as usize;

    let mut rest = data;
    while out.len() < num_values {
        let header = read_unsigned_varint(&mut rest)?;
        let run_len = length(header >> 1)?;
        let remaining = num_values.saturating_sub(out.len());

        if header & 1 == 1 {
            // `run_len` groups of 8 values, each group packed into
            // `bit_width` bytes (LSB-first bit order). The whole run must
            // be present, but only the bytes holding values still wanted
            // are unpacked.
            let byte_count = run_len
                .checked_mul(width)
                .ok_or(EncodingError::InvalidLength(header >> 1))?;
            let (bytes, tail) = rest
                .split_at_checked(byte_count)
                .ok_or(EncodingError::UnexpectedEof)?;
            rest = tail;
            let wanted = remaining.saturating_mul(width).div_ceil(8);
            let bytes = bytes.get(..wanted).unwrap_or(bytes);
            let bytes = if width <= 8 {
                // Whole groups fit a u64: 8 values per load, no per-value
                // bookkeeping (levels and dictionaries up to 256 entries).
                let mut groups = bytes.chunks_exact(width);
                for group in &mut groups {
                    let g = group
                        .iter()
                        .rev()
                        .fold(0u64, |acc, &b| (acc << 8) | u64::from(b));
                    out.extend_from_slice(&group_values(g, bit_width));
                }
                groups.remainder()
            } else {
                bytes
            };
            // Masked to `bit_width <= 32` bits, so the conversion holds.
            unpack_bits(bytes, bit_width, |v| {
                out.push(u32::try_from(v).unwrap_or(u32::MAX));
            });
        } else {
            // RLE run: `run_len` repetitions of a single value packed into
            // ceil(bit_width / 8) bytes (at most 4), little-endian.
            let (bytes, tail) = rest
                .split_at_checked(bit_width.div_ceil(8) as usize)
                .ok_or(EncodingError::UnexpectedEof)?;
            rest = tail;
            let value = bytes
                .iter()
                .rev()
                .fold(0u32, |acc, &b| (acc << 8) | u32::from(b));
            out.extend(std::iter::repeat_n(value, run_len.min(remaining)));
        }
    }

    out.truncate(num_values);
    Ok(out)
}

fn read_zigzag_varint(rest: &mut &[u8]) -> Result<i64> {
    let raw = read_unsigned_varint(rest)?;
    Ok(zigzag_decode(raw))
}

/// Decode a DELTA_BINARY_PACKED-encoded stream of `i64` values (used for
/// INT32/INT64 data pages).
///
/// Layout: a header (`block_size`, `miniblocks_per_block`, `total_value_count`,
/// zigzag `first_value`), followed by blocks of `block_size` values each; a
/// block starts with a zigzag `min_delta` and one bit-width byte per
/// miniblock, then the miniblocks themselves (each `block_size /
/// miniblocks_per_block` plain bit-packed values, LSB-first with no group
/// header -- the packed value plus `min_delta` is the delta from the
/// previous value).
///
/// Spec: https://parquet.apache.org/docs/file-format/data-pages/encodings/#delta-encoding-delta_binary_packed--5
pub fn decode_delta_binary_packed(data: &[u8]) -> Result<Vec<i64>> {
    let mut rest = data;
    let block_size = bounded(read_unsigned_varint(&mut rest)?, MAX_DELTA_BLOCK_SIZE)?;
    let miniblocks_per_block = length(read_unsigned_varint(&mut rest)?)?;
    let total_count = bounded(read_unsigned_varint(&mut rest)?, MAX_DELTA_VALUES)?;
    let first_value = read_zigzag_varint(&mut rest)?;

    let mut out = Vec::with_capacity(total_count.min(MAX_PREALLOC_VALUES));
    if total_count == 0 {
        return Ok(out);
    }
    out.push(first_value);
    let mut prev = first_value;
    let values_per_miniblock = block_size.checked_div(miniblocks_per_block).unwrap_or(0);
    let mut packed_values = Vec::new();

    while out.len() < total_count {
        let min_delta = read_zigzag_varint(&mut rest)?;
        let (bit_widths, tail) = rest
            .split_at_checked(miniblocks_per_block)
            .ok_or(EncodingError::UnexpectedEof)?;
        rest = tail;
        for &bit_width in bit_widths {
            if out.len() >= total_count {
                break;
            }
            let bit_width = u32::from(bit_width);
            if bit_width > 64 {
                return Err(EncodingError::InvalidBitWidth(bit_width));
            }
            let byte_count = values_per_miniblock
                .checked_mul(bit_width as usize)
                .ok_or(EncodingError::InvalidLength(u64::from(bit_width)))?
                .div_ceil(8);
            let (packed, tail) = rest
                .split_at_checked(byte_count)
                .ok_or(EncodingError::UnexpectedEof)?;
            rest = tail;
            let wanted = values_per_miniblock.min(total_count.saturating_sub(out.len()));
            // Deltas are unsigned offsets from `min_delta`, summed with the
            // two's-complement wrap the encoding defines for 64-bit widths.
            if bit_width == 0 {
                for _ in 0..wanted {
                    prev = prev.wrapping_add(min_delta);
                    out.push(prev);
                }
                continue;
            }
            packed_values.clear();
            unpack_bits(packed, bit_width, |v| packed_values.push(v));
            for &raw in packed_values.iter().take(wanted) {
                prev = prev
                    .wrapping_add(min_delta)
                    .wrapping_add(u64_bits_as_i64(raw));
                out.push(prev);
            }
        }
    }
    Ok(out)
}

/// Number of bits needed to represent values in `0..=max_level`.
pub fn bit_width_for_max_level(max_level: u32) -> u32 {
    // `leading_zeros()` is at most 32: cannot saturate.
    u32::BITS.saturating_sub(max_level.leading_zeros())
}

/// Map decoded definition levels to a presence mask: `true` where the value
/// is present, `false` where it is null. A level equal to `max_def_level`
/// means the value (and every optional ancestor in a nested schema) is
/// present; anything lower means null was recorded at or above that level.
///
/// For a flat (non-nested) optional column, `max_def_level` is 1: level 0 =
/// null, level 1 = present. Nested schemas can have `max_def_level > 1`;
/// this still treats "reached the max" as the only "present" case, which is
/// correct but does not yet distinguish which ancestor was null.
pub fn null_mask(levels: &[u32], max_def_level: u32) -> Vec<bool> {
    levels.iter().map(|&level| level == max_def_level).collect()
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

    #[test]
    fn decodes_pure_rle_run() {
        // bit_width=1, run of 5 values = 1
        let mut buf = Vec::new();
        write_varint(&mut buf, 5u64 << 1); // RLE, run_len=5
        buf.push(0x01); // value byte: 1
        let levels = decode_hybrid_rle_bitpacked(&buf, 1, 5).unwrap();
        assert_eq!(levels, vec![1, 1, 1, 1, 1]);
    }

    #[test]
    fn decodes_pure_bit_packed_run() {
        // bit_width=1, 1 group of 8 values, pattern 1,0,1,0,1,0,1,0 (LSB first)
        let mut buf = Vec::new();
        write_varint(&mut buf, (1u64 << 1) | 1); // bit-packed, 1 group
        buf.push(0b0101_0101);
        let levels = decode_hybrid_rle_bitpacked(&buf, 1, 8).unwrap();
        assert_eq!(levels, vec![1, 0, 1, 0, 1, 0, 1, 0]);
    }

    #[test]
    fn decodes_mixed_runs() {
        let mut buf = Vec::new();
        write_varint(&mut buf, 3u64 << 1); // RLE run of 3
        buf.push(0x00);
        write_varint(&mut buf, (1u64 << 1) | 1); // bit-packed 1 group (8 values)
        buf.push(0b1111_0000);
        let levels = decode_hybrid_rle_bitpacked(&buf, 1, 11).unwrap();
        assert_eq!(levels, vec![0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1]);
    }

    #[test]
    fn zero_bit_width_yields_all_zero_levels() {
        let levels = decode_hybrid_rle_bitpacked(&[], 0, 4).unwrap();
        assert_eq!(levels, vec![0, 0, 0, 0]);
    }

    #[test]
    fn truncated_input_errors_not_panics() {
        let mut buf = Vec::new();
        write_varint(&mut buf, 5u64 << 1); // claims 5 values but no value byte follows
        let result = decode_hybrid_rle_bitpacked(&buf, 1, 5);
        assert!(result.is_err());
    }

    #[test]
    fn bit_width_helper() {
        assert_eq!(bit_width_for_max_level(0), 0);
        assert_eq!(bit_width_for_max_level(1), 1);
        assert_eq!(bit_width_for_max_level(2), 2);
        assert_eq!(bit_width_for_max_level(3), 2);
        assert_eq!(bit_width_for_max_level(4), 3);
    }

    #[test]
    fn null_mask_flat_schema_mixed() {
        let levels = [1, 0, 1, 1, 0];
        let mask = null_mask(&levels, 1);
        assert_eq!(mask, vec![true, false, true, true, false]);
    }

    #[test]
    fn null_mask_all_present() {
        let levels = [1, 1, 1];
        let mask = null_mask(&levels, 1);
        assert_eq!(mask, vec![true, true, true]);
    }

    #[test]
    fn null_mask_all_null() {
        let levels = [0, 0, 0];
        let mask = null_mask(&levels, 1);
        assert_eq!(mask, vec![false, false, false]);
    }

    #[test]
    fn null_mask_nested_max_level_above_one() {
        // max_def_level=2: only level 2 counts as present.
        let levels = [2, 1, 0, 2];
        let mask = null_mask(&levels, 2);
        assert_eq!(mask, vec![true, false, false, true]);
    }

    fn write_zigzag(out: &mut Vec<u8>, v: i64) {
        write_varint(out, ((v << 1) ^ (v >> 63)) as u64);
    }

    fn pack_bits(out: &mut Vec<u8>, values: &[u64], bit_width: u32) {
        if bit_width == 0 {
            return;
        }
        let mut bytes = vec![0u8; (values.len() * bit_width as usize).div_ceil(8)];
        let mut bit_pos = 0usize;
        for &value in values {
            for bit in 0..bit_width as usize {
                if (value >> bit) & 1 == 1 {
                    let global_bit = bit_pos + bit;
                    bytes[global_bit / 8] |= 1 << (global_bit % 8);
                }
            }
            bit_pos += bit_width as usize;
        }
        out.extend_from_slice(&bytes);
    }

    #[test]
    fn decodes_delta_binary_packed_single_partial_block() {
        // header: block_size=8, miniblocks_per_block=2 (4 values each),
        // total_value_count=5, first_value=100.
        let mut buf = Vec::new();
        write_varint(&mut buf, 8);
        write_varint(&mut buf, 2);
        write_varint(&mut buf, 5);
        write_zigzag(&mut buf, 100);

        // Deltas for the remaining 4 values: 5, -2, 7, 10; min_delta=-2, so
        // raw (delta - min_delta) values are 7, 0, 9, 12 (needs 4 bits).
        write_zigzag(&mut buf, -2);
        buf.push(4); // miniblock 1 bit width
        buf.push(0); // miniblock 2 bit width (unused padding, decoder stops before reading it)
        pack_bits(&mut buf, &[7, 0, 9, 12], 4);

        let values = decode_delta_binary_packed(&buf).unwrap();
        assert_eq!(values, vec![100, 105, 103, 110, 120]);
    }

    #[test]
    fn decodes_delta_binary_packed_empty() {
        let mut buf = Vec::new();
        write_varint(&mut buf, 8);
        write_varint(&mut buf, 2);
        write_varint(&mut buf, 0);
        write_zigzag(&mut buf, 0);
        let values = decode_delta_binary_packed(&buf).unwrap();
        assert!(values.is_empty());
    }

    #[test]
    fn decodes_delta_binary_packed_single_value() {
        let mut buf = Vec::new();
        write_varint(&mut buf, 8);
        write_varint(&mut buf, 2);
        write_varint(&mut buf, 1);
        write_zigzag(&mut buf, 42);
        let values = decode_delta_binary_packed(&buf).unwrap();
        assert_eq!(values, vec![42]);
    }
}

#[cfg(test)]
mod tests_289 {
    use super::*;

    /// #289: zigzag and the 64-bit delta reinterpretation, at the corners.
    #[test]
    fn zigzag_decode_corners_and_wide_delta_bits() {
        assert_eq!(zigzag_decode(0), 0);
        assert_eq!(zigzag_decode(1), -1);
        assert_eq!(zigzag_decode(2), 1);
        assert_eq!(zigzag_decode(u64::MAX), i64::MIN);
        assert_eq!(zigzag_decode(u64::MAX - 1), i64::MAX);
        assert_eq!(u64_bits_as_i64(u64::MAX), -1);
        assert_eq!(u64_bits_as_i64(1 << 63), i64::MIN);
        assert_eq!(length(7).unwrap(), 7);
        // `length` can only fail where `usize` is narrower than `u64`.
        #[cfg(target_pointer_width = "32")]
        assert!(matches!(
            length(u64::MAX),
            Err(EncodingError::InvalidLength(u64::MAX))
        ));
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    reason = "test code: hand-built varints"
)]
mod malformed_input {
    //! #657: inputs that panicked or allocated without bound before the
    //! lint burn-down. Each now fails with an error.

    use super::*;

    /// A bit-packed run whose `run_len * bit_width` wraps `usize`: debug
    /// builds overflowed the multiplication, release builds wrapped it to
    /// a 2-byte run and then indexed past it (`run_len * 8` values of 3
    /// bits each).
    #[test]
    fn a_bit_packed_run_whose_byte_size_overflows_is_invalid_length() {
        let run_len: u64 = u64::MAX / 3 + 1;
        let mut data = Vec::new();
        let mut header = (run_len << 1) | 1;
        while header >= 0x80 {
            data.push((header as u8) | 0x80);
            header >>= 7;
        }
        data.push(header as u8);
        data.extend_from_slice(&[0xff, 0xff]);
        assert!(matches!(
            decode_hybrid_rle_bitpacked(&data, 3, 100),
            Err(EncodingError::InvalidLength(n)) if n == run_len
        ));
    }

    /// A dictionary page's bit-width byte is untrusted: past 32 the value
    /// bytes shifted a `u32` by 32 or more (a debug-build panic).
    #[test]
    fn a_bit_width_over_32_is_invalid_bit_width() {
        let rle_run = [0x80, 0x20, 1, 0xff, 0xff, 0xff, 0xff, 0x0f];
        assert!(matches!(
            decode_hybrid_rle_bitpacked(&rle_run, 33, 64),
            Err(EncodingError::InvalidBitWidth(33))
        ));
        assert_eq!(
            EncodingError::InvalidBitWidth(33).to_string(),
            "bit width out of range: 33"
        );
    }

    /// The widest legal width still decodes, both as a run and packed.
    #[test]
    fn a_32_bit_width_decodes_runs_and_packed_groups() {
        let mut data = vec![2 << 1, 0x78, 0x56, 0x34, 0x12, (1 << 1) | 1];
        let packed: Vec<u32> = (0..8).map(|i| u32::MAX - i).collect();
        for v in &packed {
            data.extend_from_slice(&v.to_le_bytes());
        }
        let mut expected = vec![0x1234_5678, 0x1234_5678];
        expected.extend_from_slice(&packed);
        assert_eq!(
            decode_hybrid_rle_bitpacked(&data, 32, 10).unwrap(),
            expected
        );
    }

    /// A forged `num_values` only reserves a bounded buffer up front; a
    /// stream that runs out errors instead of filling it.
    #[test]
    fn a_forged_value_count_fails_on_the_missing_data() {
        assert!(matches!(
            decode_hybrid_rle_bitpacked(&[2 << 1, 1], 1, usize::MAX),
            Err(EncodingError::UnexpectedEof)
        ));
    }

    fn varints(values: &[u64]) -> Vec<u8> {
        let mut out = Vec::new();
        for &v in values {
            let mut v = v;
            while v >= 0x80 {
                out.push((v as u8) | 0x80);
                v >>= 7;
            }
            out.push(v as u8);
        }
        out
    }

    /// A total count past `i32::MAX`: before #657 `Vec::with_capacity`
    /// of it panicked with "capacity overflow" (or aborted on the
    /// allocation for counts that fit).
    #[test]
    fn a_delta_total_count_past_a_page_count_is_invalid_length() {
        let data = varints(&[128, 4, 1 << 62, 0]);
        assert!(matches!(
            decode_delta_binary_packed(&data),
            Err(EncodingError::InvalidLength(n)) if n == 1 << 62
        ));
    }

    /// A huge miniblock count: the bit widths were collected into a
    /// `Vec::with_capacity(miniblocks_per_block)` before any were read.
    #[test]
    fn a_delta_miniblock_count_past_the_input_is_unexpected_eof() {
        let data = varints(&[128, 1 << 40, 10, 0, 0]);
        assert!(matches!(
            decode_delta_binary_packed(&data),
            Err(EncodingError::UnexpectedEof)
        ));
    }

    /// A huge block of zero-width miniblocks: before #657 a single
    /// miniblock allocated `vec![0; block_size]` (8 TB here).
    #[test]
    fn a_delta_block_size_past_the_cap_is_invalid_length() {
        let mut data = varints(&[1 << 40, 1, 1 << 30, 0, 0]);
        data.push(0);
        assert!(matches!(
            decode_delta_binary_packed(&data),
            Err(EncodingError::InvalidLength(n)) if n == 1 << 40
        ));
    }

    /// A miniblock bit width past 64 shifted a `u64` out of range.
    #[test]
    fn a_delta_bit_width_over_64_is_invalid_bit_width() {
        let mut data = varints(&[128, 4, 10, 0, 0]);
        data.extend_from_slice(&[65, 0, 0, 0]);
        data.extend_from_slice(&[0; 300]);
        assert!(matches!(
            decode_delta_binary_packed(&data),
            Err(EncodingError::InvalidBitWidth(65))
        ));
    }

    /// 64-bit-wide miniblocks take the 128-bit accumulator path.
    #[test]
    fn a_delta_64_bit_miniblock_decodes() {
        // block 32, one miniblock; first value 0, min delta 0, width 64,
        // then 32 deltas of u64::MAX (two's-complement -1 each).
        let mut data = varints(&[32, 1, 3, 0, 0]);
        data.push(64);
        for _ in 0..32 {
            data.extend_from_slice(&u64::MAX.to_le_bytes());
        }
        assert_eq!(decode_delta_binary_packed(&data).unwrap(), vec![0, -1, -2]);
    }
}
