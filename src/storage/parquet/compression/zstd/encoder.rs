//! ZSTD page compression (#692): an encoder of our own, the mirror of
//! [`super::decompress`], replacing the `ruzstd` crate's `Fastest` level,
//! which wrote 1.6x libzstd level 1.
//!
//! One single-segment frame (window = page, so a match reaches back across
//! block borders), one block per 128 KiB. A block is raw literals plus
//! sequences coded with the predefined FSE tables (RFC 8878 §3.1.1.3.2.2),
//! found by a greedy hash-table match finder with repeat offsets. A block
//! that would not shrink is stored raw, or as an RLE block when it is one
//! repeated byte, so a frame is never much larger than its input.
//!
//! An FSE encoder is the decoding table run backwards: to code a symbol
//! when the next state is `y`, the previous state is the one for that
//! symbol whose `[base, base + 2^bits)` window holds `y`, and the low
//! `bits` bits of `y - base` are written. Sequences are written last to
//! first so the decoder, reading the stream backwards, meets them in order.

use std::sync::OnceLock;

use self::literals::{normalize, write_counts};
use super::frame::{resolve_offset, LL_CODES, MAX_BLOCK, ML_CODES};
use super::fse::{Table, LL_DEFAULT, ML_DEFAULT, OF_DEFAULT};

mod literals;

/// The shortest match worth a sequence.
const MIN_MATCH: usize = 4;
/// Match offsets stay under the predefined offset table's 2^28 reach.
const MAX_OFFSET: usize = 1 << 28;
/// Hash table size bounds, in bits: small inputs get small tables.
const MIN_HASH_BITS: u32 = 8;
const MAX_HASH_BITS: u32 = 16;
/// An empty hash slot.
const EMPTY: u32 = u32::MAX;

/// One literal run and the match after it; `offset_value` is the coded
/// offset (1-3 a repeat offset, else `offset + 3`).
struct Sequence {
    lit_len: usize,
    match_len: usize,
    offset_value: usize,
}

/// How a symbol is coded: the previous state for each next state `y`.
#[derive(Clone, Copy, Default)]
struct Step {
    state: u16,
    bits: u8,
    base: u16,
}

/// A predefined table, inverted for encoding.
struct EncodeTable {
    log: u32,
    /// `steps[symbol][y]`.
    steps: Vec<Vec<Step>>,
    /// The first state that decodes each symbol: where a stream may start.
    start: Vec<usize>,
}

impl EncodeTable {
    /// The one-symbol table of an RLE field: no state, no bits.
    fn rle(symbol: usize) -> EncodeTable {
        EncodeTable {
            log: 0,
            steps: vec![vec![Step::default()]; symbol.saturating_add(1)],
            start: vec![0; symbol.saturating_add(1)],
        }
    }

    fn new(norm: &[i16], log: u32) -> Option<EncodeTable> {
        let table = Table::build(norm, log).ok()?;
        let size = 1usize.checked_shl(log)?;
        let mut steps = vec![vec![Step::default(); size]; norm.len()];
        let mut start = vec![usize::MAX; norm.len()];
        for state in 0..size {
            let entry = table.entry(state).ok()?;
            let first = start.get_mut(usize::from(entry.symbol))?;
            *first = (*first).min(state);
            let span = 1usize.checked_shl(u32::from(entry.bits))?;
            let row = steps.get_mut(usize::from(entry.symbol))?;
            for y in usize::from(entry.base)..usize::from(entry.base).checked_add(span)? {
                *row.get_mut(y)? = Step {
                    state: u16::try_from(state).ok()?,
                    bits: entry.bits,
                    base: entry.base,
                };
            }
        }
        Some(EncodeTable { log, steps, start })
    }

    fn any_state(&self, symbol: usize) -> usize {
        self.start.get(symbol).copied().unwrap_or(0)
    }

    fn step(&self, symbol: usize, y: usize) -> Step {
        self.steps
            .get(symbol)
            .and_then(|row| row.get(y))
            .copied()
            .unwrap_or_default()
    }
}

struct Tables {
    ll: EncodeTable,
    of: EncodeTable,
    ml: EncodeTable,
}

fn tables() -> Option<&'static Tables> {
    static TABLES: OnceLock<Option<Tables>> = OnceLock::new();
    TABLES
        .get_or_init(|| {
            Some(Tables {
                ll: EncodeTable::new(&LL_DEFAULT, 6)?,
                of: EncodeTable::new(&OF_DEFAULT, 5)?,
                ml: EncodeTable::new(&ML_DEFAULT, 6)?,
            })
        })
        .as_ref()
}

