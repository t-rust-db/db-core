//! FSE (finite state entropy) decoding tables (RFC 8878 §4.1).

use super::bits::Forward;
use super::{corrupt, Result};

/// One state of a decoding table: the symbol it decodes, and how the next
/// state is found (`base` plus `bits` bits from the stream).
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Entry {
    pub(super) symbol: u8,
    pub(super) bits: u8,
    pub(super) base: u16,
}

/// A decoding table of `1 << log` states.
#[derive(Debug, Clone, Default)]
pub(super) struct Table {
    pub(super) log: u32,
    entries: Vec<Entry>,
}

impl Table {
    /// State `state`'s entry.
    #[inline(always)]
    pub(super) fn entry(&self, state: usize) -> Result<Entry> {
        self.entries
            .get(state)
            .copied()
            .ok_or_else(|| corrupt("FSE state outside its table"))
    }

    /// The one-symbol table of an RLE-mode sequence field.
    pub(super) fn rle(symbol: u8) -> Table {
        Table {
            log: 0,
            entries: vec![Entry {
                symbol,
                bits: 0,
                base: 0,
            }],
        }
    }

    /// The decoding table of normalized counts `norm` at accuracy `log`
    /// (a count of -1 is a "less than 1" probability).
    pub(super) fn build(norm: &[i16], log: u32) -> Result<Table> {
        let size = 1usize
            .checked_shl(log)
            .ok_or_else(|| corrupt("FSE accuracy log too large"))?;
        let mut entries = vec![Entry::default(); size];
        let mut next = vec![0u32; norm.len()];
        // Less-than-1 symbols take the highest states, one each.
        let mut high = i64::try_from(size).map_err(|_| corrupt("FSE table too large"))?;
        for ((symbol, &count), next) in norm.iter().enumerate().zip(next.iter_mut()) {
            let symbol = u8::try_from(symbol).map_err(|_| corrupt("FSE symbol too large"))?;
            if count == -1 {
                high = high.wrapping_sub(1);
                let slot = usize::try_from(high)
                    .ok()
                    .and_then(|h| entries.get_mut(h))
                    .ok_or_else(|| corrupt("FSE counts exceed the table"))?;
                slot.symbol = symbol;
                *next = 1;
            } else {
                *next = u32::try_from(count).map_err(|_| corrupt("bad FSE count"))?;
            }
        }
        // The other symbols are spread over the remaining states.
        let step = (size >> 1).wrapping_add(size >> 3).wrapping_add(3);
        let state_mask = size.wrapping_sub(1);
        let mut position = 0usize;
        for (symbol, &count) in norm.iter().enumerate() {
            for _ in 0..count.max(0) {
                let slot = entries
                    .get_mut(position)
                    .ok_or_else(|| corrupt("FSE spread outside the table"))?;
                slot.symbol = u8::try_from(symbol).map_err(|_| corrupt("FSE symbol too large"))?;
                loop {
                    position = position.wrapping_add(step) & state_mask;
                    if i64::try_from(position).unwrap_or(i64::MAX) < high {
                        break;
                    }
                }
            }
        }
        if position != 0 {
            return Err(corrupt("FSE counts do not fill the table"));
        }
        for entry in &mut entries {
            let next = next
                .get_mut(usize::from(entry.symbol))
                .ok_or_else(|| corrupt("FSE symbol without a count"))?;
            let state = *next;
            *next = state.wrapping_add(1);
            if state == 0 {
                return Err(corrupt("FSE state of a zero-count symbol"));
            }
            let bits = log
                .checked_sub(31u32.wrapping_sub(state.leading_zeros()))
                .ok_or_else(|| corrupt("FSE count too large"))?;
            entry.bits = u8::try_from(bits).map_err(|_| corrupt("FSE count too large"))?;
            let base = usize::try_from(state.wrapping_shl(bits))
                .ok()
                .and_then(|s| s.checked_sub(size))
                .and_then(|b| u16::try_from(b).ok())
                .ok_or_else(|| corrupt("FSE state out of range"))?;
            entry.base = base;
        }
        Ok(Table { log, entries })
    }

    /// Reads a table description at the start of `data` (accuracy log at
    /// most `max_log`, symbols at most `max_symbol`); returns the table and
    /// the bytes the description took.
    pub(super) fn read(data: &[u8], max_log: u32, max_symbol: usize) -> Result<(Table, usize)> {
        let mut br = Forward::default();
        let log = br.peek(data, 4).wrapping_add(5);
        br.skip(4);
        if log > max_log {
            return Err(corrupt("FSE accuracy log too large"));
        }
        let mut remaining: i32 = (1i32 << log).wrapping_add(1);
        let mut threshold: i32 = 1i32 << log;
        let mut nb_bits = log.wrapping_add(1);
        let mut norm: Vec<i16> = Vec::new();
        while remaining > 1 {
            if norm.len() > max_symbol {
                return Err(corrupt("FSE table describes too many symbols"));
            }
            let max = threshold
                .wrapping_mul(2)
                .wrapping_sub(1)
                .wrapping_sub(remaining);
            let value = i32::try_from(br.peek(data, nb_bits)).unwrap_or(0);
            let mut count;
            if (value & threshold.wrapping_sub(1)) < max {
                count = value & threshold.wrapping_sub(1);
                br.skip(nb_bits.wrapping_sub(1));
            } else {
                count = value & threshold.wrapping_mul(2).wrapping_sub(1);
                if count >= threshold {
                    count = count.wrapping_sub(max);
                }
                br.skip(nb_bits);
            }
            count = count.wrapping_sub(1);
            remaining = remaining.wrapping_sub(count.wrapping_abs());
            norm.push(i16::try_from(count).map_err(|_| corrupt("bad FSE count"))?);
            if count == 0 {
                // A run of zero-probability symbols: 2-bit repeat counts,
                // continued while a count is 3.
                loop {
                    let repeat = br.peek(data, 2);
                    br.skip(2);
                    norm.extend(std::iter::repeat_n(0, usize::try_from(repeat).unwrap_or(0)));
                    if repeat != 3 {
                        break;
                    }
                    if norm.len() > max_symbol {
                        return Err(corrupt("FSE table describes too many symbols"));
                    }
                }
            }
            // A count's encoding caps it at `remaining - 1`, so `remaining`
            // stays >= 1 and this stops by `threshold` 1.
            while remaining < threshold {
                nb_bits = nb_bits.wrapping_sub(1);
                threshold >>= 1;
            }
        }
        // A count is only added while there are symbols left, so a table
        // that sums correctly also has at most `max_symbol + 1` of them.
        if remaining != 1 {
            return Err(corrupt("bad FSE table description"));
        }
        if br.bytes_used() > data.len() {
            return Err(corrupt("FSE table description past its block"));
        }
        Ok((Table::build(&norm, log)?, br.bytes_used()))
    }
}

/// The predefined literals-length distribution (accuracy log 6).
pub(super) const LL_DEFAULT: [i16; 36] = [
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];

/// The predefined match-length distribution (accuracy log 6).
pub(super) const ML_DEFAULT: [i16; 53] = [
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];

/// The predefined offset-code distribution (accuracy log 5).
pub(super) const OF_DEFAULT: [i16; 29] = [
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
];
