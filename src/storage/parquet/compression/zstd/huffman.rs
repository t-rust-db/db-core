//! Huffman-coded literals (RFC 8878 §4.2).

use super::bits::{Backward, Stream};
use super::fse;
use super::{corrupt, Result};

/// The longest code zstd allows.
const MAX_BITS: u32 = 11;

/// A decoding table: `(symbol, code length)` for every `max_bits`-bit
/// prefix.
#[derive(Debug, Clone, Default)]
pub(super) struct Table {
    max_bits: u32,
    entries: Vec<(u8, u8)>,
}

impl Table {
    /// Reads a tree description at the start of `data`; returns the table
    /// and the bytes the description took.
    pub(super) fn read(data: &[u8]) -> Result<(Table, usize)> {
        let header = *data
            .first()
            .ok_or_else(|| corrupt("missing Huffman header"))?;
        let (mut weights, used) = if header < 128 {
            fse_weights(data, usize::from(header))?
        } else {
            direct_weights(data, usize::from(header.wrapping_sub(127)))?
        };
        // The last weight is implied: the others' total must complete to a
        // power of two.
        let total = weights
            .iter()
            .filter(|&&w| w > 0)
            .try_fold(0u32, |sum, &w| {
                1u32.checked_shl(u32::from(w).wrapping_sub(1))
                    .and_then(|v| sum.checked_add(v))
            });
        let total = total
            .filter(|&t| t > 0)
            .ok_or_else(|| corrupt("bad Huffman weights"))?;
        let max_bits = 32u32.wrapping_sub(total.leading_zeros());
        if max_bits > MAX_BITS {
            return Err(corrupt("Huffman code too long"));
        }
        let left = (1u32 << max_bits).wrapping_sub(total);
        if !left.is_power_of_two() {
            return Err(corrupt("Huffman weights do not complete a tree"));
        }
        weights.push(u8::try_from(left.trailing_zeros().wrapping_add(1)).unwrap_or(0));
        // Codes are handed out by increasing weight, then by symbol.
        let mut rank_start = [0u32; MAX_BITS as usize + 2];
        let mut next = 0u32;
        for (w, start) in rank_start.iter_mut().enumerate().skip(1) {
            *start = next;
            let n = weights.iter().filter(|&&x| usize::from(x) == w).count();
            let n = u32::try_from(n).unwrap_or(0);
            next = next.wrapping_add(n.wrapping_shl(u32::try_from(w).unwrap_or(0).wrapping_sub(1)));
        }
        let mut entries = vec![(0u8, 0u8); 1usize << max_bits];
        for (symbol, &w) in weights.iter().enumerate() {
            if w == 0 {
                continue;
            }
            let length = (1u32 << w) >> 1;
            let start = rank_start
                .get_mut(usize::from(w))
                .ok_or_else(|| corrupt("Huffman weight too large"))?;
            let bits = u8::try_from(max_bits.wrapping_add(1).wrapping_sub(u32::from(w)))
                .map_err(|_| corrupt("Huffman weight too large"))?;
            let from = usize::try_from(*start).unwrap_or(usize::MAX);
            let to = from.saturating_add(usize::try_from(length).unwrap_or(0));
            let slots = entries
                .get_mut(from..to)
                .ok_or_else(|| corrupt("Huffman codes overflow the table"))?;
            let symbol = u8::try_from(symbol).map_err(|_| corrupt("too many Huffman symbols"))?;
            slots.fill((symbol, bits));
            *start = start.wrapping_add(length);
        }
        Ok((Table { max_bits, entries }, used))
    }

    /// Decodes `out.len()` symbols from one stream, which must end exactly
    /// at its first bit.
    pub(super) fn decode(&self, stream: &[u8], out: &mut [u8]) -> Result<()> {
        let mut br = Stream::new(stream).ok_or_else(|| corrupt("bad Huffman stream"))?;
        self.run(&mut br, stream, out)?;
        finish(&br)
    }

