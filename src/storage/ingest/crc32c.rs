//! CRC-32C (Castagnoli, reflected polynomial `0x82F63B78`), the checksum
//! on every WAL frame. Table-driven, one byte per step; no dependency.
//!
//! Spec: RFC 3720 §12.1 and appendix B.4 (the test vectors below).

use std::sync::OnceLock;

const POLY: u32 = 0x82f6_3b78;

static TABLE: OnceLock<[u32; 256]> = OnceLock::new();

fn build_table() -> [u32; 256] {
    std::array::from_fn(|i| {
        let mut crc = u32::try_from(i).unwrap_or(0);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                crc.wrapping_shr(1) ^ POLY
            } else {
                crc.wrapping_shr(1)
            };
        }
        crc
    })
}

/// Extends `crc` (the value returned for the bytes so far, 0 to start)
/// with `data`.
pub(crate) fn update(crc: u32, data: &[u8]) -> u32 {
    let table = TABLE.get_or_init(build_table);
    let mut crc = !crc;
    for &byte in data {
        let index = usize::from(u8::try_from(crc & 0xff).unwrap_or(0) ^ byte);
        crc = crc.wrapping_shr(8) ^ table.get(index).copied().unwrap_or(0);
    }
    !crc
}

/// CRC-32C of `data`.
#[cfg(test)]
pub(crate) fn crc32c(data: &[u8]) -> u32 {
    update(0, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_3720_vectors() {
        assert_eq!(crc32c(&[0u8; 32]), 0x8a91_36aa);
        assert_eq!(crc32c(&[0xffu8; 32]), 0x62a8_ab43);
        let ascending: Vec<u8> = (0..32).collect();
        assert_eq!(crc32c(&ascending), 0x46dd_794e);
        let descending: Vec<u8> = (0..32).rev().collect();
        assert_eq!(crc32c(&descending), 0x113f_db5c);
    }

    #[test]
    fn check_value_and_empty_input() {
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
        assert_eq!(crc32c(b""), 0);
    }

    #[test]
    fn update_is_incremental() {
        let data = b"the quick brown fox jumps over the lazy dog";
        let (a, b) = data.split_at(17);
        assert_eq!(update(update(0, a), b), crc32c(data));
    }
}
