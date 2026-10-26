// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! HyperLogLog sketch behind `approx_count_distinct` (#603).
//!
//! Precision [`PRECISION`] = 14: `2^14` registers, a standard error of
//! `1.04 / sqrt(2^14)` ~= 0.81 % (the documented bound is 2 %, about
//! 2.5 sigma). The sketch is small while it is: a handful of values is a
//! sorted list of `(register, rank)` pairs, and it turns into the 16 KiB
//! dense register array only past [`SPARSE_LIMIT`] registers, so a
//! high-cardinality `GROUP BY` does not pay 16 KiB a group.
//!
//! Merging is a register-wise `max`: commutative and associative, so the
//! estimate does not depend on how rows split into morsels or segments, or
//! on the order partials meet (the deterministic merge #603 asks for).
//! [`Hll::estimate`] is Ertl's improved estimator, which needs no
//! small/large range patch-ups and stays within the bound at every
//! cardinality.

/// Index bits: `2^PRECISION` registers.
pub(crate) const PRECISION: u32 = 14;
const REGISTERS: usize = 1 << PRECISION;
/// A register's largest rank: `64 - PRECISION + 1` -- the leading zeros
/// of the other hash bits, plus one, with a sentinel bit so an all-zero
/// tail is that many zeros and no more.
const MAX_RANK: u8 = 51;
/// Sparse until this many registers are set; 4 bytes a pair, so the
/// sparse form stays below the dense one's 16 KiB.
const SPARSE_LIMIT: usize = 2048;

const TAG_SPARSE: u8 = 0;
const TAG_DENSE: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Hll {
    /// `(register, rank)` ascending by register, no duplicates.
    Sparse(Vec<(u16, u8)>),
    Dense(Vec<u8>),
}

impl Default for Hll {
    fn default() -> Self {
        Hll::Sparse(Vec::new())
    }
}

impl Hll {
    /// Folds one value's 64-bit hash in.
    pub(crate) fn insert(&mut self, hash: u64) {
        // The top bits pick the register: a shifted `u64` is below 2^14.
        let index = u16::try_from(hash >> (64 - PRECISION)).unwrap_or(u16::MAX);
        // The sentinel bit bounds the zero run at `64 - PRECISION`.
        let rest = (hash << PRECISION) | (1 << (PRECISION - 1));
        let rank = u8::try_from(rest.leading_zeros())
            .unwrap_or(MAX_RANK)
            .saturating_add(1);
        self.raise(index, rank);
    }

    /// Sets register `index` to at least `rank`.
    fn raise(&mut self, index: u16, rank: u8) {
        match self {
            Hll::Dense(registers) => {
                if let Some(slot) = registers.get_mut(usize::from(index)) {
                    *slot = (*slot).max(rank);
                }
            }
            Hll::Sparse(pairs) => {
                match pairs.binary_search_by_key(&index, |&(i, _)| i) {
                    Ok(at) => {
                        if let Some(pair) = pairs.get_mut(at) {
                            pair.1 = pair.1.max(rank);
                        }
                    }
                    Err(at) => pairs.insert(at, (index, rank)),
                }
                if pairs.len() > SPARSE_LIMIT {
                    self.densify();
                }
            }
        }
    }

    fn densify(&mut self) {
        if let Hll::Sparse(pairs) = self {
            let mut registers = vec![0u8; REGISTERS];
            for &(i, rank) in pairs.iter() {
                if let Some(slot) = registers.get_mut(usize::from(i)) {
                    *slot = rank;
                }
            }
            *self = Hll::Dense(registers);
        }
    }

    /// Register-wise `max` of `other` into `self`.
    pub(crate) fn merge(&mut self, other: &Hll) {
        match other {
            Hll::Sparse(pairs) => {
                for &(i, rank) in pairs {
                    self.raise(i, rank);
                }
            }
            Hll::Dense(registers) => {
                self.densify();
                if let Hll::Dense(mine) = self {
                    for (slot, &rank) in mine.iter_mut().zip(registers) {
                        *slot = (*slot).max(rank);
                    }
                }
            }
        }
    }

