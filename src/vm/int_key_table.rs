// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Open-addressing group tables shared by the two places a `GROUP BY` is
//! grouped: [`IntKeyTable`], `i64 -> group id` for a single integer key, and
//! [`HashGroupTable`], `key hash -> group id` for every other key shape
//! (#479), where the caller compares the keys themselves.
//!
//! An open-addressing `i64 -> group id` table shared by the two places a
//! `GROUP BY <int column>` is grouped: per segment in
//! [`super::batch`]'s `GroupReduce` (#479) and across segments in
//! [`super::combine`]'s merge (#478). Linear probing over a power-of-two
//! `u32` slot array into a dense `(key, group id)` vector, load factor
//! <= 1/2, MurmurHash3's 64-bit finalizer for the slot index -- the
//! `key64` specialization ClickHouse and DataFusion (`GroupValuesPrimitive`)
//! both have; not a bare multiplicative hash, which never brings a key's
//! high-bit entropy (timestamps, shifted ids) down to the index bits.

use super::batch::{Result, VmError};

/// MurmurHash3's 64-bit finalizer -- what DuckDB and ClickHouse hash
/// integer keys with. Not a bare multiplicative hash: real ids carry
/// their entropy in the high bits (timestamps, shifted keys), which a
/// multiply alone never brings down to the low bits an open-addressing
/// table indexes by; the xor-shifts do.
pub(crate) const fn murmur_finalize(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    x ^= x >> 33;
    x = x.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    x ^ (x >> 33)
}

/// An empty slot in [`IntKeyTable::slots`].
const EMPTY: u32 = u32::MAX;

/// Open-addressing table from an `i64` group key to its group id -- the
/// `GROUP BY <int column>` specialization every engine has (ClickHouse
/// `key64`, DataFusion `GroupValuesPrimitive`, Velox's array mode).
/// Linear probing over a power-of-two `slots` array of indices (4 bytes
/// each, so 100K groups at load <= 1/2 is ~800 KB: L2-resident); the key
/// and its group id live in the dense `keys` vector the slots index into,
/// and the probe compares that `i64` directly -- no cached hash needed
/// when the key is 8 bytes. `Null` keys form one group of their own,
/// outside the table (`GROUP BY` semantics: `Null` groups with `Null`) --
/// which is why a key's position in `keys` and its group id differ.
pub(crate) struct IntKeyTable {
    /// Index into `keys`, or [`EMPTY`].
    slots: Vec<u32>,
    mask: usize,
    /// `(key, group id)` in insertion order.
    keys: Vec<(i64, u32)>,
    null_group: Option<usize>,
}

impl IntKeyTable {
    /// Sized for `expected_groups` at load factor <= 1/2 (all engines
    /// pay for the resize; the first chunk's row count is a good guess
    /// at the group count when every segment sees every group).
    pub(crate) fn with_capacity(expected_groups: usize) -> Self {
        let capacity = expected_groups
            .saturating_mul(2)
            .max(16)
            .next_power_of_two();
        IntKeyTable {
            slots: vec![EMPTY; capacity],
            mask: capacity.wrapping_sub(1),
            keys: Vec::with_capacity(expected_groups),
            null_group: None,
        }
    }

    /// The `Null` key's group -- one group outside the table (`GROUP BY`
    /// semantics: `Null` groups with `Null`), created as `next_id` on first
    /// sight -- `(id, inserted)`, like [`IntKeyTable::get_or_insert`].
    pub(crate) fn null_group_or_insert(&mut self, next_id: usize) -> (usize, bool) {
        match self.null_group {
            Some(g) => (g, false),
            None => {
                self.null_group = Some(next_id);
                (next_id, true)
            }
        }
    }

    /// Slots currently allocated (a power of two).
    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// The home slot of `key`: its bit pattern (not a sign-converted
    /// magnitude) through the finalizer, masked into `slots`.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "masked to `< slots.len()`, which is a `usize`, before use"
    )]
    fn slot_of(&self, key: i64) -> usize {
        (murmur_finalize(u64::from_ne_bytes(key.to_ne_bytes())) as usize) & self.mask
    }

    /// The group id for `key`, inserting it as group `next_id` when unseen
    /// -- `(id, inserted)`.
    #[allow(
        clippy::indexing_slicing,
        reason = "`slots[i]`: `i` is masked into `0..slots.len()`; `keys[k]`: every non-EMPTY slot holds a `k < keys.len()` by construction"
    )]
    pub(crate) fn get_or_insert(&mut self, key: i64, next_id: usize) -> Result<(usize, bool)> {
        if self.keys.len().saturating_mul(2) >= self.slots.len() {
            self.grow();
        }
        let mut i = self.slot_of(key);
        loop {
            let slot = self.slots[i];
            if slot == EMPTY {
                let too_many = || VmError::MalformedProgram {
                    opcode: "Combine",
                    reason: format!("more than {} groups", u32::MAX),
                };
                let id = u32::try_from(next_id).map_err(|_| too_many())?;
                let k = u32::try_from(self.keys.len()).map_err(|_| too_many())?;
                self.slots[i] = k;
                self.keys.push((key, id));
                return Ok((next_id, true));
            }
            let (candidate, id) = self.keys[slot as usize];
            if candidate == key {
                return Ok((id as usize, false));
            }
            i = i.wrapping_add(1) & self.mask;
        }
    }

    /// Doubles `slots` and re-places every group by its key's hash.
    #[allow(
        clippy::indexing_slicing,
        clippy::cast_possible_truncation,
        reason = "`slots[i]`: `i` is masked into `0..slots.len()`; `k as u32`: `k < keys.len() <= u32::MAX`, every entry was admitted by `get_or_insert`'s `u32::try_from`"
    )]
    fn grow(&mut self) {
        let capacity = self.slots.len().saturating_mul(2);
        self.slots = vec![EMPTY; capacity];
        self.mask = capacity.wrapping_sub(1);
        for k in 0..self.keys.len() {
            let mut i = self.slot_of(self.keys[k].0);
            while self.slots[i] != EMPTY {
                i = i.wrapping_add(1) & self.mask;
            }
            self.slots[i] = k as u32;
        }
    }
}