/// A backward-read bitstream writer: bits fill from the low end of byte 0,
/// and [`Self::finish`] adds the end marker the reader looks for.
#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    nbits: u32,
}

impl BitWriter {
    /// Appends the low `n` bits (`n` <= 32) of `value`.
    fn put(&mut self, value: u64, n: u32) {
        let mask = 1u64.wrapping_shl(n).wrapping_sub(1);
        self.acc |= (value & mask).wrapping_shl(self.nbits);
        self.nbits = self.nbits.wrapping_add(n);
        while self.nbits >= 8 {
            self.out.push(u8::try_from(self.acc & 0xFF).unwrap_or(0));
            self.acc >>= 8;
            self.nbits = self.nbits.wrapping_sub(8);
        }
    }

    /// The bytes written, the last one padded with zeros.
    fn finish_plain(mut self) -> Vec<u8> {
        if self.nbits > 0 {
            self.out.push(u8::try_from(self.acc & 0xFF).unwrap_or(0));
        }
        self.out
    }

    /// As [`Self::finish_plain`], after the end marker bit the reader
    /// looks for.
    fn finish(mut self) -> Vec<u8> {
        self.put(1, 1);
        self.finish_plain()
    }
}

/// Compress `input` into one ZSTD frame.
pub(crate) fn compress(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity((input.len() >> 1).saturating_add(32));
    write_frame_header(&mut out, input.len());
    let mut finder = Finder::new(input.len());
    let mut reps = [1usize, 4, 8];
    let mut start = 0usize;
    loop {
        let end = start.saturating_add(MAX_BLOCK).min(input.len());
        let last = end == input.len();
        write_block(&mut out, input, start, end, last, &mut finder, &mut reps);
        if last {
            return out;
        }
        start = end;
    }
}

/// A single-segment frame header carrying the content size.
fn write_frame_header(out: &mut Vec<u8>, size: usize) {
    out.extend_from_slice(&[0x28, 0xB5, 0x2F, 0xFD]);
    let size = u64::try_from(size).unwrap_or(u64::MAX);
    if size < 256 {
        out.push(0x20);
        out.push(u8::try_from(size).unwrap_or(0));
    } else if size < 65_536 + 256 {
        out.push(0x20 | 0x40);
        out.extend_from_slice(
            &u16::try_from(size.wrapping_sub(256))
                .unwrap_or(0)
                .to_le_bytes(),
        );
    } else if let Ok(size) = u32::try_from(size) {
        out.push(0x20 | 0x80);
        out.extend_from_slice(&size.to_le_bytes());
    } else {
        out.push(0x20 | 0xC0);
        out.extend_from_slice(&size.to_le_bytes());
    }
}

fn block_header(out: &mut Vec<u8>, last: bool, kind: u32, size: usize) {
    let h =
        u32::from(last) | kind.wrapping_shl(1) | u32::try_from(size).unwrap_or(0).wrapping_shl(3);
    out.extend_from_slice(h.to_le_bytes().get(..3).unwrap_or_default());
}

fn write_block(
    out: &mut Vec<u8>,
    input: &[u8],
    start: usize,
    end: usize,
    last: bool,
    finder: &mut Finder,
    reps: &mut [usize; 3],
) {
    let block = input.get(start..end).unwrap_or_default();
    if let Some(byte) = literals::run_byte(block) {
        block_header(out, last, 1, block.len());
        out.push(byte);
        finder.skip(input, start, end);
        return;
    }
    let saved = *reps;
    let body = tables().and_then(|t| compressed_body(t, input, start, end, finder, reps));
    match body {
        Some(body) if body.len() < block.len() => {
            block_header(out, last, 2, body.len());
            out.extend_from_slice(&body);
        }
        _ => {
            *reps = saved;
            block_header(out, last, 0, block.len());
            out.extend_from_slice(block);
        }
    }
}

/// The literals and sequences sections of one compressed block.
fn compressed_body(
    tables: &Tables,
    input: &[u8],
    start: usize,
    end: usize,
    finder: &mut Finder,
    reps: &mut [usize; 3],
) -> Option<Vec<u8>> {
    let mut sequences = Vec::new();
    let mut literals = Vec::new();
    finder.find(input, start, end, reps, &mut sequences, &mut literals);
    let mut body = Vec::with_capacity((literals.len() >> 1).saturating_add(64));
    literals::write(&mut body, &literals);
    write_sequences(&mut body, tables, &sequences)?;
    Some(body)
}

