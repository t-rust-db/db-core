//! Frames, blocks, literals and sequences (RFC 8878 §3.1).
//!
//! A frame decodes straight into an output buffer of the page's declared
//! size plus [`SLACK`] bytes, through a cursor: literal runs and matches
//! are copied in fixed 16-byte chunks (wild copies), so a short copy is a
//! couple of loads and stores rather than a `memmove` call. The slack only
//! ever takes the over-write of a copy's last chunk; every copy is checked
//! against the declared size first.

use super::bits::Stream;
use super::fse::{self, LL_DEFAULT, ML_DEFAULT, OF_DEFAULT};
use super::huffman;
use super::xxhash;
use super::{corrupt, Result, ZstdError};

/// Room past the output and the literals for 16-byte wild copies.
const SLACK: usize = 32;

/// The largest block, and the most literals a block can regenerate
/// (RFC 8878 §3.1.1.2: `Block_Maximum_Size`).
const MAX_BLOCK: usize = 128 * 1024;

/// Literals-length code -> (baseline, extra bits).
const LL_CODES: [(u32, u32); 36] = [
    (0, 0),
    (1, 0),
    (2, 0),
    (3, 0),
    (4, 0),
    (5, 0),
    (6, 0),
    (7, 0),
    (8, 0),
    (9, 0),
    (10, 0),
    (11, 0),
    (12, 0),
    (13, 0),
    (14, 0),
    (15, 0),
    (16, 1),
    (18, 1),
    (20, 1),
    (22, 1),
    (24, 2),
    (28, 2),
    (32, 3),
    (40, 3),
    (48, 4),
    (64, 6),
    (128, 7),
    (256, 8),
    (512, 9),
    (1024, 10),
    (2048, 11),
    (4096, 12),
    (8192, 13),
    (16384, 14),
    (32768, 15),
    (65536, 16),
];

/// Match-length code -> (baseline, extra bits).
const ML_CODES: [(u32, u32); 53] = [
    (3, 0),
    (4, 0),
    (5, 0),
    (6, 0),
    (7, 0),
    (8, 0),
    (9, 0),
    (10, 0),
    (11, 0),
    (12, 0),
    (13, 0),
    (14, 0),
    (15, 0),
    (16, 0),
    (17, 0),
    (18, 0),
    (19, 0),
    (20, 0),
    (21, 0),
    (22, 0),
    (23, 0),
    (24, 0),
    (25, 0),
    (26, 0),
    (27, 0),
    (28, 0),
    (29, 0),
    (30, 0),
    (31, 0),
    (32, 0),
    (33, 0),
    (34, 0),
    (35, 1),
    (37, 1),
    (39, 1),
    (41, 1),
    (43, 2),
    (47, 2),
    (51, 3),
    (59, 3),
    (67, 4),
    (83, 4),
    (99, 5),
    (131, 7),
    (259, 8),
    (515, 9),
    (1027, 10),
    (2051, 11),
    (4099, 12),
    (8195, 13),
    (16387, 14),
    (32771, 15),
    (65539, 16),
];

/// The frame's output: `size` bytes plus [`SLACK`], written at `op`.
struct Out {
    buf: Vec<u8>,
    op: usize,
    size: usize,
}

impl Out {
    /// Whether `n` more bytes fit the declared size.
    fn room(&self, n: usize) -> Result<()> {
        if n > self.size.saturating_sub(self.op) {
            // More output than declared: report one byte past, as a
            // stream that stopped there would.
            return Err(ZstdError::SizeMismatch {
                expected: self.size,
                actual: self.size.saturating_add(1),
            });
        }
        Ok(())
    }

    /// Copies `src[from..from + n]` to the cursor in 16-byte chunks; `src`
    /// holds at least 16 bytes past `from + n`, and `n` was checked with
    /// [`Self::room`].
    #[inline(always)]
    fn wild_copy(&mut self, src: &[u8], from: usize, n: usize) -> Result<()> {
        // Most runs fit the first chunk.
        let chunk = src
            .get(from..from.saturating_add(16))
            .and_then(|c| <[u8; 16]>::try_from(c).ok())
            .ok_or_else(|| corrupt("literals overrun"))?;
        self.buf
            .get_mut(self.op..self.op.saturating_add(16))
            .ok_or_else(|| corrupt("output overrun"))?
            .copy_from_slice(&chunk);
        if n > 16 {
            self.wild_copy_rest(src, from, n)?;
        }
        self.op = self.op.wrapping_add(n);
        Ok(())
    }

