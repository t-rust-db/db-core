//! XXH64 (seed 0), for a frame's content checksum: its low 32 bits,
//! little-endian, follow the last block (RFC 8878 §3.1.1).

const P1: u64 = 0x9E37_79B1_85EB_CA87;
const P2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const P3: u64 = 0x1656_67B1_9E37_79F9;
const P4: u64 = 0x85EB_CA77_C2B2_AE63;
const P5: u64 = 0x27D4_EB2F_1656_67C5;

fn round(acc: u64, input: u64) -> u64 {
    acc.wrapping_add(input.wrapping_mul(P2))
        .rotate_left(31)
        .wrapping_mul(P1)
}

fn merge(acc: u64, value: u64) -> u64 {
    (acc ^ round(0, value)).wrapping_mul(P1).wrapping_add(P4)
}

/// XXH64 of `data` with seed 0.
pub(super) fn xxh64(data: &[u8]) -> u64 {
    let (stripes, tail) = data.as_chunks::<32>();
    let mut h = if stripes.is_empty() {
        P5
    } else {
        let mut v = [P1.wrapping_add(P2), P2, 0, 0u64.wrapping_sub(P1)];
        for stripe in stripes {
            for (acc, lane) in v.iter_mut().zip(stripe.as_chunks::<8>().0) {
                *acc = round(*acc, u64::from_le_bytes(*lane));
            }
        }
        let [v1, v2, v3, v4] = v;
        let h = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));
        [v1, v2, v3, v4].into_iter().fold(h, merge)
    };
    h = h.wrapping_add(u64::try_from(data.len()).unwrap_or(u64::MAX));
    let (words, mut rest) = tail.as_chunks::<8>();
    for word in words {
        h = (h ^ round(0, u64::from_le_bytes(*word)))
            .rotate_left(27)
            .wrapping_mul(P1)
            .wrapping_add(P4);
    }
    if let Some((word, after)) = rest.split_first_chunk::<4>() {
        h = (h ^ u64::from(u32::from_le_bytes(*word)).wrapping_mul(P1))
            .rotate_left(23)
            .wrapping_mul(P2)
            .wrapping_add(P3);
        rest = after;
    }
    for &b in rest {
        h = (h ^ u64::from(b).wrapping_mul(P5))
            .rotate_left(11)
            .wrapping_mul(P1);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(P2);
    h ^= h >> 29;
    h = h.wrapping_mul(P3);
    h ^ (h >> 32)
}

#[cfg(test)]
mod tests {
    use super::xxh64;

    #[test]
    fn known_vectors() {
        assert_eq!(xxh64(b""), 0xEF46_DB37_51D8_E999);
        assert_eq!(xxh64(b"abc"), 0x44BC_2CF5_AD77_0999);
    }
}
