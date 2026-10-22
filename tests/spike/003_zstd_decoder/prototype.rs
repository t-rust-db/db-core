//! SPIKE #697: a zstd decoder for Parquet pages, from
//! RFC 8878, that writes straight into an output buffer of the page's
//! known size. No ring buffer, no `Read` trait, no dictionary support, no
//! checksum verification (the checksum is skipped).

#![allow(
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::cast_lossless,
    clippy::arithmetic_side_effects,
    clippy::needless_late_init,
    clippy::needless_range_loop,
    clippy::same_item_push,
    missing_docs
)]

type Result<T> = std::result::Result<T, String>;

fn err<T>(why: &str) -> Result<T> {
    Err(why.to_string())
}

// --- bit readers -----------------------------------------------------------

/// Forward little-endian bit reader, for FSE table descriptions.
struct Fwd<'a> {
    data: &'a [u8],
    bit: usize,
}

impl Fwd<'_> {
    fn peek(&self, n: u32) -> u32 {
        let mut v = 0u64;
        let byte = self.bit / 8;
        for i in 0..5 {
            v |= u64::from(*self.data.get(byte + i).unwrap_or(&0)) << (8 * i);
        }
        ((v >> (self.bit % 8)) & ((1u64 << n) - 1)) as u32
    }

    fn skip(&mut self, n: u32) {
        self.bit += n as usize;
    }

    fn bytes_used(&self) -> usize {
        self.bit.div_ceil(8)
    }
}

/// Backward bit reader (zstd's FSE and Huffman streams): starts below the
/// end marker of the last byte and reads towards byte 0; past byte 0 it
/// reads zeros, and `overflowed` says so.
struct Rev<'a> {
    data: &'a [u8],
    pos: i64,
}

impl<'a> Rev<'a> {
    fn new(data: &'a [u8]) -> Result<Rev<'a>> {
        let Some(&last) = data.last() else {
            return err("empty bitstream");
        };
        if last == 0 {
            return err("bitstream without end marker");
        }
        let pos = (data.len() * 8) as i64 - (last.leading_zeros() as i64 + 1);
        Ok(Rev { data, pos })
    }

    #[inline(always)]
    fn load(&self, byte: usize) -> u64 {
        if let Some(b) = self.data.get(byte..byte + 8) {
            u64::from_le_bytes(b.try_into().unwrap_or([0; 8]))
        } else {
            let mut buf = [0u8; 8];
            let end = self.data.len().min(byte + 8);
            if byte < end {
                buf[..end - byte].copy_from_slice(&self.data[byte..end]);
            }
            u64::from_le_bytes(buf)
        }
    }

    /// The next `n` bits (n <= 56), without consuming them.
    #[inline(always)]
    fn peek(&self, n: u32) -> u64 {
        if n == 0 {
            return 0;
        }
        let mask = (1u64 << n) - 1;
        let start = self.pos - i64::from(n);
        if start >= 0 {
            (self.load((start / 8) as usize) >> (start % 8)) & mask
        } else if self.pos <= 0 {
            0
        } else {
            let have = self.pos as u32;
            (self.load(0) & ((1u64 << have) - 1)) << (n - have)
        }
    }

    #[inline(always)]
    fn read(&mut self, n: u32) -> u64 {
        let v = self.peek(n);
        self.pos -= i64::from(n);
        v
    }

    fn overflowed(&self) -> bool {
        self.pos < 0
    }
}

// --- FSE -------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
struct FseEntry {
    symbol: u8,
    nb_bits: u8,
    base: u16,
}

#[derive(Clone, Default)]
struct Fse {
    log: u32,
    table: Vec<FseEntry>,
}

impl Fse {
    /// The decoding table of normalized counts `norm` at `log`.
    fn build(norm: &[i16], log: u32) -> Result<Fse> {
        let size = 1usize << log;
        let mut table = vec![FseEntry::default(); size];
        let mut next = vec![0u32; norm.len()];
        let mut high = size - 1;
        for (s, &n) in norm.iter().enumerate() {
            if n == -1 {
                table[high].symbol = s as u8;
                high = high.wrapping_sub(1);
                next[s] = 1;
            } else {
                next[s] = n.max(0) as u32;
            }
        }
        let step = (size >> 1) + (size >> 3) + 3;
        let mask = size - 1;
        let mut position = 0usize;
        for (s, &n) in norm.iter().enumerate() {
            for _ in 0..n.max(0) {
                table[position].symbol = s as u8;
                loop {
                    position = (position + step) & mask;
                    if position <= high || high == usize::MAX {
                        break;
                    }
                }
            }
        }
        if position != 0 {
            return err("FSE spread did not return to 0");
        }
        for entry in &mut table {
            let s = entry.symbol as usize;
            let state = next[s];
            next[s] += 1;
            let nb = log - (31 - state.leading_zeros());
            entry.nb_bits = nb as u8;
            entry.base = ((state << nb) as usize - size) as u16;
        }
        Ok(Fse { log, table })
    }