    /// Decodes four streams into their outputs, the last of which is the
    /// shortest: groups of five symbols alternate between the streams, so
    /// their independent dependency chains overlap.
    pub(super) fn decode4(&self, streams: [&[u8]; 4], outs: [&mut [u8]; 4]) -> Result<()> {
        let [sa, sb, sc, sd] = streams;
        let [oa, ob, oc, od] = outs;
        let mut ra = Stream::new(sa).ok_or_else(|| corrupt("bad Huffman stream"))?;
        let mut rb = Stream::new(sb).ok_or_else(|| corrupt("bad Huffman stream"))?;
        let mut rc = Stream::new(sc).ok_or_else(|| corrupt("bad Huffman stream"))?;
        let mut rd = Stream::new(sd).ok_or_else(|| corrupt("bad Huffman stream"))?;
        let head = od.len().wrapping_sub(od.len().wrapping_rem(5));
        let (ha, ta) = split_head(oa, head)?;
        let (hb, tb) = split_head(ob, head)?;
        let (hc, tc) = split_head(oc, head)?;
        let (hd, td) = split_head(od, head)?;
        let groups = ha
            .as_chunks_mut::<5>()
            .0
            .iter_mut()
            .zip(hb.as_chunks_mut::<5>().0.iter_mut())
            .zip(hc.as_chunks_mut::<5>().0.iter_mut())
            .zip(hd.as_chunks_mut::<5>().0.iter_mut());
        for (((ga, gb), gc), gd) in groups {
            self.group(&mut ra, sa, ga.as_mut_slice())?;
            self.group(&mut rb, sb, gb.as_mut_slice())?;
            self.group(&mut rc, sc, gc.as_mut_slice())?;
            self.group(&mut rd, sd, gd.as_mut_slice())?;
        }
        self.run(&mut ra, sa, ta)?;
        self.run(&mut rb, sb, tb)?;
        self.run(&mut rc, sc, tc)?;
        self.run(&mut rd, sd, td)?;
        finish(&ra)?;
        finish(&rb)?;
        finish(&rc)?;
        finish(&rd)
    }

    /// Decodes `out` (any length) from `br`.
    fn run(&self, br: &mut Stream, stream: &[u8], out: &mut [u8]) -> Result<()> {
        out.chunks_mut(5)
            .try_for_each(|group| self.group(br, stream, group))
    }

    /// Decodes up to five symbols: a reload leaves 56 bits, five codes of
    /// at most 11.
    #[inline(always)]
    fn group(&self, br: &mut Stream, stream: &[u8], group: &mut [u8]) -> Result<()> {
        br.reload(stream);
        for slot in group {
            let prefix = usize::try_from(br.peek(self.max_bits)).unwrap_or(usize::MAX);
            let &(symbol, bits) = self
                .entries
                .get(prefix)
                .ok_or_else(|| corrupt("Huffman code outside its table"))?;
            *slot = symbol;
            br.consume(u32::from(bits));
        }
        Ok(())
    }
}

/// `out` split after its first `head` symbols.
fn split_head(out: &mut [u8], head: usize) -> Result<(&mut [u8], &mut [u8])> {
    out.split_at_mut_checked(head)
        .ok_or_else(|| corrupt("short Huffman output"))
}

/// Whether the stream was read exactly to its first bit.
fn finish(br: &Stream) -> Result<()> {
    if !br.finished() {
        return Err(corrupt("Huffman stream not read to its end"));
    }
    Ok(())
}

/// Weights FSE-coded in `size` bytes after the header, decoded with two
/// interleaved states.
fn fse_weights(data: &[u8], size: usize) -> Result<(Vec<u8>, usize)> {
    let body = data
        .get(1..size.saturating_add(1))
        .ok_or_else(|| corrupt("short Huffman weights"))?;
    let (table, used) = fse::Table::read(body, 6, 255)?;
    let stream = body
        .get(used..)
        .ok_or_else(|| corrupt("short Huffman weights"))?;
    let mut br = Backward::new(stream).ok_or_else(|| corrupt("bad Huffman weight stream"))?;
    let mut s1 = usize::try_from(br.read(stream, table.log)).unwrap_or(usize::MAX);
    let mut s2 = usize::try_from(br.read(stream, table.log)).unwrap_or(usize::MAX);
    let mut weights = Vec::with_capacity(256);
    loop {
        let e = table.entry(s1)?;
        weights.push(e.symbol);
        s1 = usize::from(e.base)
            .wrapping_add(usize::try_from(br.read(stream, u32::from(e.bits))).unwrap_or(0));
        if br.overflowed() {
            weights.push(table.entry(s2)?.symbol);
            break;
        }
        let e = table.entry(s2)?;
        weights.push(e.symbol);
        s2 = usize::from(e.base)
            .wrapping_add(usize::try_from(br.read(stream, u32::from(e.bits))).unwrap_or(0));
        if br.overflowed() {
            weights.push(table.entry(s1)?.symbol);
            break;
        }
        if weights.len() > 255 {
            return Err(corrupt("too many Huffman weights"));
        }
    }
    // The last iteration can add two weights past the loop's bound; the
    // implied last weight then makes at most 256 symbols.
    if weights.len() > 255 {
        return Err(corrupt("too many Huffman weights"));
    }
    Ok((weights, size.saturating_add(1)))
}

/// `count` weights as 4-bit nibbles after the header, high nibble first.
fn direct_weights(data: &[u8], count: usize) -> Result<(Vec<u8>, usize)> {
    let bytes = count.div_ceil(2);
    let body = data
        .get(1..bytes.saturating_add(1))
        .ok_or_else(|| corrupt("short Huffman weights"))?;
    let weights = body
        .iter()
        .flat_map(|&b| [b >> 4, b & 0xF])
        .take(count)
        .collect();
    Ok((weights, bytes.saturating_add(1)))
}
