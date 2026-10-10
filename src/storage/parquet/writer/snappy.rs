//! Snappy raw-block compressor, the mirror of
//! [`crate::storage::parquet::compression::snappy::decompress`].
//!
//! Greedy LZ77 over a hash table of 4-byte sequences: literals, and copies
//! with a 2-byte offset (so matches are looked for at most 65535 bytes
//! back). It trades ratio for simplicity, not correctness: every output is
//! a valid Snappy block.
//!
//! Format: https://github.com/google/snappy/blob/main/format_description.txt

use super::thrift::write_varint;

const HASH_BITS: u32 = 14;
const MIN_MATCH: usize = 4;
const MAX_OFFSET: usize = 65_535;
/// A literal element can describe up to 2^32 bytes; chunking at 64 KiB
/// keeps every length in the 1-, 2- or 3-byte forms.
const LITERAL_CHUNK: usize = 1 << 16;

/// Compress `input` into one Snappy raw block.
pub(crate) fn compress(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len().saturating_add(32));
    write_varint(&mut out, u64::try_from(input.len()).unwrap_or(u64::MAX));
    let mut table = vec![usize::MAX; 1usize.wrapping_shl(HASH_BITS)];
    let mut literal_start = 0usize;
    let mut i = 0usize;
    while let Some(word) = read_u32(input, i) {
        let slot = table.get_mut(hash(word));
        let candidate = slot.map_or(usize::MAX, |s| std::mem::replace(s, i));
        let length = match_length(input, candidate, i);
        if length >= MIN_MATCH {
            emit_literal(&mut out, input.get(literal_start..i).unwrap_or_default());
            emit_copy(&mut out, i.wrapping_sub(candidate), length);
            i = i.saturating_add(length);
            literal_start = i;
        } else {
            i = i.saturating_add(1);
        }
    }
    emit_literal(&mut out, input.get(literal_start..).unwrap_or_default());
    out
}

fn read_u32(input: &[u8], at: usize) -> Option<u32> {
    let end = at.checked_add(4)?;
    let bytes: [u8; 4] = input.get(at..end)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

fn hash(word: u32) -> usize {
    let h = word
        .wrapping_mul(0x1e35_a7bd)
        .wrapping_shr(u32::BITS.wrapping_sub(HASH_BITS));
    usize::try_from(h).unwrap_or(0)
}

/// How many bytes at `candidate` repeat the bytes at `at`; 0 when there is
/// no candidate or it is too far back for a 2-byte offset.
fn match_length(input: &[u8], candidate: usize, at: usize) -> usize {
    let Some(offset) = at.checked_sub(candidate) else {
        return 0;
    };
    if !(1..=MAX_OFFSET).contains(&offset) {
        return 0;
    }
    let earlier = input.get(candidate..).unwrap_or_default();
    let later = input.get(at..).unwrap_or_default();
    earlier
        .iter()
        .zip(later)
        .take_while(|(a, b)| a == b)
        .count()
}

fn emit_literal(out: &mut Vec<u8>, literal: &[u8]) {
    for chunk in literal.chunks(LITERAL_CHUNK) {
        let n = chunk.len().saturating_sub(1);
        match u8::try_from(n) {
            Ok(small) if small < 60 => out.push(small.wrapping_shl(2)),
            _ => {
                let len_bytes = u32::try_from(n).unwrap_or(u32::MAX).to_le_bytes();
                let used = if n < 256 {
                    1
                } else if n < 65_536 {
                    2
                } else {
                    3
                };
                let tag = u8::try_from(59usize.saturating_add(used)).unwrap_or(62);
                out.push(tag.wrapping_shl(2));
                out.extend_from_slice(len_bytes.get(..used).unwrap_or_default());
            }
        }
        out.extend_from_slice(chunk);
    }
}

/// Copies of at most 64 bytes each; a remainder of 65..=67 is split 60 +
/// rest so the last copy is still at least four bytes.
fn emit_copy(out: &mut Vec<u8>, offset: usize, mut length: usize) {
    while length >= 68 {
        emit_copy_2(out, offset, 64);
        length = length.saturating_sub(64);
    }
    if length > 64 {
        emit_copy_2(out, offset, 60);
        length = length.saturating_sub(60);
    }
    emit_copy_2(out, offset, length);
}

/// A copy with a 2-byte offset: tag `(length - 1) << 2 | 0b10`.
fn emit_copy_2(out: &mut Vec<u8>, offset: usize, length: usize) {
    let tag = u8::try_from(length.saturating_sub(1)).unwrap_or(63);
    out.push(tag.wrapping_shl(2) | 0b10);
    out.extend_from_slice(&u16::try_from(offset).unwrap_or(u16::MAX).to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::parquet::compression;
    use crate::storage::parquet::footer::Codec;

    fn decompress(data: &[u8], size: usize) -> compression::Result<Vec<u8>> {
        compression::decompress(Codec::Snappy, data, size).map(|c| c.into_owned())
    }
    use crate::storage::parquet::writer::tests::Rng;

    fn round_trip(input: &[u8]) -> Vec<u8> {
        let compressed = compress(input);
        assert_eq!(decompress(&compressed, input.len()).unwrap(), input);
        compressed
    }

    #[test]
    fn empty_and_tiny_inputs() {
        round_trip(b"");
        round_trip(b"a");
        round_trip(b"abc");
    }

    #[test]
    fn repetitive_input_shrinks() {
        let input = b"severity=INFO service=api ".repeat(400);
        let compressed = round_trip(&input);
        assert!(compressed.len() < input.len() / 10, "{}", compressed.len());
    }

    #[test]
    fn long_literals_use_the_wide_length_forms() {
        // Incompressible bytes of each literal-length class: 1-byte (60..256),
        // 2-byte (256..65536) and a run over one 64 KiB chunk.
        let mut x: u32 = 0x9e37_79b9;
        let noise: Vec<u8> = (0..70_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x.to_le_bytes()[0]
            })
            .collect();
        for n in [61, 300, 70_000] {
            round_trip(&noise[..n]);
        }
    }

    #[test]
    fn long_matches_are_split_into_copies() {
        // One long run: copies of 64 with an overlapping offset of 1, and
        // the 65..=67 remainder split.
        for n in [5, 68, 69, 70, 71, 200] {
            round_trip(&vec![7u8; n]);
        }
    }

    #[test]
    fn matches_further_than_a_2_byte_offset_are_not_used() {
        let mut input = b"0123456789abcdef".to_vec();
        input.extend(std::iter::repeat_n(b'.', 70_000));
        input.extend_from_slice(b"0123456789abcdef");
        round_trip(&input);
    }

    #[test]
    fn random_inputs_round_trip() {
        // A small alphabet half the time so matches of every length occur.
        let mut rng = Rng::new(0x736e_6170);
        for _ in 0..500 {
            let input: Vec<u8> = (0..rng.below(4000))
                .map(|_| match rng.below(4) {
                    0 => b'a',
                    1 => b'b',
                    _ => rng.next() as u8,
                })
                .collect();
            round_trip(&input);
        }
    }
}