    /// Reads a table description at the start of `data`; returns the
    /// table and the bytes it used.
    fn read(data: &[u8], max_log: u32, max_symbol: usize) -> Result<(Fse, usize)> {
        let mut br = Fwd { data, bit: 0 };
        let log = br.peek(4) + 5;
        br.skip(4);
        if log > max_log {
            return err("FSE accuracy log too large");
        }
        let mut remaining: i32 = (1 << log) + 1;
        let mut threshold: i32 = 1 << log;
        let mut nb_bits = log + 1;
        let mut norm: Vec<i16> = Vec::new();
        while remaining > 1 && norm.len() <= max_symbol {
            let max = (2 * threshold - 1) - remaining;
            let bits = br.peek(nb_bits) as i32;
            let mut count;
            if (bits & (threshold - 1)) < max {
                count = bits & (threshold - 1);
                br.skip(nb_bits - 1);
            } else {
                count = bits & (2 * threshold - 1);
                if count >= threshold {
                    count -= max;
                }
                br.skip(nb_bits);
            }
            count -= 1;
            remaining -= count.abs();
            norm.push(count as i16);
            if count == 0 {
                loop {
                    let repeat = br.peek(2);
                    br.skip(2);
                    for _ in 0..repeat {
                        norm.push(0);
                    }
                    if repeat != 3 {
                        break;
                    }
                }
            }
            while remaining < threshold && threshold > 1 {
                nb_bits -= 1;
                threshold >>= 1;
            }
        }
        if remaining != 1 || norm.len() > max_symbol + 1 {
            return err("bad FSE table description");
        }
        Ok((Fse::build(&norm, log)?, br.bytes_used()))
    }

