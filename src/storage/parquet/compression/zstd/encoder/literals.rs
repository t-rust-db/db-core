//! The literals section of a block (RFC 8878 §3.1.1.3.1): raw, RLE, or
//! Huffman-coded, whichever is smallest.
//!
//! The Huffman code is the decoder's canonical layout read backwards: a
//! symbol of weight `w` (code length `max_bits + 1 - w`) takes the next
//! `2^(w-1)` slots of the `max_bits`-bit prefix space, by increasing
//! weight then symbol. The tree travels as weights, the last symbol's
//! implied, as 4-bit nibbles (up to 128 weights) or FSE-coded.

use super::{BitWriter, EncodeTable};

/// The longest code zstd allows.
const MAX_BITS: u32 = 11;
/// Under this many literals a tree costs more than it saves.
const MIN_HUFFMAN: usize = 64;
/// FSE accuracy of the weights' table: the largest the decoder reads.
const WEIGHTS_LOG: u32 = 6;

pub(super) fn write(out: &mut Vec<u8>, literals: &[u8]) {
    if let Some(byte) = run_byte(literals) {
        size_header(out, 1, literals.len());
        out.push(byte);
        return;
    }
    if literals.len() >= MIN_HUFFMAN {
        if let Some(section) = huffman(literals) {
            if section.len() < literals.len().saturating_add(3) {
                out.extend_from_slice(&section);
                return;
            }
        }
    }
    size_header(out, 0, literals.len());
    out.extend_from_slice(literals);
}

/// The byte `bytes` repeats throughout, when it is at least two long.
pub(super) fn run_byte(bytes: &[u8]) -> Option<u8> {
    let (&first, rest) = bytes.split_first()?;
    rest.first()?;
    rest.iter().all(|&b| b == first).then_some(first)
}

/// A raw (0) or RLE (1) section header: the size in 5, 12 or 20 bits.
fn size_header(out: &mut Vec<u8>, kind: u32, size: usize) {
    let size = u32::try_from(size).unwrap_or(0);
    if size < 32 {
        out.push(u8::try_from(size.wrapping_shl(3) | kind).unwrap_or(0));
    } else if size < 4096 {
        out.extend_from_slice(&(size.wrapping_shl(4) | 0b0100 | kind).to_le_bytes()[..2]);
    } else {
        out.extend_from_slice(&(size.wrapping_shl(4) | 0b1100 | kind).to_le_bytes()[..3]);
    }
}

