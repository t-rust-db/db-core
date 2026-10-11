//! Value encodings for data and dictionary pages: PLAIN for every physical
//! type the writer emits, and the RLE/bit-packed hybrid used for definition
//! levels and dictionary indices. Each is the inverse of a decoder in
//! [`crate::storage::parquet::encoding`] or
//! [`crate::storage::parquet::reader`].
//!
//! Spec: https://parquet.apache.org/docs/file-format/data-pages/encodings/

use super::thrift::write_varint;

/// Bits needed to represent `max` (0 for 0).
pub(crate) fn bit_width(max: u32) -> u32 {
    u32::BITS.wrapping_sub(max.leading_zeros())
}

/// Packs `values` LSB-first at `bit_width` bits each, padding the last byte
/// with zeros. Bits of a value above `bit_width` are dropped.
fn bit_pack(out: &mut Vec<u8>, values: &[u32], bit_width: u32) {
    let mask = 1u64
        .checked_shl(bit_width)
        .map_or(u64::MAX, |m| m.wrapping_sub(1));
    let mut acc: u64 = 0;
    let mut bits: u32 = 0;
    for &v in values {
        acc |= (u64::from(v) & mask).wrapping_shl(bits);
        bits = bits.wrapping_add(bit_width);
        while bits >= 8 {
            out.push(low_byte(acc));
            acc = acc.wrapping_shr(8);
            bits = bits.wrapping_sub(8);
        }
    }
    if bits > 0 {
        out.push(low_byte(acc));
    }
}

fn low_byte(v: u64) -> u8 {
    u8::try_from(v & 0xff).unwrap_or(0)
}

/// Length of the run of values equal to `values[start]`.
fn run_length(values: &[u32], start: usize) -> usize {
    let rest = values.get(start..).unwrap_or_default();
    rest.first()
        .map_or(0, |first| rest.iter().take_while(|v| *v == first).count())
}

/// RLE/bit-packed hybrid encoding of `values` (no length prefix).
///
/// Runs of eight or more equal values become RLE runs; everything else is
/// bit-packed in groups of eight. A bit-packed run must hold a multiple of
/// eight values except at the very end (readers stop at the value count, so
/// only the last group may be padded), so before an RLE run the pending
/// literals are topped up to a multiple of eight from the run itself.
pub(crate) fn rle_hybrid(out: &mut Vec<u8>, values: &[u32], bit_width: u32) {
    let value_bytes = usize::try_from(bit_width.div_ceil(8)).unwrap_or(4);
    let mut literals: Vec<u32> = Vec::new();
    let mut i = 0usize;
    while let Some(&value) = values.get(i) {
        let run = run_length(values, i);
        let pad = 8usize
            .wrapping_sub(literals.len().wrapping_rem(8))
            .wrapping_rem(8);
        match run.checked_sub(pad) {
            Some(rest) if rest >= 8 => {
                literals.extend(std::iter::repeat_n(value, pad));
                flush_literals(out, &mut literals, bit_width);
                write_varint(out, u64::try_from(rest).unwrap_or(u64::MAX).wrapping_shl(1));
                let bytes = value.to_le_bytes();
                out.extend_from_slice(bytes.get(..value_bytes).unwrap_or(&bytes));
                i = i.saturating_add(run);
            }
            _ => {
                literals.push(value);
                i = i.saturating_add(1);
            }
        }
    }
    flush_literals(out, &mut literals, bit_width);
}

/// Writes the pending literals as one bit-packed run (header: group count
/// with the low bit set) and clears them.
fn flush_literals(out: &mut Vec<u8>, literals: &mut Vec<u32>, bit_width: u32) {
    if literals.is_empty() {
        return;
    }
    let groups = literals.len().div_ceil(8);
    let header = u64::try_from(groups).unwrap_or(u64::MAX).wrapping_shl(1) | 1;
    write_varint(out, header);
    literals.resize(groups.saturating_mul(8), 0);
    bit_pack(out, literals, bit_width);
    literals.clear();
}

