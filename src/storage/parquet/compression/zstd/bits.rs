//! Bit readers for zstd's entropy-coded data (RFC 8878 §4.1).
//!
//! Neither reader holds its bytes: the slice is passed to every call, so
//! neither needs a named lifetime (`make check-mvl-limit`).

/// Eight bytes of `data` from `byte` on, little-endian; zero past the end.
#[inline(always)]
fn load(data: &[u8], byte: usize) -> u64 {
    if let Some(Ok(b)) = data
        .get(byte..byte.saturating_add(8))
        .map(<[u8; 8]>::try_from)
    {
        return u64::from_le_bytes(b);
    }
    let mut buf = [0u8; 8];
    for (i, slot) in buf.iter_mut().enumerate() {
        *slot = data.get(byte.saturating_add(i)).copied().unwrap_or(0);
    }
    u64::from_le_bytes(buf)
}

/// The low `n` bits set (`n` < 64).
#[inline(always)]
fn mask(n: u32) -> u64 {
    1u64.wrapping_shl(n).wrapping_sub(1)
}

/// A forward little-endian reader: FSE table descriptions are read from
/// the low bit of their first byte up.
#[derive(Debug, Default)]
pub(super) struct Forward {
    bit: usize,
}

impl Forward {
    /// The next `n` bits (`n` <= 32) at the cursor, without consuming them.
    pub(super) fn peek(&self, data: &[u8], n: u32) -> u32 {
        let v = load(data, self.bit.wrapping_div(8))
            .wrapping_shr(u32::try_from(self.bit.wrapping_rem(8)).unwrap_or(0));
        u32::try_from(v & mask(n)).unwrap_or(0)
    }

    /// Moves the cursor `n` bits on.
    pub(super) fn skip(&mut self, n: u32) {
        self.bit = self.bit.saturating_add(usize::try_from(n).unwrap_or(0));
    }

    /// Whole bytes read so far (a partly read byte counts).
    pub(super) fn bytes_used(&self) -> usize {
        self.bit.div_ceil(8)
    }
}

/// A backward reader: Huffman and sequence bitstreams are read from the
/// bit below the end marker (the highest set bit of the last byte) down
/// to bit 0 of the first byte. Reading past bit 0 yields zeros, and
/// [`Self::overflowed`] reports it; a well-formed stream ends exactly at 0.
#[derive(Debug)]
pub(super) struct Backward {
    /// Unread bits left; negative once read past the start.
    pos: i64,
}

impl Backward {
    /// A reader over `data`, or `None` if it is empty or its last byte has
    /// no end marker.
    pub(super) fn new(data: &[u8]) -> Option<Backward> {
        let last = *data.last()?;
        if last == 0 {
            return None;
        }
        let total = i64::try_from(data.len()).ok()?.checked_mul(8)?;
        let marker = i64::from(last.leading_zeros()).checked_add(1)?;
        Some(Backward {
            pos: total.checked_sub(marker)?,
        })
    }

    /// The next `n` bits (`n` <= 56), without consuming them.
    #[inline(always)]
    pub(super) fn peek(&self, data: &[u8], n: u32) -> u64 {
        if n == 0 {
            return 0;
        }
        let start = self.pos.wrapping_sub(i64::from(n));
        if let Ok(start) = u64::try_from(start) {
            let byte = usize::try_from(start.wrapping_div(8)).unwrap_or(usize::MAX);
            let shift = u32::try_from(start.wrapping_rem(8)).unwrap_or(0);
            load(data, byte).wrapping_shr(shift) & mask(n)
        } else if let Ok(have) = u32::try_from(self.pos) {
            // Fewer than `n` bits left: they are the high bits, zeros below.
            (load(data, 0) & mask(have)).wrapping_shl(n.wrapping_sub(have))
        } else {
            0
        }
    }

    /// Consumes `n` bits.
    #[inline(always)]
    pub(super) fn consume(&mut self, n: u32) {
        self.pos = self.pos.wrapping_sub(i64::from(n));
    }

    /// Reads and consumes `n` bits.
    #[inline(always)]
    pub(super) fn read(&mut self, data: &[u8], n: u32) -> u64 {
        let v = self.peek(data, n);
        self.consume(n);
        v
    }

    /// Whether more bits were read than the stream holds.
    pub(super) fn overflowed(&self) -> bool {
        self.pos < 0
    }
}

/// A backward reader for the sequences: the same bits as [`Backward`],
/// read through a 64-bit container reloaded between groups of reads
/// (at most 56 bits each) instead of a load per read.
#[derive(Debug)]
pub(super) struct Stream {
    /// Byte offset of the container in the stream.
    ptr: usize,
    /// The 8 bytes at `ptr`, little-endian.
    container: u64,
    /// Bits of the container, from its top, already read.
    consumed: u32,
}

impl Stream {
    /// A reader over `data`, or `None` if it is empty or its last byte has
    /// no end marker.
    pub(super) fn new(data: &[u8]) -> Option<Stream> {
        let last = *data.last()?;
        if last == 0 {
            return None;
        }
        let ptr = data.len().saturating_sub(8);
        // A stream under 8 bytes sits at the container's bottom: the
        // zeros above it are skipped like the end marker.
        let pad = u32::try_from(8usize.saturating_sub(data.len()).wrapping_mul(8)).ok()?;
        Some(Stream {
            ptr,
            container: load(data, ptr),
            consumed: pad.wrapping_add(last.leading_zeros()).wrapping_add(1),
        })
    }

    /// Refills the container: at least 56 bits are unread afterwards,
    /// unless fewer are left in the stream.
    #[inline(always)]
    pub(super) fn reload(&mut self, data: &[u8]) {
        let bytes = usize::try_from(self.consumed >> 3).unwrap_or(0);
        if self.ptr >= bytes {
            self.ptr = self.ptr.wrapping_sub(bytes);
            self.consumed &= 7;
        } else {
            // Fewer bytes left below the container than were consumed.
            self.consumed = self
                .consumed
                .wrapping_sub(u32::try_from(self.ptr.wrapping_mul(8)).unwrap_or(0));
            self.ptr = 0;
        }
        self.container = load(data, self.ptr);
    }

    /// Reads `n` bits (`n` <= 56 and within the last [`Self::reload`]'s
    /// 56); past the stream's start they are garbage, which
    /// [`Self::finished`] reports.
    #[inline(always)]
    pub(super) fn read(&mut self, n: u32) -> u64 {
        let v = self.peek(n);
        self.consume(n);
        v
    }

    /// The next `n` bits, as [`Self::read`] without consuming them.
    #[inline(always)]
    pub(super) fn peek(&self, n: u32) -> u64 {
        (self.container.wrapping_shl(self.consumed) >> 1) >> 63u32.wrapping_sub(n)
    }

    /// Consumes `n` bits.
    #[inline(always)]
    pub(super) fn consume(&mut self, n: u32) {
        self.consumed = self.consumed.wrapping_add(n);
    }

    /// Whether the stream was read exactly to its first bit.
    pub(super) fn finished(&self) -> bool {
        self.ptr == 0 && self.consumed == 64
    }
}