/// A complete compressed-literals section, or `None` when the symbols
/// cannot be coded (a single symbol) or the tree cannot be described.
fn huffman(literals: &[u8]) -> Option<Vec<u8>> {
    let mut counts = [0u32; 256];
    for &b in literals {
        let c = counts.get_mut(usize::from(b))?;
        *c = c.saturating_add(1);
    }
    let lengths = code_lengths(&counts)?;
    let max_bits = u32::from(*lengths.iter().max()?);
    // Weight of a symbol: `max_bits + 1 - length`, 0 when unused.
    let weight_of = |symbol: usize| -> u8 {
        match lengths.get(symbol).copied().unwrap_or(0) {
            0 => 0,
            len => u8::try_from(max_bits.wrapping_add(1).wrapping_sub(u32::from(len))).unwrap_or(0),
        }
    };
    let weights: Vec<u8> = (0..256).map(weight_of).collect();
    let last = weights.iter().rposition(|&w| w > 0)?;
    let tree = tree_description(weights.get(..last)?)?;

    // Canonical codes: by increasing weight, then symbol.
    let mut codes = [0u32; 256];
    let mut start = 0u32;
    for w in 1..=max_bits {
        for (symbol, &sw) in weights.iter().enumerate() {
            if u32::from(sw) == w {
                *codes.get_mut(symbol)? = start >> w.wrapping_sub(1);
                start = start.wrapping_add(1u32.wrapping_shl(w.wrapping_sub(1)));
            }
        }
    }
    let stream = |symbols: &[u8]| -> Vec<u8> {
        let mut bw = BitWriter::default();
        for &s in symbols.iter().rev() {
            let i = usize::from(s);
            bw.put(
                u64::from(codes.get(i).copied().unwrap_or(0)),
                u32::from(lengths.get(i).copied().unwrap_or(0)),
            );
        }
        bw.finish()
    };
    let mut body = tree;
    let four = literals.len() >= 256;
    if four {
        let per = literals.len().div_ceil(4);
        let parts: Vec<Vec<u8>> = literals.chunks(per).map(stream).collect();
        for part in parts.iter().take(3) {
            body.extend_from_slice(&u16::try_from(part.len()).ok()?.to_le_bytes());
        }
        for part in &parts {
            body.extend_from_slice(part);
        }
    } else {
        body.extend_from_slice(&stream(literals));
    }
    // Sizes of 10, 14 or 18 bits; a header of 3, 3, 4 or 5 bytes.
    let (format, bits) = if !four {
        (0u64, 10u32)
    } else if literals.len().max(body.len()) < 1024 {
        (1, 10)
    } else if literals.len().max(body.len()) < 16_384 {
        (2, 14)
    } else {
        (3, 18)
    };
    let header_len = match format {
        0 | 1 => 3,
        2 => 4,
        _ => 5,
    };
    if body.len().max(literals.len()) >= 1usize.checked_shl(bits)? {
        return None;
    }
    let h = 2u64
        | (format << 2)
        | (u64::try_from(literals.len()).ok()? << 4)
        | (u64::try_from(body.len()).ok()? << 4u32.wrapping_add(bits));
    let mut section = Vec::with_capacity(body.len().saturating_add(header_len));
    section.extend_from_slice(h.to_le_bytes().get(..header_len)?);
    section.extend_from_slice(&body);
    Some(section)
}

/// Code lengths (at most [`MAX_BITS`]) of an optimal prefix code, by
/// halving the counts until the longest fits; `None` under 2 symbols.
fn code_lengths(counts: &[u32; 256]) -> Option<[u8; 256]> {
    let mut counts = *counts;
    loop {
        let lengths = huffman_lengths(&counts)?;
        if lengths.iter().all(|&l| u32::from(l) <= MAX_BITS) {
            return Some(lengths);
        }
        for c in &mut counts {
            if *c > 0 {
                *c = (*c >> 1) | 1;
            }
        }
    }
}

/// The unrestricted Huffman code lengths, by the two-queue merge.
fn huffman_lengths(counts: &[u32; 256]) -> Option<[u8; 256]> {
    let mut leaves: Vec<(u64, usize)> = counts
        .iter()
        .enumerate()
        .filter(|&(_, &c)| c > 0)
        .map(|(s, &c)| (u64::from(c), s))
        .collect();
    if leaves.len() < 2 {
        return None;
    }
    leaves.sort_unstable();
    let n = leaves.len();
    // Nodes 0..n are the leaves, in order; internal nodes follow.
    let mut weight: Vec<u64> = leaves.iter().map(|&(w, _)| w).collect();
    let mut parent = vec![0usize; n];
    let (mut leaf, mut internal) = (0usize, n);
    let pick = |weight: &Vec<u64>, leaf: &mut usize, internal: &mut usize| -> Option<usize> {
        let l = (*leaf < n).then(|| weight.get(*leaf).copied()).flatten();
        let i = (*internal < weight.len())
            .then(|| weight.get(*internal).copied())
            .flatten();
        match (l, i) {
            (Some(l), Some(i)) if l <= i => {
                *leaf = leaf.wrapping_add(1);
                Some(leaf.wrapping_sub(1))
            }
            (Some(_), None) => {
                *leaf = leaf.wrapping_add(1);
                Some(leaf.wrapping_sub(1))
            }
            (_, Some(_)) => {
                *internal = internal.wrapping_add(1);
                Some(internal.wrapping_sub(1))
            }
            (None, None) => None,
        }
    };
    for _ in 1..n {
        let a = pick(&weight, &mut leaf, &mut internal)?;
        let b = pick(&weight, &mut leaf, &mut internal)?;
        let node = weight.len();
        weight.push(weight.get(a)?.saturating_add(*weight.get(b)?));
        parent.push(0);
        *parent.get_mut(a)? = node;
        *parent.get_mut(b)? = node;
    }
    // A parent is created after its children: depths run from the root down.
    let mut depth = vec![0u32; weight.len()];
    for node in (0..weight.len().saturating_sub(1)).rev() {
        let d = depth.get(*parent.get(node)?)?.wrapping_add(1);
        *depth.get_mut(node)? = d;
    }
    let mut lengths = [0u8; 256];
    for (node, &(_, symbol)) in leaves.iter().enumerate() {
        *lengths.get_mut(symbol)? = u8::try_from(*depth.get(node)?).ok()?;
    }
    Some(lengths)
}