/// Open-addressing table from a 64-bit key hash to a group id, for keys
/// the caller compares itself -- composite, string, float keys (#479).
/// `u32` slots into a dense `(hash, group id)` vector, linear probing,
/// load factor <= 1/2. A probe compares the stored hash first and only
/// asks the caller's `same_key(group)` for the exact comparison on a
/// hash match, so with a well-mixed hash almost every miss costs one
/// `u64` compare and no key access at all. Replaces `HashMap<u64,
/// Vec<usize>>`: no SipHash of the already-hashed `u64`, no heap `Vec`
/// per group.
pub(crate) struct HashGroupTable {
    /// Index into `entries`, or [`EMPTY`].
    slots: Vec<u32>,
    mask: usize,
    /// `(key hash, group id)` in insertion order.
    entries: Vec<(u64, u32)>,
}

impl HashGroupTable {
    /// Sized for `expected_groups` at load factor <= 1/2; doubles as
    /// groups appear.
    pub(crate) fn with_capacity(expected_groups: usize) -> Self {
        let capacity = expected_groups
            .saturating_mul(2)
            .max(16)
            .next_power_of_two();
        HashGroupTable {
            slots: vec![EMPTY; capacity],
            mask: capacity.wrapping_sub(1),
            entries: Vec::with_capacity(expected_groups),
        }
    }

