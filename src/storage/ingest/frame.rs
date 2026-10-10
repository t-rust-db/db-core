//! WAL frames (ADR 0027): `[len u32][crc32c u32][lsn u64][payload]`, all
//! little-endian. The checksum covers `len`, `lsn` and the payload, so a
//! frame whose length or LSN was torn fails it too.

use super::crc32c;

/// Bytes before the payload.
pub(crate) const HEADER_LEN: usize = 16;

/// One frame read at an offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Frame {
    /// A whole frame with a matching checksum; the payload is
    /// `buf[payload_start..end]` and the next frame starts at `end`.
    Complete {
        lsn: u64,
        payload_start: usize,
        end: usize,
    },
    /// The buffer ends inside the header or the payload.
    Incomplete,
    /// All bytes are present but the checksum does not match.
    BadChecksum,
}

fn checksum(len: [u8; 4], lsn: [u8; 8], payload: &[u8]) -> u32 {
    let crc = crc32c::update(0, &len);
    let crc = crc32c::update(crc, &lsn);
    crc32c::update(crc, payload)
}

/// Appends one frame; `None` (and nothing appended) when the payload is
/// longer than a `u32` length can describe.
pub(crate) fn encode(out: &mut Vec<u8>, lsn: u64, payload: &[u8]) -> Option<()> {
    let len = u32::try_from(payload.len()).ok()?.to_le_bytes();
    let lsn = lsn.to_le_bytes();
    out.extend_from_slice(&len);
    out.extend_from_slice(&checksum(len, lsn, payload).to_le_bytes());
    out.extend_from_slice(&lsn);
    out.extend_from_slice(payload);
    Some(())
}

/// Bytes `encode` appends for a payload of `payload_len` bytes.
#[cfg(test)]
pub(crate) fn encoded_len(payload_len: usize) -> usize {
    HEADER_LEN.saturating_add(payload_len)
}

fn array<const N: usize>(buf: &[u8], at: usize) -> Option<[u8; N]> {
    let end = at.checked_add(N)?;
    buf.get(at..end)?.try_into().ok()
}

/// Reads the frame starting at `buf[at]`.
pub(crate) fn decode(buf: &[u8], at: usize) -> Frame {
    let (Some(len), Some(crc), Some(lsn)) = (
        array::<4>(buf, at),
        at.checked_add(4).and_then(|o| array::<4>(buf, o)),
        at.checked_add(8).and_then(|o| array::<8>(buf, o)),
    ) else {
        return Frame::Incomplete;
    };
    let Some(payload_start) = at.checked_add(HEADER_LEN) else {
        return Frame::Incomplete;
    };
    let payload_len = usize::try_from(u32::from_le_bytes(len)).unwrap_or(usize::MAX);
    let Some(end) = payload_start.checked_add(payload_len) else {
        return Frame::Incomplete;
    };
    let Some(payload) = buf.get(payload_start..end) else {
        return Frame::Incomplete;
    };
    if checksum(len, lsn, payload) != u32::from_le_bytes(crc) {
        return Frame::BadChecksum;
    }
    Frame::Complete {
        lsn: u64::from_le_bytes(lsn),
        payload_start,
        end,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_rng::Rng;

    fn frames(payloads: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, p) in payloads.iter().enumerate() {
            encode(&mut out, i as u64 + 1, p).unwrap();
        }
        out
    }

    #[test]
    fn round_trips_a_sequence() {
        let buf = frames(&[b"one", b"", b"three"]);
        let mut at = 0;
        let mut seen = Vec::new();
        while let Frame::Complete {
            lsn,
            payload_start,
            end,
        } = decode(&buf, at)
        {
            seen.push((lsn, buf[payload_start..end].to_vec()));
            at = end;
        }
        assert_eq!(at, buf.len());
        assert_eq!(
            seen,
            vec![
                (1, b"one".to_vec()),
                (2, Vec::new()),
                (3, b"three".to_vec())
            ]
        );
        assert_eq!(buf.len(), encoded_len(3) + encoded_len(0) + encoded_len(5));
    }

    #[test]
    fn every_truncation_is_incomplete() {
        let buf = frames(&[b"payload"]);
        for cut in 0..buf.len() {
            assert_eq!(decode(&buf[..cut], 0), Frame::Incomplete, "cut at {cut}");
        }
    }

    #[test]
    fn every_bit_flip_is_detected() {
        // Flips in len make the frame incomplete or mis-sized (checksum
        // fails); flips anywhere else fail the checksum.
        let buf = frames(&[b"0123456789"]);
        for byte in 0..buf.len() {
            for bit in 0..8 {
                let mut bad = buf.clone();
                bad[byte] ^= 1 << bit;
                assert!(
                    !matches!(decode(&bad, 0), Frame::Complete { .. }),
                    "flip {byte}:{bit} accepted"
                );
            }
        }
    }

    #[test]
    fn zero_fill_is_not_a_frame() {
        // What a crash can leave after an extended-but-unwritten tail.
        assert_eq!(decode(&[0u8; 64], 0), Frame::BadChecksum);
    }

    #[test]
    fn random_payloads_round_trip() {
        let mut rng = Rng::new(0x6672_616d);
        for _ in 0..500 {
            let payload: Vec<u8> = (0..rng.below(300)).map(|_| rng.next() as u8).collect();
            let lsn = rng.next();
            let mut buf = Vec::new();
            encode(&mut buf, lsn, &payload).unwrap();
            assert_eq!(
                decode(&buf, 0),
                Frame::Complete {
                    lsn,
                    payload_start: HEADER_LEN,
                    end: buf.len()
                }
            );
            assert_eq!(&buf[HEADER_LEN..], payload.as_slice());
        }
    }
}