/// The Huffman tree description of the weights of symbols `0..last`: 4-bit
/// nibbles when they fit, else FSE-coded, whichever is smaller.
fn tree_description(weights: &[u8]) -> Option<Vec<u8>> {
    let direct = (weights.len() <= 128).then(|| {
        let mut d = vec![u8::try_from(weights.len().wrapping_add(127)).unwrap_or(255)];
        for pair in weights.chunks(2) {
            let hi = pair.first().copied().unwrap_or(0);
            let lo = pair.get(1).copied().unwrap_or(0);
            d.push(hi << 4 | lo);
        }
        d
    });
    let fse = fse_weights(weights);
    match (direct, fse) {
        (Some(d), Some(f)) => Some(if f.len() < d.len() { f } else { d }),
        (d, f) => d.or(f),
    }
}

/// The weights FSE-coded: a header byte (the size), the table, and a
/// stream of two interleaved states, even weights on one and odd on the
/// other. The decoder stops when an update runs past the stream's start,
/// which every state does here: no count exceeds half the table, so every
/// state reads at least a bit.
fn fse_weights(weights: &[u8]) -> Option<Vec<u8>> {
    let n = weights.len();
    if n < 2 {
        return None;
    }
    let mut hist = [0u32; 16];
    for &w in weights {
        let h = hist.get_mut(usize::from(w))?;
        *h = h.saturating_add(1);
    }
    let alphabet = hist.iter().rposition(|&h| h > 0)?.wrapping_add(1);
    let norm = normalize(hist.get(..alphabet)?, WEIGHTS_LOG, 1 << (WEIGHTS_LOG - 1))?;
    let table = EncodeTable::new(&norm, WEIGHTS_LOG)?;
    let mut bw = BitWriter::default();
    let mut states = [0usize; 2];
    let symbol_at = |i: usize| usize::from(weights.get(i).copied().unwrap_or(0));
    for i in n.saturating_sub(2)..n {
        *states.get_mut(i & 1)? = table.any_state(symbol_at(i));
    }
    for i in (0..n.saturating_sub(2)).rev() {
        let state = states.get_mut(i & 1)?;
        let step = table.step(symbol_at(i), *state);
        bw.put(
            u64::try_from(state.wrapping_sub(usize::from(step.base))).ok()?,
            u32::from(step.bits),
        );
        *state = usize::from(step.state);
    }
    bw.put(u64::try_from(*states.get(1)?).ok()?, WEIGHTS_LOG);
    bw.put(u64::try_from(*states.first()?).ok()?, WEIGHTS_LOG);
    let mut body = write_counts(&norm, WEIGHTS_LOG)?;
    body.extend_from_slice(&bw.finish());
    let mut out = vec![u8::try_from(body.len()).ok().filter(|&l| l < 128)?];
    out.extend_from_slice(&body);
    Some(out)
}