/// The code of `value` in a baseline table, and its extra bits' value.
fn code_of(codes: &[(u32, u32)], value: u32) -> (usize, u32, u32) {
    let code = codes
        .partition_point(|&(base, _)| base <= value)
        .saturating_sub(1);
    let (base, bits) = codes.get(code).copied().unwrap_or_default();
    (code, value.wrapping_sub(base), bits)
}

/// One sequence's three codes, each with its extra bits' value and width.
struct Coded {
    ll: (usize, u32, u32),
    of: (usize, u32, u32),
    ml: (usize, u32, u32),
}

/// How one of the three sequence fields is coded.
enum Mode {
    /// The predefined table (mode 0).
    Predefined,
    /// One symbol throughout (mode 1): no state, no bits.
    Rle(u8, EncodeTable),
    /// A table of its own, described in the block (mode 2).
    Custom(Vec<u8>, EncodeTable),
}

impl Mode {
    fn id(&self) -> u8 {
        match self {
            Mode::Predefined => 0,
            Mode::Rle(..) => 1,
            Mode::Custom(..) => 2,
        }
    }

    /// The mode's own table; the predefined one is the caller's.
    fn table(&self) -> Option<&EncodeTable> {
        match self {
            Mode::Predefined => None,
            Mode::Rle(_, t) | Mode::Custom(_, t) => Some(t),
        }
    }

    fn description(&self, out: &mut Vec<u8>) {
        match self {
            Mode::Predefined => {}
            Mode::Rle(symbol, _) => out.push(*symbol),
            Mode::Custom(desc, _) => out.extend_from_slice(desc),
        }
    }
}

/// The cheapest way to code the symbols with histogram `hist`: the
/// predefined table (`default`, accuracy `default_log`), a single repeated
/// symbol, or a table fitted to them (accuracy at most `max_log`).
fn choose_mode(
    hist: &[u32],
    predefined: &EncodeTable,
    default: &[i16],
    max_log: u32,
    count: usize,
) -> Option<Mode> {
    let distinct = hist.iter().filter(|&&h| h > 0).count();
    if distinct == 1 {
        let symbol = hist.iter().position(|&h| h > 0)?;
        return Some(Mode::Rle(
            u8::try_from(symbol).ok()?,
            EncodeTable::rle(symbol),
        ));
    }
    let log = count
        .ilog2()
        .saturating_sub(1)
        .max(distinct.next_power_of_two().ilog2())
        .clamp(5, max_log);
    let custom = normalize(
        hist,
        log,
        1usize
            .checked_shl(log)?
            .saturating_sub(distinct.saturating_sub(1)),
    )
    .and_then(|norm| {
        let desc = write_counts(&norm, log)?;
        let table = EncodeTable::new(&norm, log)?;
        let bits = cost_bits(hist, &norm, log)
            .saturating_add(u64::try_from(desc.len()).ok()?.saturating_mul(8));
        Some((bits, desc, table))
    });
    let default_bits = cost_bits(hist, default, predefined.log);
    match custom {
        Some((bits, desc, table)) if bits < default_bits => Some(Mode::Custom(desc, table)),
        _ => Some(Mode::Predefined),
    }
}

/// About how many bits `hist`'s symbols take under the table `norm` of
/// accuracy `log`: `log - floor(log2(count))` each.
fn cost_bits(hist: &[u32], norm: &[i16], log: u32) -> u64 {
    hist.iter()
        .zip(norm)
        .map(|(&h, &n)| {
            let n = u64::try_from(n.max(1)).unwrap_or(1);
            u64::from(h).saturating_mul(u64::from(log.saturating_sub(n.ilog2())))
        })
        .sum()
}