/// PLAIN BOOLEAN: one bit per value, LSB-first.
pub(crate) fn plain_bool(out: &mut Vec<u8>, values: &[bool]) {
    let bits: Vec<u32> = values.iter().map(|&b| u32::from(b)).collect();
    bit_pack(out, &bits, 1);
}

/// PLAIN BYTE_ARRAY: a little-endian `u32` length, then the bytes. `None`
/// if a value is longer than a `u32` can describe.
pub(crate) fn plain_byte_array(out: &mut Vec<u8>, value: &[u8]) -> Option<()> {
    let len = u32::try_from(value.len()).ok()?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(value);
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::parquet::encoding::decode_hybrid_rle_bitpacked;
    use crate::test_rng::Rng;

    fn decode(bytes: &[u8], bit_width: u32, n: usize) -> Vec<u32> {
        decode_hybrid_rle_bitpacked(bytes, bit_width, n).unwrap()
    }

    #[test]
    fn bit_width_matches_the_largest_value() {
        assert_eq!(bit_width(0), 0);
        assert_eq!(bit_width(1), 1);
        assert_eq!(bit_width(255), 8);
        assert_eq!(bit_width(256), 9);
        assert_eq!(bit_width(u32::MAX), 32);
    }

    #[test]
    fn long_runs_become_rle_runs() {
        let values = vec![1u32; 1000];
        let mut out = Vec::new();
        rle_hybrid(&mut out, &values, 1);
        // One RLE header (1000 << 1 as a varint) plus one value byte.
        assert_eq!(out.len(), 3);
        assert_eq!(decode(&out, 1, 1000), values);
    }

    #[test]
    fn a_run_after_odd_literals_is_aligned_first() {
        // Three literals, then a run of 20: five of the run top the
        // literals up to a group of eight, the other 15 are an RLE run.
        let mut values = vec![0u32, 1, 0];
        values.extend(std::iter::repeat_n(1, 20));
        let mut out = Vec::new();
        rle_hybrid(&mut out, &values, 1);
        assert_eq!(decode(&out, 1, values.len()), values);
    }

    #[test]
    fn a_short_run_stays_literal() {
        // Two literals then a run of 9: aligning takes 6, leaving 3 < 8.
        let mut values = vec![2u32, 3];
        values.extend(std::iter::repeat_n(5, 9));
        let mut out = Vec::new();
        rle_hybrid(&mut out, &values, 3);
        assert_eq!(decode(&out, 3, values.len()), values);
    }

    #[test]
    fn empty_input_writes_nothing() {
        let mut out = Vec::new();
        rle_hybrid(&mut out, &[], 4);
        assert!(out.is_empty());
    }

    #[test]
    fn plain_bool_packs_lsb_first() {
        let mut out = Vec::new();
        plain_bool(
            &mut out,
            &[true, false, true, true, false, false, false, false, true],
        );
        assert_eq!(out, vec![0b0000_1101, 0b0000_0001]);
    }

    #[test]
    fn plain_byte_array_prefixes_the_length() {
        let mut out = Vec::new();
        assert_eq!(plain_byte_array(&mut out, b"ab"), Some(()));
        assert_eq!(out, vec![2, 0, 0, 0, b'a', b'b']);
    }

    #[test]
    fn random_values_round_trip_at_every_width() {
        let mut rng = Rng::new(0x726c_6568);
        for width in 1u32..=32 {
            let mask = 1u64.checked_shl(width).map_or(u64::MAX, |m| m - 1);
            for _ in 0..60 {
                let mut values: Vec<u32> = (0..rng.below(300))
                    .map(|_| u32::try_from(rng.next() & mask).unwrap())
                    .collect();
                // Splice in runs so RLE runs and the alignment path occur.
                for _ in 0..rng.below(5) {
                    let at = rng.below(values.len() as u64 + 1) as usize;
                    let v = values.get(at).copied().unwrap_or(0);
                    values.splice(at..at, std::iter::repeat_n(v, 1 + rng.below(40) as usize));
                }
                let mut out = Vec::new();
                rle_hybrid(&mut out, &values, width);
                assert_eq!(decode(&out, width, values.len()), values, "width {width}");
            }
        }
    }
}