    fn rle(symbol: u8) -> Fse {
        Fse {
            log: 0,
            table: vec![FseEntry {
                symbol,
                nb_bits: 0,
                base: 0,
            }],
        }
    }
}

const LL_DEFAULT: [i16; 36] = [
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];
const ML_DEFAULT: [i16; 53] = [
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];
const OF_DEFAULT: [i16; 29] = [
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
];

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

// --- Huffman ---------------------------------------------------------------

#[derive(Clone, Default)]
struct Huffman {
    max_bits: u32,
    /// (symbol, bits) per `max_bits`-bit prefix.
    table: Vec<(u8, u8)>,
}

impl Huffman {
    /// Reads a tree description; returns it and the bytes it used.
    fn read(data: &[u8]) -> Result<(Huffman, usize)> {
        let Some(&header) = data.first() else {
            return err("missing Huffman header");
        };
        let mut weights: Vec<u8> = Vec::with_capacity(256);
        let used;
        if header < 128 {
            let size = header as usize;
            let body = data.get(1..1 + size).ok_or("short Huffman weights")?;
            let (fse, table_bytes) = Fse::read(body, 6, 255)?;
            let mut br = Rev::new(&body[table_bytes..])?;
            let mut s1 = br.read(fse.log) as usize;
            let mut s2 = br.read(fse.log) as usize;
            loop {
                let e = fse.table[s1];
                weights.push(e.symbol);
                s1 = e.base as usize + br.read(u32::from(e.nb_bits)) as usize;
                if br.overflowed() {
                    weights.push(fse.table[s2].symbol);
                    break;
                }
                let e = fse.table[s2];
                weights.push(e.symbol);
                s2 = e.base as usize + br.read(u32::from(e.nb_bits)) as usize;
                if br.overflowed() {
                    weights.push(fse.table[s1].symbol);
                    break;
                }
                if weights.len() > 255 {
                    return err("too many Huffman weights");
                }
            }
            used = 1 + size;
        } else {
            let count = (header - 127) as usize;
            let body = data
                .get(1..1 + count.div_ceil(2))
                .ok_or("short direct Huffman weights")?;
            for i in 0..count {
                let b = body[i / 2];
                weights.push(if i % 2 == 0 { b >> 4 } else { b & 0xF });
            }
            used = 1 + count.div_ceil(2);
        }
        // The last weight is implied: the total must reach a power of two.
        let total: u32 = weights
            .iter()
            .filter(|&&w| w > 0)
            .map(|&w| 1u32 << (w - 1))
            .sum();
        if total == 0 {
            return err("empty Huffman tree");
        }
        let max_bits = 32 - total.leading_zeros();
        let left = (1u32 << max_bits) - total;
        if !left.is_power_of_two() {
            return err("Huffman weights do not complete a tree");
        }
        weights.push((left.trailing_zeros() + 1) as u8);
        if max_bits > 11 {
            return err("Huffman code too long");
        }
        let mut rank_start = [0u32; 13];
        let mut next = 0u32;
        for w in 1..=max_bits as usize {
            rank_start[w] = next;
            let n = weights.iter().filter(|&&x| x as usize == w).count() as u32;
            next += n << (w - 1);
        }
        let mut table = vec![(0u8, 0u8); 1 << max_bits];
        for (symbol, &w) in weights.iter().enumerate() {
            if w == 0 {
                continue;
            }
            let length = (1u32 << w) >> 1;
            let start = rank_start[w as usize];
            let bits = (max_bits + 1 - u32::from(w)) as u8;
            for slot in &mut table[start as usize..(start + length) as usize] {
                *slot = (symbol as u8, bits);
            }
            rank_start[w as usize] += length;
        }
        Ok((Huffman { max_bits, table }, used))
    }

    /// Decodes `out.len()` symbols from one stream.
    fn decode_stream(&self, stream: &[u8], out: &mut [u8]) -> Result<()> {
        let mut br = Rev::new(stream)?;
        for slot in out.iter_mut() {
            let (symbol, bits) = self.table[br.peek(self.max_bits) as usize];
            *slot = symbol;
            br.pos -= i64::from(bits);
        }
        if br.pos != 0 {
            return err("Huffman stream not fully consumed");
        }
        Ok(())
    }
}

// --- frames and blocks -----------------------------------------------------

/// What a frame carries from block to block.
struct State {
    huffman: Option<Huffman>,
    ll: Option<Fse>,
    of: Option<Fse>,
    ml: Option<Fse>,
    reps: [usize; 3],
    /// The block's literals, then `SLACK` bytes for 16-byte wild reads.
    literals: Vec<u8>,
    lit_len: usize,
}

/// Room past the end of the output and the literals for wild copies.
const SLACK: usize = 32;

/// The frame's output: `size` bytes plus `SLACK`, written at `op`. Every
/// copy is checked against `size`, so the slack only ever takes the
/// over-write of a wild copy's last chunk.
struct Out {
    buf: Vec<u8>,
    op: usize,
    size: usize,
}

impl Out {
    fn room(&self, n: usize) -> Result<()> {
        if n > self.size - self.op {
            return err("output longer than the page header says");
        }
        Ok(())
    }