fn write_sequences(body: &mut Vec<u8>, t: &Tables, sequences: &[Sequence]) -> Option<()> {
    let count = sequences.len();
    if count == 0 {
        body.push(0);
        return Some(());
    }
    if count < 128 {
        body.push(u8::try_from(count).ok()?);
    } else if count < 0x7F00 {
        body.push(u8::try_from((count >> 8).wrapping_add(128)).ok()?);
        body.push(u8::try_from(count & 0xFF).ok()?);
    } else {
        body.push(255);
        body.extend_from_slice(
            &u16::try_from(count.wrapping_sub(0x7F00))
                .ok()?
                .to_le_bytes(),
        );
    }
    let mut coded = Vec::with_capacity(count);
    let (mut ll_hist, mut of_hist, mut ml_hist) = ([0u32; 36], [0u32; 29], [0u32; 53]);
    for seq in sequences {
        let ll = code_of(&LL_CODES, u32::try_from(seq.lit_len).ok()?);
        let ml = code_of(&ML_CODES, u32::try_from(seq.match_len).ok()?);
        let of_value = u32::try_from(seq.offset_value).ok()?;
        let of_code = 31u32.wrapping_sub(of_value.leading_zeros());
        let of = (
            usize::try_from(of_code).ok()?,
            of_value.wrapping_sub(1u32.wrapping_shl(of_code)),
            of_code,
        );
        for (hist, code) in [
            (ll_hist.as_mut_slice(), ll.0),
            (of_hist.as_mut_slice(), of.0),
            (ml_hist.as_mut_slice(), ml.0),
        ] {
            let h = hist.get_mut(code)?;
            *h = h.saturating_add(1);
        }
        coded.push(Coded { ll, of, ml });
    }
    let ll_mode = choose_mode(&ll_hist, &t.ll, &LL_DEFAULT, 9, count)?;
    let of_mode = choose_mode(&of_hist, &t.of, &OF_DEFAULT, 8, count)?;
    let ml_mode = choose_mode(&ml_hist, &t.ml, &ML_DEFAULT, 9, count)?;
    body.push(ll_mode.id() << 6 | of_mode.id() << 4 | ml_mode.id() << 2);
    ll_mode.description(body);
    of_mode.description(body);
    ml_mode.description(body);
    let ll_t = ll_mode.table().unwrap_or(&t.ll);
    let of_t = of_mode.table().unwrap_or(&t.of);
    let ml_t = ml_mode.table().unwrap_or(&t.ml);
    let mut bw = BitWriter::default();
    // (ll, of, ml) states of the sequence after the one being written.
    let mut states: Option<(usize, usize, usize)> = None;
    for c in coded.iter().rev() {
        let next = match states {
            None => (
                ll_t.any_state(c.ll.0),
                of_t.any_state(c.of.0),
                ml_t.any_state(c.ml.0),
            ),
            Some((ll_y, of_y, ml_y)) => {
                // The decoder reads these updates ll, ml, of: write of, ml, ll.
                let of = of_t.step(c.of.0, of_y);
                let ml = ml_t.step(c.ml.0, ml_y);
                let ll = ll_t.step(c.ll.0, ll_y);
                for (s, y) in [(of, of_y), (ml, ml_y), (ll, ll_y)] {
                    bw.put(
                        u64::try_from(y.wrapping_sub(usize::from(s.base))).ok()?,
                        u32::from(s.bits),
                    );
                }
                (
                    usize::from(ll.state),
                    usize::from(of.state),
                    usize::from(ml.state),
                )
            }
        };
        // Extra bits are read offset, match length, literals: write the reverse.
        bw.put(u64::from(c.ll.1), c.ll.2);
        bw.put(u64::from(c.ml.1), c.ml.2);
        bw.put(u64::from(c.of.1), c.of.2);
        states = Some(next);
    }
    // Initial states are read ll, of, ml.
    let (ll, of, ml) = states?;
    bw.put(u64::try_from(ml).ok()?, ml_t.log);
    bw.put(u64::try_from(of).ok()?, of_t.log);
    bw.put(u64::try_from(ll).ok()?, ll_t.log);
    body.extend_from_slice(&bw.finish());
    Some(())
}

/// The match finder: a hash table of 5-byte sequences over the whole page,
/// carried across blocks so a match may reach into an earlier one.
struct Finder {
    table: Vec<u32>,
    bits: u32,
}

impl Finder {
    fn new(len: usize) -> Finder {
        let needed = usize::BITS.wrapping_sub(len.leading_zeros());
        let bits = needed.clamp(MIN_HASH_BITS, MAX_HASH_BITS);
        Finder {
            table: vec![EMPTY; 1usize.wrapping_shl(bits)],
            bits,
        }
    }

    fn slot(&self, input: &[u8], at: usize) -> Option<usize> {
        let word = read_u64(input, at)?;
        let h = (word.wrapping_shl(24)).wrapping_mul(0x9E37_79B1_85EB_CA87)
            >> 64u32.wrapping_sub(self.bits);
        usize::try_from(h).ok()
    }