/// `hist` scaled to sum to `1 << log`, every present symbol at least 1 and
/// at most `cap`. `None` if that is impossible (one symbol).
pub(super) fn normalize(hist: &[u32], log: u32, cap: usize) -> Option<Vec<i16>> {
    let size = 1u64.checked_shl(log)?;
    let cap = u64::try_from(cap).ok()?;
    let total: u64 = hist.iter().map(|&h| u64::from(h)).sum();
    let present = hist.iter().filter(|&&h| h > 0).count();
    if present < 2 {
        return None;
    }
    let mut norm: Vec<u64> = hist
        .iter()
        .map(|&h| {
            if h == 0 {
                0
            } else {
                u64::from(h)
                    .saturating_mul(size)
                    .checked_div(total)
                    .unwrap_or(1)
                    .clamp(1, cap)
            }
        })
        .collect();
    // Hand out or take back units one at a time, where they matter most.
    loop {
        let sum: u64 = norm.iter().sum();
        if sum == size {
            break;
        }
        let grow = sum < size;
        let best = (0..norm.len())
            .filter(|&i| {
                let h = hist.get(i).copied().unwrap_or(0);
                let n = norm.get(i).copied().unwrap_or(0);
                h > 0 && if grow { n < cap } else { n > 1 }
            })
            .max_by(|&a, &b| {
                // Growing: the largest count per slot; shrinking: the smallest.
                let at = |i: usize| {
                    (
                        u64::from(hist.get(i).copied().unwrap_or(0)),
                        norm.get(i).copied().unwrap_or(1),
                    )
                };
                let ((ha, na), (hb, nb)) = (at(a), at(b));
                let by_ratio = ha.saturating_mul(nb).cmp(&hb.saturating_mul(na));
                if grow {
                    by_ratio
                } else {
                    by_ratio.reverse()
                }
            })?;
        let slot = norm.get_mut(best)?;
        *slot = if grow {
            slot.wrapping_add(1)
        } else {
            slot.wrapping_sub(1)
        };
    }
    norm.iter().map(|&n| i16::try_from(n).ok()).collect()
}

/// An FSE table description (the inverse of `fse::Table::read`) for counts
/// that are all non-negative.
pub(super) fn write_counts(norm: &[i16], log: u32) -> Option<Vec<u8>> {
    let mut bw = BitWriter::default();
    bw.put(u64::from(log.checked_sub(5)?), 4);
    let mut remaining = 1i32.checked_shl(log)?.wrapping_add(1);
    let mut threshold = 1i32.checked_shl(log)?;
    let mut nb_bits = log.wrapping_add(1);
    let mut symbol = 0usize;
    while remaining > 1 {
        let count = i32::from(*norm.get(symbol)?);
        symbol = symbol.wrapping_add(1);
        let max = threshold
            .wrapping_mul(2)
            .wrapping_sub(1)
            .wrapping_sub(remaining);
        remaining = remaining.wrapping_sub(count);
        let mut value = count.wrapping_add(1);
        if value >= threshold {
            value = value.wrapping_add(max);
        }
        let small = value < max;
        bw.put(
            u64::try_from(value).ok()?,
            nb_bits.wrapping_sub(u32::from(small)),
        );
        if count == 0 {
            // Zeros that follow: 2-bit repeat counts, a 3 meaning "more".
            let mut zeros = 0usize;
            while norm.get(symbol.wrapping_add(zeros)) == Some(&0) {
                zeros = zeros.wrapping_add(1);
            }
            symbol = symbol.wrapping_add(zeros);
            while zeros >= 3 {
                bw.put(3, 2);
                zeros = zeros.wrapping_sub(3);
            }
            bw.put(u64::try_from(zeros).ok()?, 2);
        }
        while remaining < threshold {
            nb_bits = nb_bits.wrapping_sub(1);
            threshold >>= 1;
        }
    }
    Some(bw.finish_plain())
}