    /// The estimated number of distinct values inserted.
    pub(crate) fn estimate(&self) -> i64 {
        // Registers by value: `counts[k]` registers hold rank `k`.
        let mut counts = [0u32; MAX_RANK as usize + 1];
        match self {
            Hll::Sparse(pairs) => {
                for &(_, rank) in pairs {
                    bump(&mut counts, rank);
                }
                let zeros = REGISTERS.saturating_sub(pairs.len());
                if let Some(c) = counts.first_mut() {
                    *c = u32::try_from(zeros).unwrap_or(u32::MAX);
                }
            }
            Hll::Dense(registers) => {
                for &rank in registers {
                    bump(&mut counts, rank);
                }
            }
        }
        let m = f64::from(1u32 << PRECISION);
        let at = |k: usize| f64::from(counts.get(k).copied().unwrap_or(0));
        let q = (64 - PRECISION) as usize;
        let mut z = m * tau(1.0 - at(q + 1) / m);
        for k in (1..=q).rev() {
            z = 0.5 * (z + at(k));
        }
        z += m * sigma(at(0) / m);
        if !z.is_normal() {
            // No register set (sigma is infinite) or every one at the top
            // rank (zero): nothing to estimate.
            return 0;
        }
        let alpha_inf = 1.0 / (2.0 * std::f64::consts::LN_2);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "a rounded, non-negative cardinality estimate far below i64::MAX"
        )]
        let estimate = (alpha_inf * m * m / z).round() as i64;
        estimate
    }

    /// The sketch as bytes: a tag, then the pairs or the registers.
    pub(crate) fn encode(&self) -> Vec<u8> {
        match self {
            Hll::Sparse(pairs) => {
                let mut out = Vec::new();
                out.push(TAG_SPARSE);
                for &(i, rank) in pairs {
                    out.extend_from_slice(&i.to_le_bytes());
                    out.push(rank);
                }
                out
            }
            Hll::Dense(registers) => {
                let mut out = Vec::with_capacity(registers.len().saturating_add(1));
                out.push(TAG_DENSE);
                out.extend_from_slice(registers);
                out
            }
        }
    }

    /// The sketch [`Hll::encode`] wrote; `None` for anything else.
    pub(crate) fn decode(bytes: &[u8]) -> Option<Hll> {
        let (&tag, body) = bytes.split_first()?;
        match tag {
            TAG_SPARSE if body.len() % 3 == 0 && body.len() / 3 <= SPARSE_LIMIT => {
                let mut pairs: Vec<(u16, u8)> = body
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .map(|&[lo, hi, rank]| (u16::from_le_bytes([lo, hi]), rank))
                    .collect();
                // Ascending and unique, as `raise` relies on.
                if pairs
                    .windows(2)
                    .any(|w| w.first().zip(w.get(1)).is_some_and(|(a, b)| a.0 >= b.0))
                    || pairs
                        .iter()
                        .any(|&(i, r)| usize::from(i) >= REGISTERS || r == 0 || r > MAX_RANK)
                {
                    return None;
                }
                pairs.shrink_to_fit();
                Some(Hll::Sparse(pairs))
            }
            TAG_DENSE if body.len() == REGISTERS && body.iter().all(|&r| r <= MAX_RANK) => {
                Some(Hll::Dense(body.to_vec()))
            }
            _ => None,
        }
    }
}

fn bump(counts: &mut [u32], rank: u8) {
    if let Some(c) = counts.get_mut(usize::from(rank)) {
        *c = c.saturating_add(1);
    }
}

/// Ertl's `sigma`, for the registers still at zero.
fn sigma(mut x: f64) -> f64 {
    if x >= 1.0 {
        return f64::INFINITY;
    }
    let mut y = 1.0;
    let mut z = x;
    loop {
        x *= x;
        let before = z;
        z += x * y;
        y += y;
        if z == before {
            return z;
        }
    }
}

