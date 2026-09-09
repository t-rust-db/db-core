//! Shared low-level encodings used inside Parquet data pages: the
//! RLE/Bit-Packed Hybrid format used for definition (and repetition)
//! levels.

use std::fmt;

#[derive(Debug)]
pub enum EncodingError {
    UnexpectedEof,
    InvalidVarint,
}

impl fmt::Display for EncodingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodingError::UnexpectedEof => write!(f, "unexpected end of input"),
            EncodingError::InvalidVarint => write!(f, "varint too long"),
        }
    }
}

impl std::error::Error for EncodingError {}

pub type Result<T> = std::result::Result<T, EncodingError>;

fn read_unsigned_varint(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        if shift >= 70 {
            return Err(EncodingError::InvalidVarint);
        }
        let b = *buf.get(*pos).ok_or(EncodingError::UnexpectedEof)?;
        *pos += 1;
        result |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    Ok(result)
}

/// Decode `num_values` levels from the RLE/Bit-Packed Hybrid format used for
/// Parquet definition and repetition levels (and dictionary indices).
/// `bit_width` is the number of bits needed to represent the largest
/// possible level value.
///
/// Spec: https://parquet.apache.org/docs/file-format/data-pages/encodings/#rle
pub fn decode_hybrid_rle_bitpacked(
    data: &[u8],
    bit_width: u32,
    num_values: usize,
) -> Result<Vec<u32>> {
    let mut out = Vec::with_capacity(num_values);
    if bit_width == 0 {
        out.resize(num_values, 0);
        return Ok(out);
    }

    let mut pos = 0usize;
    while out.len() < num_values {
        let header = read_unsigned_varint(data, &mut pos)?;
        let is_bit_packed = header & 1 == 1;
        let run_len = (header >> 1) as usize;

        if is_bit_packed {
            // `run_len` groups of 8 values, each group packed into
            // `bit_width` bytes (LSB-first bit order).
            let num_groups = run_len;
            let byte_count = num_groups * bit_width as usize;
            let bytes = data
                .get(pos..pos + byte_count)
                .ok_or(EncodingError::UnexpectedEof)?;
            pos += byte_count;

            let mut bit_pos = 0usize;
            for _ in 0..num_groups * 8 {
                if out.len() >= num_values {
                    break;
                }
                let mut value: u32 = 0;
                for bit in 0..bit_width as usize {
                    let global_bit = bit_pos + bit;
                    let byte = bytes[global_bit / 8];
                    let b = (byte >> (global_bit % 8)) & 1;
                    value |= (b as u32) << bit;
                }
                out.push(value);
                bit_pos += bit_width as usize;
            }
        } else {
            // RLE run: `run_len` repetitions of a single value packed into
            // ceil(bit_width / 8) bytes, little-endian.
            let value_byte_count = bit_width.div_ceil(8) as usize;
            let bytes = data
                .get(pos..pos + value_byte_count)
                .ok_or(EncodingError::UnexpectedEof)?;
            pos += value_byte_count;
            let mut value: u32 = 0;
            for (i, b) in bytes.iter().enumerate() {
                value |= (*b as u32) << (8 * i);
            }
            let take = run_len.min(num_values - out.len());
            out.resize(out.len() + take, value);
        }
    }

    out.truncate(num_values);
    Ok(out)
}

fn read_zigzag_varint(buf: &[u8], pos: &mut usize) -> Result<i64> {
    let raw = read_unsigned_varint(buf, pos)?;
    Ok(((raw >> 1) as i64) ^ -((raw & 1) as i64))
}

/// Unpack `count` `bit_width`-bit unsigned values, packed LSB-first with no
/// group header (the DELTA_BINARY_PACKED miniblock layout — plain
/// bit-packing, unlike the RLE/Bit-Packed Hybrid format above).
fn unpack_bit_packed(data: &[u8], bit_width: u32, count: usize) -> Result<Vec<u64>> {
    if bit_width == 0 {
        return Ok(vec![0; count]);
    }
    let byte_count = (count * bit_width as usize).div_ceil(8);
    let bytes = data.get(..byte_count).ok_or(EncodingError::UnexpectedEof)?;
    let mut out = Vec::with_capacity(count);
    let mut bit_pos = 0usize;
    for _ in 0..count {
        let mut value: u64 = 0;
        for bit in 0..bit_width as usize {
            let global_bit = bit_pos + bit;
            let byte = bytes[global_bit / 8];
            let b = (byte >> (global_bit % 8)) & 1;
            value |= (b as u64) << bit;
        }
        out.push(value);
        bit_pos += bit_width as usize;
    }
    Ok(out)
}

/// Decode a DELTA_BINARY_PACKED-encoded stream of `i64` values (used for
/// INT32/INT64 data pages).
///
/// Layout: a header (`block_size`, `miniblocks_per_block`, `total_value_count`,
/// zigzag `first_value`), followed by blocks of `block_size` values each; a
/// block starts with a zigzag `min_delta` and one bit-width byte per
/// miniblock, then the miniblocks themselves (each `block_size /
/// miniblocks_per_block` plain bit-packed values — the packed value plus
/// `min_delta` is the delta from the previous value).
///
/// Spec: https://parquet.apache.org/docs/file-format/data-pages/encodings/#delta-encoding-delta_binary_packed--5
pub fn decode_delta_binary_packed(data: &[u8]) -> Result<Vec<i64>> {
    let mut pos = 0usize;
    let block_size = read_unsigned_varint(data, &mut pos)? as usize;
    let miniblocks_per_block = read_unsigned_varint(data, &mut pos)? as usize;
    let total_count = read_unsigned_varint(data, &mut pos)? as usize;
    let first_value = read_zigzag_varint(data, &mut pos)?;

    let mut out = Vec::with_capacity(total_count);
    if total_count == 0 {
        return Ok(out);
    }
    out.push(first_value);
    let mut prev = first_value;
    let values_per_miniblock = block_size.checked_div(miniblocks_per_block).unwrap_or(0);

    while out.len() < total_count {
        let min_delta = read_zigzag_varint(data, &mut pos)?;
        let mut bit_widths = Vec::with_capacity(miniblocks_per_block);
        for _ in 0..miniblocks_per_block {
            bit_widths.push(*data.get(pos).ok_or(EncodingError::UnexpectedEof)? as u32);
            pos += 1;
        }
        for bit_width in bit_widths {
            if out.len() >= total_count {
                break;
            }
            let packed = unpack_bit_packed(&data[pos..], bit_width, values_per_miniblock)?;
            pos += (values_per_miniblock * bit_width as usize).div_ceil(8);
            for raw in packed {
                if out.len() >= total_count {
                    break;
                }
                prev += min_delta + raw as i64;
                out.push(prev);
            }
        }
    }
    Ok(out)
}

/// Number of bits needed to represent values in `0..=max_level`.
pub fn bit_width_for_max_level(max_level: u32) -> u32 {
    32 - max_level.leading_zeros()
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