    /// [`Self::wild_copy`]'s chunks past the first.
    #[inline(never)]
    fn wild_copy_rest(&mut self, src: &[u8], from: usize, n: usize) -> Result<()> {
        let mut i = 16usize;
        while i < n {
            let s = from.wrapping_add(i);
            let d = self.op.wrapping_add(i);
            let chunk = src
                .get(s..s.wrapping_add(16))
                .and_then(|c| <[u8; 16]>::try_from(c).ok())
                .ok_or_else(|| corrupt("literals overrun"))?;
            self.buf
                .get_mut(d..d.wrapping_add(16))
                .ok_or_else(|| corrupt("output overrun"))?
                .copy_from_slice(&chunk);
            i = i.wrapping_add(16);
        }
        Ok(())
    }

    /// Copies `n` bytes from `offset` back; `offset` <= `op`, and `n` was
    /// checked with [`Self::room`].
    #[inline(always)]
    fn copy_match(&mut self, offset: usize, n: usize) -> Result<()> {
        if offset < 16 {
            return self.copy_overlapping(offset, n);
        }
        // Each chunk reads only bytes written before it.
        let src = self.op.wrapping_sub(offset);
        let chunk = self
            .buf
            .get(src..src.wrapping_add(16))
            .and_then(|c| <[u8; 16]>::try_from(c).ok())
            .ok_or_else(|| corrupt("match overrun"))?;
        self.buf
            .get_mut(self.op..self.op.saturating_add(16))
            .ok_or_else(|| corrupt("output overrun"))?
            .copy_from_slice(&chunk);
        if n > 16 {
            self.copy_match_rest(offset, n)?;
        }
        self.op = self.op.wrapping_add(n);
        Ok(())
    }

    /// [`Self::copy_match`]'s chunks past the first, `offset` >= 16.
    #[inline(never)]
    fn copy_match_rest(&mut self, offset: usize, n: usize) -> Result<()> {
        let src = self.op.wrapping_sub(offset);
        let mut i = 16usize;
        while i < n {
            let s = src.wrapping_add(i);
            let d = self.op.wrapping_add(i);
            let chunk = self
                .buf
                .get(s..s.wrapping_add(16))
                .and_then(|c| <[u8; 16]>::try_from(c).ok())
                .ok_or_else(|| corrupt("match overrun"))?;
            self.buf
                .get_mut(d..d.wrapping_add(16))
                .ok_or_else(|| corrupt("output overrun"))?
                .copy_from_slice(&chunk);
            i = i.wrapping_add(16);
        }
        Ok(())
    }

    /// A match that overlaps itself (`offset` < 16): it repeats its own
    /// output, byte by byte, inside one window checked once (`offset + n`
    /// long).
    #[inline(never)]
    fn copy_overlapping(&mut self, offset: usize, n: usize) -> Result<()> {
        let src = self.op.wrapping_sub(offset);
        let window = self
            .buf
            .get_mut(src..self.op.saturating_add(n))
            .ok_or_else(|| corrupt("match overrun"))?;
        for i in 0..n {
            let b = window.get(i).copied().unwrap_or(0);
            if let Some(slot) = window.get_mut(i.wrapping_add(offset)) {
                *slot = b;
            }
        }
        self.op = self.op.wrapping_add(n);
        Ok(())
    }
}

/// What carries from block to block within a frame.
struct State {
    huffman: Option<huffman::Table>,
    ll: Option<fse::Table>,
    of: Option<fse::Table>,
    ml: Option<fse::Table>,
    /// The three repeat offsets, most recent first.
    reps: [usize; 3],
    /// The block's literals, then [`SLACK`] bytes for wild reads.
    literals: Vec<u8>,
    literals_len: usize,
}