/// Ertl's `tau`, for the registers at the top rank.
fn tau(mut x: f64) -> f64 {
    if x == 0.0 || x >= 1.0 {
        return 0.0;
    }
    let mut y = 1.0;
    let mut z = 1.0 - x;
    loop {
        x = x.sqrt();
        let before = z;
        y *= 0.5;
        z -= (1.0 - x).powi(2) * y;
        if z == before {
            return z / 3.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A well-mixed 64-bit hash of `n` (SplitMix64's finalizer).
    fn hash(n: u64) -> u64 {
        let mut z = n.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn sketch(range: std::ops::Range<u64>) -> Hll {
        let mut h = Hll::default();
        for n in range {
            h.insert(hash(n));
        }
        h
    }

    fn within(estimate: i64, exact: i64, tolerance: f64) -> bool {
        ((estimate - exact) as f64).abs() <= exact as f64 * tolerance
    }

    #[test]
    fn empty_sketch_estimates_zero() {
        assert_eq!(Hll::default().estimate(), 0);
    }

    #[test]
    fn small_cardinalities_are_near_exact() {
        for n in [1u64, 2, 10, 100, 1000] {
            let est = sketch(0..n).estimate();
            assert!(within(est, n as i64, 0.02), "n={n} est={est}");
        }
    }

    #[test]
    fn error_stays_within_two_percent_across_the_range() {
        // Straddles the sparse/dense switch and the estimator's
        // small-range handover (~2.5 * 2^14).
        for n in [3_000u64, 10_000, 40_000, 41_000, 100_000, 500_000] {
            let est = sketch(0..n).estimate();
            assert!(within(est, n as i64, 0.02), "n={n} est={est}");
        }
    }

    #[test]
    fn duplicates_do_not_move_the_estimate() {
        let mut h = sketch(0..5_000);
        let before = h.estimate();
        for n in 0..5_000 {
            h.insert(hash(n));
        }
        assert_eq!(h.estimate(), before);
    }

    #[test]
    fn merge_equals_one_sketch_over_the_union_whatever_the_split_or_order() {
        let whole = sketch(0..60_000);
        let (a, b, c) = (
            sketch(0..20_000),
            sketch(15_000..45_000),
            sketch(40_000..60_000),
        );
        let mut abc = a.clone();
        abc.merge(&b);
        abc.merge(&c);
        let mut cba = c.clone();
        cba.merge(&b);
        cba.merge(&a);
        // Sparse and dense forms of one register set compare by estimate
        // and by encoding of the densified form.
        assert_eq!(abc.estimate(), whole.estimate());
        assert_eq!(cba.estimate(), whole.estimate());
        let (mut x, mut y) = (abc, cba);
        x.densify();
        y.densify();
        assert_eq!(x, y);
    }

    #[test]
    fn sparse_merges_sparse_and_dense_alike() {
        let small = sketch(0..50);
        let big = sketch(0..30_000);
        let mut a = small.clone();
        a.merge(&big);
        let mut b = big.clone();
        b.merge(&small);
        assert_eq!(a.estimate(), b.estimate());
        assert!(matches!(a, Hll::Dense(_)));
    }

    #[test]
    fn encoding_round_trips_both_forms() {
        for h in [sketch(0..100), sketch(0..20_000), Hll::default()] {
            let back = Hll::decode(&h.encode()).unwrap();
            assert_eq!(back, h);
        }
        assert!(sketch(0..100).encode().len() < 1000);
    }

    #[test]
    fn decode_rejects_malformed_bytes() {
        assert!(Hll::decode(&[]).is_none());
        assert!(Hll::decode(&[9]).is_none());
        assert!(Hll::decode(&[TAG_SPARSE, 1, 2]).is_none());
        // Unsorted pairs.
        assert!(Hll::decode(&[TAG_SPARSE, 5, 0, 1, 4, 0, 1]).is_none());
        // Register out of range.
        assert!(Hll::decode(&[TAG_SPARSE, 0xFF, 0xFF, 1]).is_none());
        // Zero or oversized rank.
        assert!(Hll::decode(&[TAG_SPARSE, 0, 0, 0]).is_none());
        assert!(Hll::decode(&[TAG_SPARSE, 0, 0, 200]).is_none());
        // A dense body of the wrong length, or with a bad rank.
        assert!(Hll::decode(&[TAG_DENSE, 1, 2]).is_none());
        let mut dense = vec![TAG_DENSE];
        dense.extend(std::iter::repeat_n(0u8, REGISTERS));
        assert!(Hll::decode(&dense).is_some());
        dense[1] = 200;
        assert!(Hll::decode(&dense).is_none());
    }

    fn sparse_body(pairs: &[(u16, u8)]) -> Vec<u8> {
        let mut out = vec![TAG_SPARSE];
        for &(i, rank) in pairs {
            out.extend_from_slice(&i.to_le_bytes());
            out.push(rank);
        }
        out
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_hll_decode_14f99db7__v1_a_whole_number_of_pairs_within_the_limit_decodes() {
        assert!(Hll::decode(&sparse_body(&[(1, 1), (2, 3)])).is_some());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_hll_decode_14f99db7__v2_a_whole_number_of_pairs_past_the_limit_is_refused() {
        let pairs: Vec<(u16, u8)> = (0..=SPARSE_LIMIT as u16).map(|i| (i, 1)).collect();
        assert!(Hll::decode(&sparse_body(&pairs)).is_none());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_hll_decode_14f99db7__v3_a_torn_pair_is_refused() {
        assert!(Hll::decode(&[TAG_SPARSE, 1, 0]).is_none());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_hll_decode_a3209780__v1_pairs_out_of_order_are_refused() {
        assert!(Hll::decode(&sparse_body(&[(5, 1), (4, 1)])).is_none());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_hll_decode_a3209780__v2_ordered_pairs_in_range_decode() {
        assert!(Hll::decode(&sparse_body(&[(4, 1), (5, MAX_RANK)])).is_some());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_hll_decode_a3209780__v3_an_ordered_pair_with_a_bad_rank_is_refused() {
        assert!(Hll::decode(&sparse_body(&[(4, 1), (5, 0)])).is_none());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_hll_decode_76b118cd__v1_a_full_register_array_decodes() {
        let mut dense = vec![TAG_DENSE];
        dense.extend(std::iter::repeat_n(MAX_RANK, REGISTERS));
        assert!(Hll::decode(&dense).is_some());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_hll_decode_76b118cd__v2_a_full_array_with_a_bad_rank_is_refused() {
        let mut dense = vec![TAG_DENSE];
        dense.extend(std::iter::repeat_n(MAX_RANK + 1, REGISTERS));
        assert!(Hll::decode(&dense).is_none());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_hll_decode_76b118cd__v3_a_short_register_array_is_refused() {
        assert!(Hll::decode(&[TAG_DENSE, 1, 1]).is_none());
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_hll_tau_693cd9fd__v1_no_register_below_the_top_rank_is_zero() {
        assert_eq!(tau(0.0), 0.0);
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_hll_tau_693cd9fd__v2_some_registers_at_the_top_rank_is_positive() {
        let t = tau(0.5);
        assert!(t > 0.0 && t < 1.0, "{t}");
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_hll_tau_693cd9fd__v3_every_register_at_the_top_rank_is_zero() {
        assert_eq!(tau(1.0), 0.0);
    }
}