    /// Records position `at`; returns the position it replaced.
    fn insert(&mut self, input: &[u8], at: usize) -> Option<usize> {
        let slot = self.slot(input, at)?;
        let new = u32::try_from(at).unwrap_or(EMPTY);
        let old = std::mem::replace(self.table.get_mut(slot)?, new);
        (old != EMPTY).then(|| usize::try_from(old).unwrap_or(0))
    }

    /// Hashes a block that was emitted without a search.
    fn skip(&mut self, input: &[u8], start: usize, end: usize) {
        for at in start..end {
            self.insert(input, at);
        }
    }

    fn find(
        &mut self,
        input: &[u8],
        start: usize,
        end: usize,
        reps: &mut [usize; 3],
        sequences: &mut Vec<Sequence>,
        literals: &mut Vec<u8>,
    ) {
        let limit = end.saturating_sub(8);
        let mut anchor = start;
        let mut ip = start;
        while ip < limit {
            let candidate = self.insert(input, ip);
            let lit_len = ip.wrapping_sub(anchor);
            let mut best = (0usize, 0usize);
            for offset in rep_offsets(reps, lit_len) {
                best = longer(best, input, ip, end, offset);
            }
            if let Some(c) = candidate {
                best = longer(best, input, ip, end, ip.wrapping_sub(c));
            }
            let (mut len, offset) = best;
            if len < MIN_MATCH {
                // Skip faster the longer nothing matches.
                ip = ip.saturating_add(1).saturating_add(lit_len >> 5);
                continue;
            }
            // Pull the match back over equal literals.
            let back = equal_before(input, anchor, ip, offset);
            let from = ip.wrapping_sub(back);
            len = len.saturating_add(back);
            let lit_len = from.wrapping_sub(anchor);
            let offset_value = offset_value(reps, lit_len, offset);
            literals.extend_from_slice(input.get(anchor..from).unwrap_or_default());
            sequences.push(Sequence {
                lit_len,
                match_len: len,
                offset_value,
            });
            // The value is valid by construction.
            resolve_offset(reps, offset_value, lit_len).ok();
            ip = from.saturating_add(len);
            anchor = ip;
            if let Some(before) = ip.checked_sub(2) {
                self.insert(input, before);
            }
        }
        literals.extend_from_slice(input.get(anchor..end).unwrap_or_default());
    }
}

/// The offsets that the coded values 1, 2 and 3 stand for.
fn rep_offsets(reps: &[usize; 3], lit_len: usize) -> [usize; 3] {
    if lit_len > 0 {
        *reps
    } else {
        [reps[1], reps[2], reps[0].saturating_sub(1)]
    }
}

/// The coded value of `offset`: a repeat code when it is one, else
/// `offset + 3`.
fn offset_value(reps: &[usize; 3], lit_len: usize, offset: usize) -> usize {
    rep_offsets(reps, lit_len)
        .iter()
        .position(|&r| r == offset)
        .map_or_else(|| offset.saturating_add(3), |i| i.wrapping_add(1))
}

/// `best`, or the match at `offset` back from `ip` when it is longer.
fn longer(
    best: (usize, usize),
    input: &[u8],
    ip: usize,
    end: usize,
    offset: usize,
) -> (usize, usize) {
    if !(1..=ip.min(MAX_OFFSET.wrapping_sub(1))).contains(&offset) {
        return best;
    }
    let len = match_len(input, ip.wrapping_sub(offset), ip, end);
    if len > best.0 {
        (len, offset)
    } else {
        best
    }
}

/// How many bytes before `ip` (not before `anchor`) equal those `offset`
/// before them.
fn equal_before(input: &[u8], anchor: usize, ip: usize, offset: usize) -> usize {
    let room = ip.wrapping_sub(anchor).min(ip.saturating_sub(offset));
    (1..=room)
        .take_while(|&k| {
            input.get(ip.wrapping_sub(k)) == input.get(ip.wrapping_sub(offset).wrapping_sub(k))
        })
        .count()
}

/// How many bytes from `earlier` repeat those from `at`, up to `end`.
fn match_len(input: &[u8], earlier: usize, at: usize, end: usize) -> usize {
    let a = input.get(earlier..).unwrap_or_default();
    let b = input.get(at..end).unwrap_or_default();
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

fn read_u64(input: &[u8], at: usize) -> Option<u64> {
    let bytes: [u8; 8] = input.get(at..at.checked_add(8)?)?.try_into().ok()?;
    Some(u64::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests;