/// One frame, decoded to exactly `size` bytes.
pub(super) fn decode(frame: &[u8], size: usize) -> Result<Vec<u8>> {
    if frame.get(..4) != Some(&[0x28, 0xB5, 0x2F, 0xFD]) {
        return Err(ZstdError::Frame("not a zstd frame".to_string()));
    }
    let descriptor = *frame
        .get(4)
        .ok_or_else(|| ZstdError::Frame("short frame header".to_string()))?;
    let single_segment = descriptor & 0x20 != 0;
    let checksum = descriptor & 0x04 != 0;
    if descriptor & 0x03 != 0 {
        return Err(ZstdError::Frame(
            "dictionaries are not supported".to_string(),
        ));
    }
    if descriptor & 0x08 != 0 {
        return Err(ZstdError::Frame(
            "reserved frame header bit set".to_string(),
        ));
    }
    let content_size_bytes = match descriptor >> 6 {
        0 => usize::from(single_segment),
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let mut pos = 5usize
        .wrapping_add(usize::from(!single_segment))
        .wrapping_add(content_size_bytes);
    let content_size = frame
        .get(pos.saturating_sub(content_size_bytes)..pos)
        .ok_or_else(|| ZstdError::Frame("short frame header".to_string()))?;
    if !content_size.is_empty() {
        // The frame's own size must be the page's: a frame that says
        // otherwise is refused before any block is decoded.
        let value = content_size
            .iter()
            .rev()
            .fold(0u64, |acc, &b| acc.wrapping_shl(8) | u64::from(b));
        let value = if content_size_bytes == 2 {
            value.wrapping_add(256)
        } else {
            value
        };
        let value = usize::try_from(value).unwrap_or(usize::MAX);
        if value != size {
            return Err(ZstdError::SizeMismatch {
                expected: size,
                actual: value,
            });
        }
    }
    let mut out = Out {
        buf: vec![0; size.saturating_add(SLACK)],
        op: 0,
        size,
    };
    let mut state = State {
        huffman: None,
        ll: None,
        of: None,
        ml: None,
        reps: [1, 4, 8],
        literals: Vec::new(),
        literals_len: 0,
    };
    loop {
        let header = frame
            .get(pos..pos.saturating_add(3))
            .ok_or_else(|| corrupt("short block header"))?;
        let h = header
            .iter()
            .rev()
            .fold(0u32, |acc, &b| acc.wrapping_shl(8) | u32::from(b));
        pos = pos.wrapping_add(3);
        let last = h & 1 != 0;
        let block_size = usize::try_from(h >> 3).unwrap_or(usize::MAX);
        if block_size > MAX_BLOCK {
            return Err(corrupt("block larger than 128 KiB"));
        }
        match (h >> 1) & 3 {
            0 => {
                let raw = frame
                    .get(pos..pos.saturating_add(block_size))
                    .ok_or_else(|| corrupt("short raw block"))?;
                out.room(block_size)?;
                out.buf
                    .get_mut(out.op..out.op.wrapping_add(block_size))
                    .ok_or_else(|| corrupt("output overrun"))?
                    .copy_from_slice(raw);
                out.op = out.op.wrapping_add(block_size);
                pos = pos.wrapping_add(block_size);
            }
            1 => {
                let b = *frame.get(pos).ok_or_else(|| corrupt("short RLE block"))?;
                out.room(block_size)?;
                out.buf
                    .get_mut(out.op..out.op.wrapping_add(block_size))
                    .ok_or_else(|| corrupt("output overrun"))?
                    .fill(b);
                out.op = out.op.wrapping_add(block_size);
                pos = pos.wrapping_add(1);
            }
            2 => {
                let block = frame
                    .get(pos..pos.saturating_add(block_size))
                    .ok_or_else(|| corrupt("short compressed block"))?;
                compressed_block(block, &mut state, &mut out)?;
                pos = pos.wrapping_add(block_size);
            }
            _ => return Err(corrupt("reserved block type")),
        }
        if last {
            break;
        }
    }
    if out.op != size {
        return Err(ZstdError::SizeMismatch {
            expected: size,
            actual: out.op,
        });
    }
    out.buf.truncate(size);
    if checksum {
        let stored = frame
            .get(pos..pos.saturating_add(4))
            .and_then(|c| <[u8; 4]>::try_from(c).ok())
            .ok_or_else(|| corrupt("missing content checksum"))?;
        // The low 32 bits of the content's XXH64.
        let computed = xxhash::xxh64(&out.buf).to_le_bytes();
        if computed.get(..4) != Some(&stored[..]) {
            return Err(corrupt("content checksum mismatch"));
        }
    }
    Ok(out.buf)
}

fn compressed_block(block: &[u8], state: &mut State, out: &mut Out) -> Result<()> {
    let used = literals_section(block, state)?;
    state.literals_len = state.literals.len();
    state
        .literals
        .resize(state.literals_len.saturating_add(SLACK), 0);
    let rest = block.get(used..).ok_or_else(|| corrupt("short block"))?;
    sequences_section(rest, state, out)
}

/// Decodes the literals section into `state.literals`; returns its size.
fn literals_section(block: &[u8], state: &mut State) -> Result<usize> {
    let b0 = *block.first().ok_or_else(|| corrupt("empty block"))?;
    let kind = b0 & 3;
    let size_format = (b0 >> 2) & 3;
    let byte = |i: usize| -> Result<usize> {
        block
            .get(i)
            .map(|&b| usize::from(b))
            .ok_or_else(|| corrupt("short literals header"))
    };
    let high = usize::from(b0 >> 4);
    state.literals.clear();
    if kind < 2 {
        // Raw or RLE: one size, 5, 12 or 20 bits.
        let (header, regenerated): (usize, usize) = match size_format {
            0 | 2 => (1, usize::from(b0 >> 3)),
            1 => (2, high | byte(1)? << 4),
            _ => (3, high | byte(1)? << 4 | byte(2)? << 12),
        };
        if regenerated > MAX_BLOCK {
            return Err(corrupt("literals larger than a block"));
        }
        if kind == 0 {
            let raw = block
                .get(header..header.saturating_add(regenerated))
                .ok_or_else(|| corrupt("short raw literals"))?;
            state.literals.extend_from_slice(raw);
            return Ok(header.saturating_add(regenerated));
        }
        let b = *block
            .get(header)
            .ok_or_else(|| corrupt("short RLE literals"))?;
        state.literals.resize(regenerated, b);
        return Ok(header.saturating_add(1));
    }
    // Huffman-coded (kind 2 with a tree, 3 reusing the last one): two
    // sizes of 10, 14 or 18 bits, in 1 or 4 streams.
    let (header, bits, streams): (usize, u32, usize) = match size_format {
        0 => (3, 10, 1),
        1 => (3, 10, 4),
        2 => (4, 14, 4),
        _ => (5, 18, 4),
    };
    let h = block
        .get(..header)
        .ok_or_else(|| corrupt("short literals header"))?
        .iter()
        .rev()
        .fold(0u64, |acc, &b| acc.wrapping_shl(8) | u64::from(b));
    let mask = (1u64 << bits).wrapping_sub(1);
    let regenerated = usize::try_from((h >> 4) & mask).unwrap_or(usize::MAX);
    let compressed = usize::try_from((h >> (4u32.wrapping_add(bits))) & mask).unwrap_or(usize::MAX);
    if regenerated > MAX_BLOCK {
        return Err(corrupt("literals larger than a block"));
    }
    let mut body = block
        .get(header..header.saturating_add(compressed))
        .ok_or_else(|| corrupt("short compressed literals"))?;
    if kind == 2 {
        let (table, used) = huffman::Table::read(body)?;
        state.huffman = Some(table);
        body = body
            .get(used..)
            .ok_or_else(|| corrupt("short compressed literals"))?;
    }
    let table = state
        .huffman
        .as_ref()
        .ok_or_else(|| corrupt("treeless literals without a tree"))?;
    state.literals.resize(regenerated, 0);
    if streams == 1 {
        table.decode(body, &mut state.literals)?;
    } else {
        let jump = body.get(..6).ok_or_else(|| corrupt("short jump table"))?;
        let size_at = |i: usize| -> usize {
            usize::from(u16::from_le_bytes([
                jump.get(i).copied().unwrap_or(0),
                jump.get(i.wrapping_add(1)).copied().unwrap_or(0),
            ]))
        };
        let rest = body.get(6..).unwrap_or_default();
        let (a, rest) = rest
            .split_at_checked(size_at(0))
            .ok_or_else(|| corrupt("bad jump table"))?;
        let (b, rest) = rest
            .split_at_checked(size_at(2))
            .ok_or_else(|| corrupt("bad jump table"))?;
        let (c, d) = rest
            .split_at_checked(size_at(4))
            .ok_or_else(|| corrupt("bad jump table"))?;
        // Streams 1-3 hold a quarter each (rounded up), stream 4 the rest.
        let per = regenerated.div_ceil(4);
        let (o1, o) = state.literals.split_at_mut(per.min(regenerated));
        let (o2, o) = o.split_at_mut(per.min(o.len()));
        let (o3, o4) = o.split_at_mut(per.min(o.len()));
        table.decode4([a, b, c, d], [o1, o2, o3, o4])?;
    }
    Ok(header.saturating_add(compressed))
}

/// One of the three sequence tables, per its compression mode; returns
/// the bytes its description took.
fn read_table(
    mode: u8,
    data: &[u8],
    slot: &mut Option<fse::Table>,
    default: &[i16],
    default_log: u32,
    max_log: u32,
    codes: Option<&[(u32, u32)]>,
) -> Result<usize> {
    let used = read_entries(mode, data, slot, default, default_log, max_log)?;
    if mode != 3 {
        slot.as_mut()
            .ok_or_else(|| corrupt("missing sequence table"))?
            .assign_values(codes)?;
    }
    Ok(used)
}

/// [`read_table`] before the table's values are assigned.
fn read_entries(
    mode: u8,
    data: &[u8],
    slot: &mut Option<fse::Table>,
    default: &[i16],
    default_log: u32,
    max_log: u32,
) -> Result<usize> {
    match mode {
        0 => {
            *slot = Some(fse::Table::build(default, default_log)?);
            Ok(0)
        }
        1 => {
            let symbol = *data.first().ok_or_else(|| corrupt("short RLE table"))?;
            if usize::from(symbol) >= default.len() {
                return Err(corrupt("RLE symbol out of range"));
            }
            *slot = Some(fse::Table::rle(symbol));
            Ok(1)
        }
        2 => {
            let (table, used) = fse::Table::read(data, max_log, default.len().saturating_sub(1))?;
            *slot = Some(table);
            Ok(used)
        }
        _ => {
            if slot.is_none() {
                return Err(corrupt("repeated sequence table without a previous one"));
            }
            Ok(0)
        }
    }
}

/// Decodes the sequences section and executes it into `out`.
fn sequences_section(data: &[u8], state: &mut State, out: &mut Out) -> Result<()> {
    let b0 = *data
        .first()
        .ok_or_else(|| corrupt("missing sequences header"))?;
    let byte = |i: usize| -> Result<usize> {
        data.get(i)
            .map(|&b| usize::from(b))
            .ok_or_else(|| corrupt("short sequences header"))
    };
    let (count, mut pos) = match b0 {
        0 => {
            out.room(state.literals_len)?;
            return out.wild_copy(&state.literals, 0, state.literals_len);
        }
        1..=127 => (usize::from(b0), 1),
        128..=254 => ((usize::from(b0).wrapping_sub(128) << 8) | byte(1)?, 2),
        255 => ((byte(1)? | byte(2)? << 8).wrapping_add(0x7F00), 3),
    };
    // Every sequence makes at least 3 bytes (a match is at least 3 long),
    // so a count the output cannot hold is refused before decoding, as
    // more output than declared.
    out.room(count.saturating_mul(3))?;
    let modes = byte(pos)?;
    pos = pos.wrapping_add(1);
    let modes = u8::try_from(modes).unwrap_or(0);
    let rest = |pos: usize| data.get(pos..).unwrap_or_default();
    pos = pos.wrapping_add(read_table(
        modes >> 6,
        rest(pos),
        &mut state.ll,
        &LL_DEFAULT,
        6,
        9,
        Some(&LL_CODES),
    )?);
    pos = pos.wrapping_add(read_table(
        (modes >> 4) & 3,
        rest(pos),
        &mut state.of,
        &OF_DEFAULT,
        5,
        8,
        None,
    )?);
    pos = pos.wrapping_add(read_table(
        (modes >> 2) & 3,
        rest(pos),
        &mut state.ml,
        &ML_DEFAULT,
        6,
        9,
        Some(&ML_CODES),
    )?);
    let (Some(ll), Some(of), Some(ml)) = (&state.ll, &state.of, &state.ml) else {
        return Err(corrupt("missing sequence tables"));
    };
    let stream = data.get(pos..).ok_or_else(|| corrupt("short sequences"))?;
    let mut br = Stream::new(stream).ok_or_else(|| corrupt("bad sequence stream"))?;
    let state_of = |v: u64| usize::try_from(v).unwrap_or(usize::MAX);
    let mut ll_state = state_of(br.read(ll.log));
    let mut of_state = state_of(br.read(of.log));
    let mut ml_state = state_of(br.read(ml.log));
    let literals = &state.literals;
    let literals_len = state.literals_len;
    let reps = &mut state.reps;
    let mut lit = 0usize;
    for i in 0..count {
        let lle = ll.entry(ll_state)?;
        let mle = ml.entry(ml_state)?;
        let ofe = of.entry(of_state)?;
        // Extra bits come in this order: offset, match length, literals,
        // then the three state updates. An offset code is at most 28:
        // tables are read with that bound.
        let of_code = ofe.value >> 24;
        let ml_bits = mle.value >> 24;
        let ll_bits = lle.value >> 24;
        let updates = u32::from(lle.bits)
            .wrapping_add(u32::from(mle.bits))
            .wrapping_add(u32::from(ofe.bits));
        br.reload(stream);
        let offset_value = state_of(1u64.wrapping_shl(of_code).wrapping_add(br.read(of_code)));
        let match_len = state_of(u64::from(mle.value & 0xFF_FFFF).wrapping_add(br.read(ml_bits)));
        // The reload holds 56 bits: the rest, at most 16 + 26, may need
        // another.
        if of_code
            .wrapping_add(ml_bits)
            .wrapping_add(ll_bits)
            .wrapping_add(updates)
            > 56
        {
            br.reload(stream);
        }
        let lit_len = state_of(u64::from(lle.value & 0xFF_FFFF).wrapping_add(br.read(ll_bits)));
        let offset = resolve_offset(reps, offset_value, lit_len)?;
        if lit_len > literals_len.saturating_sub(lit) {
            return Err(corrupt("sequence past the literals"));
        }
        out.room(lit_len.saturating_add(match_len))?;
        out.wild_copy(literals, lit, lit_len)?;
        lit = lit.wrapping_add(lit_len);
        // 1..=op reaches back into the output; 0 wraps past it too.
        if offset.wrapping_sub(1) >= out.op {
            return Err(corrupt("match offset before the output"));
        }
        out.copy_match(offset, match_len)?;
        if i.wrapping_add(1) < count {
            // States update in this order: literals, match length, offset.
            ll_state = usize::from(lle.base).wrapping_add(state_of(br.read(u32::from(lle.bits))));
            ml_state = usize::from(mle.base).wrapping_add(state_of(br.read(u32::from(mle.bits))));
            of_state = usize::from(ofe.base).wrapping_add(state_of(br.read(u32::from(ofe.bits))));
        }
    }
    if !br.finished() {
        return Err(corrupt("sequence stream not read to its end"));
    }
    let last = literals_len.saturating_sub(lit);
    out.room(last)?;
    out.wild_copy(literals, lit, last)
}

/// The match offset an offset value stands for, updating the repeat
/// offsets (RFC 8878 §3.1.1.5). Values 1-3 are repeat offsets, shifted by
/// one when the sequence has no literals; larger values are `value - 3`.
#[inline(always)]
pub(super) fn resolve_offset(reps: &mut [usize; 3], value: usize, lit_len: usize) -> Result<usize> {
    if value > 3 {
        let offset = value.wrapping_sub(3);
        *reps = [offset, reps[0], reps[1]];
        return Ok(offset);
    }
    let index = if lit_len == 0 {
        value
    } else {
        value.wrapping_sub(1)
    };
    let offset = match index {
        0 => return Ok(reps[0]),
        1 => reps[1],
        2 => reps[2],
        _ => reps[0]
            .checked_sub(1)
            .ok_or_else(|| corrupt("repeat offset underflow"))?,
    };
    *reps = if index == 1 {
        [offset, reps[0], reps[2]]
    } else {
        [offset, reps[0], reps[1]]
    };
    Ok(offset)
}