    /// Copies `src[from..from + n]` to the output in 16-byte chunks; `src`
    /// has at least 16 readable bytes past `from + n`.
    #[inline(always)]
    fn wild_copy(&mut self, src: &[u8], from: usize, n: usize) {
        let mut i = 0;
        while i < n {
            let chunk: [u8; 16] = src[from + i..from + i + 16].try_into().unwrap_or([0; 16]);
            self.buf[self.op + i..self.op + i + 16].copy_from_slice(&chunk);
            i += 16;
        }
        self.op += n;
    }

    /// Copies `n` bytes from `offset` back in the output.
    #[inline(always)]
    fn copy_match(&mut self, offset: usize, n: usize) {
        let mut src = self.op - offset;
        if offset >= 16 {
            let mut i = 0;
            while i < n {
                let chunk: [u8; 16] = self.buf[src + i..src + i + 16]
                    .try_into()
                    .unwrap_or([0; 16]);
                self.buf[self.op + i..self.op + i + 16].copy_from_slice(&chunk);
                i += 16;
            }
            self.op += n;
        } else {
            for _ in 0..n {
                self.buf[self.op] = self.buf[src];
                self.op += 1;
                src += 1;
            }
        }
    }
}

/// One zstd frame (a Parquet page) decompressed to exactly
/// `uncompressed_size` bytes.
pub fn decompress(frame: &[u8], uncompressed_size: usize) -> Result<Vec<u8>> {
    if frame.get(..4) != Some(&[0x28, 0xB5, 0x2F, 0xFD]) {
        return err("not a zstd frame");
    }
    let fhd = *frame.get(4).ok_or("short frame header")?;
    let single_segment = fhd & 0x20 != 0;
    let checksum = fhd & 0x04 != 0;
    let dict_id_size = [0, 1, 2, 4][(fhd & 3) as usize];
    let fcs_size = match fhd >> 6 {
        0 => usize::from(single_segment),
        1 => 2,
        2 => 4,
        _ => 8,
    };
    if dict_id_size != 0 {
        return err("dictionaries are not supported");
    }
    let mut pos = 5 + usize::from(!single_segment) + fcs_size;
    let mut out = Out {
        buf: vec![0; uncompressed_size + SLACK],
        op: 0,
        size: uncompressed_size,
    };
    let mut state = State {
        huffman: None,
        ll: None,
        of: None,
        ml: None,
        reps: [1, 4, 8],
        literals: Vec::new(),
        lit_len: 0,
    };
    loop {
        let header = frame.get(pos..pos + 3).ok_or("short block header")?;
        let h = u32::from(header[0]) | u32::from(header[1]) << 8 | u32::from(header[2]) << 16;
        pos += 3;
        let last = h & 1 != 0;
        let size = (h >> 3) as usize;
        match (h >> 1) & 3 {
            0 => {
                let raw = frame.get(pos..pos + size).ok_or("short raw block")?;
                out.room(size)?;
                out.buf[out.op..out.op + size].copy_from_slice(raw);
                out.op += size;
                pos += size;
            }
            1 => {
                let b = *frame.get(pos).ok_or("short RLE block")?;
                out.room(size)?;
                out.buf[out.op..out.op + size].fill(b);
                out.op += size;
                pos += 1;
            }
            2 => {
                let block = frame.get(pos..pos + size).ok_or("short compressed block")?;
                compressed_block(block, &mut state, &mut out)?;
                pos += size;
            }
            _ => return err("reserved block type"),
        }
        if last {
            break;
        }
    }
    if checksum {
        pos += 4;
    }
    let _ = pos;
    if out.op != uncompressed_size {
        return err("decompressed size differs from the page header");
    }
    out.buf.truncate(uncompressed_size);
    Ok(out.buf)
}

fn compressed_block(block: &[u8], state: &mut State, out: &mut Out) -> Result<()> {
    let used = literals_section(block, state)?;
    state.lit_len = state.literals.len();
    state.literals.resize(state.lit_len + SLACK, 0);
    sequences_section(&block[used..], state, out)
}

/// Decodes the literals section into `state.literals`; returns its size.
fn literals_section(block: &[u8], state: &mut State) -> Result<usize> {
    let b0 = *block.first().ok_or("empty block")?;
    let kind = b0 & 3;
    let size_format = (b0 >> 2) & 3;
    let byte =
        |i: usize| -> Result<u32> { Ok(u32::from(*block.get(i).ok_or("short literals header")?)) };
    if kind < 2 {
        let (header, regenerated) = match size_format {
            0 | 2 => (1, (b0 >> 3) as usize),
            1 => (2, ((b0 >> 4) as u32 | byte(1)? << 4) as usize),
            _ => (
                3,
                ((b0 >> 4) as u32 | byte(1)? << 4 | byte(2)? << 12) as usize,
            ),
        };
        state.literals.clear();
        if kind == 0 {
            state.literals.extend_from_slice(
                block
                    .get(header..header + regenerated)
                    .ok_or("short raw literals")?,
            );
            return Ok(header + regenerated);
        }
        let b = *block.get(header).ok_or("short RLE literals")?;
        state.literals.resize(regenerated, b);
        return Ok(header + 1);
    }
    let (header, bits, streams) = match size_format {
        0 => (3, 10, 1),
        1 => (3, 10, 4),
        2 => (4, 14, 4),
        _ => (5, 18, 4),
    };
    let mut h: u64 = 0;
    for i in 0..header {
        h |= u64::from(*block.get(i).ok_or("short literals header")?) << (8 * i);
    }
    let mask = (1u64 << bits) - 1;
    let regenerated = ((h >> 4) & mask) as usize;
    let compressed = ((h >> (4 + bits)) & mask) as usize;
    let mut body = block
        .get(header..header + compressed)
        .ok_or("short compressed literals")?;
    if kind == 2 {
        let (huffman, used) = Huffman::read(body)?;
        state.huffman = Some(huffman);
        body = &body[used..];
    }
    let huffman = state
        .huffman
        .as_ref()
        .ok_or("treeless literals without a tree")?;
    state.literals.clear();
    state.literals.resize(regenerated, 0);
    if streams == 1 {
        huffman.decode_stream(body, &mut state.literals)?;
    } else {
        let jump = body.get(..6).ok_or("short jump table")?;
        let s1 = usize::from(u16::from_le_bytes([jump[0], jump[1]]));
        let s2 = usize::from(u16::from_le_bytes([jump[2], jump[3]]));
        let s3 = usize::from(u16::from_le_bytes([jump[4], jump[5]]));
        let rest = &body[6..];
        let (a, rest) = rest.split_at_checked(s1).ok_or("bad jump table")?;
        let (b, rest) = rest.split_at_checked(s2).ok_or("bad jump table")?;
        let (c, d) = rest.split_at_checked(s3).ok_or("bad jump table")?;
        let per = regenerated.div_ceil(4);
        let (o1, o) = state.literals.split_at_mut(per.min(regenerated));
        let (o2, o) = o.split_at_mut(per.min(o.len()));
        let (o3, o4) = o.split_at_mut(per.min(o.len()));
        huffman.decode_stream(a, o1)?;
        huffman.decode_stream(b, o2)?;
        huffman.decode_stream(c, o3)?;
        huffman.decode_stream(d, o4)?;
    }
    Ok(header + compressed)
}

/// Reads one of the three sequence tables per its mode; returns bytes used.
fn table(
    mode: u8,
    data: &[u8],
    slot: &mut Option<Fse>,
    default: &[i16],
    default_log: u32,
    max_log: u32,
) -> Result<usize> {
    match mode {
        0 => {
            *slot = Some(Fse::build(default, default_log)?);
            Ok(0)
        }
        1 => {
            *slot = Some(Fse::rle(*data.first().ok_or("short RLE table")?));
            Ok(1)
        }
        2 => {
            let (fse, used) = Fse::read(data, max_log, default.len() - 1)?;
            *slot = Some(fse);
            Ok(used)
        }
        _ => {
            if slot.is_none() {
                return err("repeat table without a previous one");
            }
            Ok(0)
        }
    }
}

fn sequences_section(data: &[u8], state: &mut State, out: &mut Out) -> Result<()> {
    let b0 = *data.first().ok_or("missing sequences header")?;
    let byte = |i: usize| -> Result<usize> {
        Ok(usize::from(*data.get(i).ok_or("short sequences header")?))
    };
    let (count, mut pos) = match b0 {
        0 => {
            out.room(state.lit_len)?;
            out.wild_copy(&state.literals, 0, state.lit_len);
            return Ok(());
        }
        1..=127 => (usize::from(b0), 1),
        128..=254 => (((usize::from(b0) - 128) << 8) + byte(1)?, 2),
        255 => (byte(1)? + (byte(2)? << 8) + 0x7F00, 3),
    };
    let modes = *data.get(pos).ok_or("missing compression modes")?;
    pos += 1;
    pos += table(modes >> 6, &data[pos..], &mut state.ll, &LL_DEFAULT, 6, 9)?;
    pos += table(
        (modes >> 4) & 3,
        &data[pos..],
        &mut state.of,
        &OF_DEFAULT,
        5,
        8,
    )?;
    pos += table(
        (modes >> 2) & 3,
        &data[pos..],
        &mut state.ml,
        &ML_DEFAULT,
        6,
        9,
    )?;
    let (Some(ll), Some(of), Some(ml)) = (&state.ll, &state.of, &state.ml) else {
        return err("missing sequence tables");
    };
    let mut br = Rev::new(&data[pos..])?;
    let mut ll_state = br.read(ll.log) as usize;
    let mut of_state = br.read(of.log) as usize;
    let mut ml_state = br.read(ml.log) as usize;
    let literals = &state.literals;
    let lit_len_total = state.lit_len;
    let mut lit = 0usize;
    let reps = &mut state.reps;
    for i in 0..count {
        let lle = ll.table[ll_state];
        let mle = ml.table[ml_state];
        let ofe = of.table[of_state];
        let of_code = u32::from(ofe.symbol);
        let offset_value = (1usize << of_code) + br.read(of_code) as usize;
        let (ml_base, ml_bits) = ML_CODES[usize::from(mle.symbol)];
        let match_len = (ml_base + br.read(ml_bits) as u32) as usize;
        let (ll_base, ll_bits) = LL_CODES[usize::from(lle.symbol)];
        let lit_len = (ll_base + br.read(ll_bits) as u32) as usize;
        let offset = if offset_value > 3 {
            let o = offset_value - 3;
            reps[2] = reps[1];
            reps[1] = reps[0];
            reps[0] = o;
            o
        } else {
            let index = if lit_len == 0 {
                offset_value
            } else {
                offset_value - 1
            };
            let o = match index {
                0 => reps[0],
                1 => reps[1],
                2 => reps[2],
                _ => reps[0] - 1,
            };
            if index > 0 {
                if index > 1 {
                    reps[2] = reps[1];
                }
                reps[1] = reps[0];
                reps[0] = o;
            }
            o
        };
        if lit_len > lit_len_total - lit {
            return err("literals overrun");
        }
        out.room(lit_len + match_len)?;
        out.wild_copy(literals, lit, lit_len);
        lit += lit_len;
        if offset == 0 || offset > out.op {
            return err("offset beyond the output");
        }
        out.copy_match(offset, match_len);
        if i + 1 < count {
            ll_state = lle.base as usize + br.read(u32::from(lle.nb_bits)) as usize;
            ml_state = mle.base as usize + br.read(u32::from(mle.nb_bits)) as usize;
            of_state = ofe.base as usize + br.read(u32::from(ofe.nb_bits)) as usize;
        }
    }
    let rest = lit_len_total - lit;
    out.room(rest)?;
    out.wild_copy(literals, lit, rest);
    Ok(())
}