    /// Slots currently allocated (a power of two).
    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// The home slot of `hash`: re-finalized so a caller's fold (whose low
    /// bits may be weak) still spreads over the slot index.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "masked to `< slots.len()`, which is a `usize`, before use"
    )]
    fn slot_of(&self, hash: u64) -> usize {
        (murmur_finalize(hash) as usize) & self.mask
    }

    /// The group whose key hashes to `hash` and for which `same_key` holds,
    /// inserting a new group `next_id` when there is none --
    /// `(id, inserted)`.
    #[allow(
        clippy::indexing_slicing,
        reason = "`slots[i]`: `i` is masked into `0..slots.len()`; `entries[k]`: every non-EMPTY slot holds a `k < entries.len()` by construction"
    )]
    pub(crate) fn find_or_insert(
        &mut self,
        hash: u64,
        next_id: usize,
        same_key: impl Fn(usize) -> bool,
    ) -> Result<(usize, bool)> {
        if self.entries.len().saturating_mul(2) >= self.slots.len() {
            self.grow();
        }
        let mut i = self.slot_of(hash);
        loop {
            let slot = self.slots[i];
            if slot == EMPTY {
                let too_many = || VmError::MalformedProgram {
                    opcode: "GroupReduce",
                    reason: format!("more than {} groups", u32::MAX),
                };
                let id = u32::try_from(next_id).map_err(|_| too_many())?;
                let k = u32::try_from(self.entries.len()).map_err(|_| too_many())?;
                self.slots[i] = k;
                self.entries.push((hash, id));
                return Ok((next_id, true));
            }
            let (candidate, id) = self.entries[slot as usize];
            if candidate == hash && same_key(id as usize) {
                return Ok((id as usize, false));
            }
            i = i.wrapping_add(1) & self.mask;
        }
    }

    /// Doubles `slots` and re-places every entry by its stored hash.
    #[allow(
        clippy::indexing_slicing,
        clippy::cast_possible_truncation,
        reason = "`slots[i]`: `i` is masked into `0..slots.len()`; `k as u32`: `k < entries.len() <= u32::MAX`, every entry was admitted by `find_or_insert`'s `u32::try_from`"
    )]
    fn grow(&mut self) {
        let capacity = self.slots.len().saturating_mul(2);
        self.slots = vec![EMPTY; capacity];
        self.mask = capacity.wrapping_sub(1);
        for k in 0..self.entries.len() {
            let mut i = self.slot_of(self.entries[k].0);
            while self.slots[i] != EMPTY {
                i = i.wrapping_add(1) & self.mask;
            }
            self.slots[i] = k as u32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_key_table_grows_and_keeps_every_key_findable() {
        let mut table = IntKeyTable::with_capacity(1);
        assert_eq!(table.capacity(), 16);
        for k in 0..1_000i64 {
            let (g, inserted) = table.get_or_insert(k * 1_000_003, k as usize).unwrap();
            assert!(inserted);
            assert_eq!(g, k as usize);
        }
        assert!(table.capacity() >= 2_000, "load factor stays <= 1/2");
        for k in 0..1_000i64 {
            // `next_id` is ignored for a key already present.
            let (g, inserted) = table.get_or_insert(k * 1_000_003, usize::MAX).unwrap();
            assert!(!inserted);
            assert_eq!(g, k as usize);
        }
    }

    #[test]
    fn null_group_is_created_once_and_lives_outside_the_table() {
        let mut table = IntKeyTable::with_capacity(4);
        let (g0, ins0) = table.get_or_insert(7, 0).unwrap();
        let (n1, ins1) = table.null_group_or_insert(1);
        let (n2, ins2) = table.null_group_or_insert(usize::MAX);
        let (g3, ins3) = table.get_or_insert(-7, 2).unwrap();
        assert_eq!((g0, ins0), (0, true));
        assert_eq!((n1, ins1), (1, true));
        assert_eq!((n2, ins2), (1, false));
        assert_eq!((g3, ins3), (2, true));
        assert_eq!(table.get_or_insert(7, 99).unwrap(), (0, false));
    }

    #[test]
    fn murmur_finalizer_spreads_high_bit_entropy_into_the_low_bits() {
        // Keys differing only above bit 32 must not all land in one slot
        // of a small table -- the failure mode of a bare multiply.
        let mask = 1023usize;
        let mut slots = std::collections::HashSet::new();
        for k in 0..1_000u64 {
            slots.insert((murmur_finalize(k << 40) as usize) & mask);
        }
        assert!(
            slots.len() > 600,
            "only {} distinct low-10-bit slots",
            slots.len()
        );
    }

    #[test]
    fn hash_group_table_distinguishes_colliding_hashes_by_the_callers_key_compare() {
        let mut table = HashGroupTable::with_capacity(1);
        assert_eq!(table.capacity(), 16);
        // Keys 0..999 all hash to the same value: the table must still
        // keep them apart through `same_key`, and find each again.
        let keys: Vec<usize> = (0..1_000).collect();
        for &k in &keys {
            let (g, inserted) = table
                .find_or_insert(0xDEAD_BEEF, k, |g| keys[g] == k)
                .unwrap();
            assert!(inserted);
            assert_eq!(g, k);
        }
        assert!(table.capacity() >= 2_000);
        for &k in &keys {
            let (g, inserted) = table
                .find_or_insert(0xDEAD_BEEF, usize::MAX, |g| keys[g] == k)
                .unwrap();
            assert!(!inserted);
            assert_eq!(g, k);
        }
        // A different hash never matches even when `same_key` would.
        let (g, inserted) = table.find_or_insert(1, 1_000, |_| true).unwrap();
        assert!(inserted);
        assert_eq!(g, 1_000);
    }

    // vm_int_key_table_find_or_insert_aee09802 (`HashGroupTable::find_or_insert`):
    // `candidate == hash && same_key(id as usize)`
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_int_key_table_find_or_insert_aee09802__v1_matching_hash_and_key_finds_the_group() {
        let mut table = HashGroupTable::with_capacity(4);
        let keys = [10usize, 20];
        assert_eq!(
            table.find_or_insert(0xA, 0, |g| keys[g] == 10).unwrap(),
            (0, true)
        );
        // Same hash, and the caller's compare agrees: the existing group.
        assert_eq!(
            table
                .find_or_insert(0xA, usize::MAX, |g| keys[g] == 10)
                .unwrap(),
            (0, false)
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_int_key_table_find_or_insert_aee09802__v2_matching_hash_but_a_different_key_inserts(
    ) {
        let mut table = HashGroupTable::with_capacity(4);
        let keys = [10usize, 20];
        assert_eq!(
            table.find_or_insert(0xA, 0, |g| keys[g] == 10).unwrap(),
            (0, true)
        );
        // A hash collision: same hash, the compare says a different key.
        assert_eq!(
            table.find_or_insert(0xA, 1, |g| keys[g] == 20).unwrap(),
            (1, true)
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn mcdc__vm_int_key_table_find_or_insert_aee09802__v3_a_different_hash_probes_past_the_slot() {
        let mut table = HashGroupTable::with_capacity(4);
        let keys = [10usize, 20];
        assert_eq!(
            table.find_or_insert(0xA, 0, |g| keys[g] == 10).unwrap(),
            (0, true)
        );
        // A different hash that lands on the same slot must not match
        // even though the compare would have said yes.
        let colliding = (0..u64::MAX)
            .skip(1)
            .find(|h| *h != 0xA && table.slot_of(*h) == table.slot_of(0xA))
            .unwrap();
        assert_eq!(
            table.find_or_insert(colliding, 1, |_| true).unwrap(),
            (1, true)
        );
    }
}
