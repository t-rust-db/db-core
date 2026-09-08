//! Flat, open-addressing hash table for equality joins.
//!
//! This is a **multimap**: the same key may legitimately appear in more
//! than one entry (a foreign-key column on the build side is the common
//! case -- one dimension row's key matches many fact rows, or the caller
//! chose to build on the larger side). No entry is ever removed, so a
//! probe can prove "no match" the moment it reaches the first empty slot
//! along a key's probe sequence -- the standard open-addressing
//! correctness invariant.
//!
//! Chosen over `std::collections::HashMap<K, Vec<V>>` -- and especially
//! over `HashMap<String, Vec<usize>>`, which is what this replaces in
//! column-rs's `execute_joined` -- to avoid a heap allocation per distinct
//! key and per-row `String` formatting just to get something `Hash`.
//! Entries live in one flat `Vec`, resized by doubling, probed linearly.
//! See the `t-rust-db/benchmark` parity results this targets: column-rs's
//! `join` benchmark was 64x DuckDB's time and 4.4 GB RSS (vs. DuckDB's
//! ~86 MB) at 10M rows on the old `HashMap<String, Vec<usize>>`.

#![forbid(unsafe_code)]

use std::hash::{BuildHasher, Hash, RandomState};

struct Entry<K, V> {
    key: K,
    value: V,
}

/// Grow when the table would exceed a 70% load factor.
const MAX_LOAD_NUM: usize = 7;
const MAX_LOAD_DEN: usize = 10;
const DEFAULT_CAPACITY: usize = 16;

/// An open-addressing hash table keyed on join keys, holding one `V`
/// payload per inserted key (duplicates are kept as separate entries).
pub struct JoinHashTable<K, V, S = RandomState> {
    entries: Vec<Option<Entry<K, V>>>,
    hasher: S,
    len: usize,
}

impl<K: Hash + Eq, V> JoinHashTable<K, V, RandomState> {
    /// An empty table with [`DEFAULT_CAPACITY`] slots and a random hasher.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// `cap` is a hint (rounded up to the next power of two, minimum
    /// [`DEFAULT_CAPACITY`]) -- pass the build side's row count to avoid
    /// rehashing during a bulk insert.
    pub fn with_capacity(cap: usize) -> Self {
        Self::with_capacity_and_hasher(cap, RandomState::new())
    }
}

impl<K: Hash + Eq, V> Default for JoinHashTable<K, V, RandomState> {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "open addressing over a power-of-two table: `entries.len()` is never zero so `mask = len - 1` is valid, and `idx & mask` always names a live slot; `len + 1`, `steps + 1` and the doubling in `grow` are bounded by the table's own allocation"
)]
impl<K: Hash + Eq, V, S: BuildHasher> JoinHashTable<K, V, S> {
    /// Like [`JoinHashTable::with_capacity`], but with a caller-supplied hasher.
    pub fn with_capacity_and_hasher(cap: usize, hasher: S) -> Self {
        let cap = cap.next_power_of_two().max(DEFAULT_CAPACITY);
        let mut entries = Vec::with_capacity(cap);
        entries.resize_with(cap, || None);
        Self {
            entries,
            hasher,
            len: 0,
        }
    }

    /// Number of entries stored.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether no entries are stored.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Number of slots currently allocated (always a power of two).
    pub fn capacity(&self) -> usize {
        self.entries.len()
    }

    /// Slot-space hash of `key`. Truncating the `u64` hash to `usize` on
    /// a 32-bit target only discards high bits that the power-of-two
    /// `mask` in every caller would have dropped anyway.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "callers mask to `entries.len() - 1`; high bits are never used"
    )]
    fn hash_of(&self, key: &K) -> usize {
        self.hasher.hash_one(key) as usize
    }

    /// Insert one `(key, value)` pair. Never overwrites an existing entry
    /// -- duplicate keys are expected and each becomes its own entry (see
    /// module docs). Grows the table first if this insert would exceed the
    /// load factor.
    pub fn insert(&mut self, key: K, value: V) {
        if (self.len + 1) * MAX_LOAD_DEN > self.entries.len() * MAX_LOAD_NUM {
            self.grow();
        }
        let mask = self.entries.len() - 1;
        let mut idx = self.hash_of(&key) & mask;
        loop {
            if self.entries[idx].is_none() {
                self.entries[idx] = Some(Entry { key, value });
                self.len += 1;
                return;
            }
            idx = (idx + 1) & mask;
        }
    }

    fn grow(&mut self) {
        let new_cap = self.entries.len() * 2;
        let mut new_entries = Vec::with_capacity(new_cap);
        new_entries.resize_with(new_cap, || None);
        let old_entries = std::mem::replace(&mut self.entries, new_entries);
        self.len = 0;
        for slot in old_entries.into_iter().flatten() {
            self.insert_no_grow(slot.key, slot.value);
        }
    }

    /// Same as [`Self::insert`] but assumes capacity already suffices --
    /// used by [`Self::grow`] to avoid re-triggering growth mid-rehash.
    fn insert_no_grow(&mut self, key: K, value: V) {
        let mask = self.entries.len() - 1;
        let mut idx = self.hash_of(&key) & mask;
        loop {
            if self.entries[idx].is_none() {
                self.entries[idx] = Some(Entry { key, value });
                self.len += 1;
                return;
            }
            idx = (idx + 1) & mask;
        }
    }

    /// First matching value for `key`, if any. For a build side with
    /// unique keys (the common dimension-table case) this is the only
    /// lookup you need; for duplicate build keys use [`Self::get_all`].
    pub fn get(&self, key: &K) -> Option<&V> {
        let mask = self.entries.len() - 1;
        let cap = self.entries.len();
        let mut idx = self.hash_of(key) & mask;
        let mut steps = 0;
        while steps < cap {
            // empty slot proves no further match
            let entry = self.entries[idx].as_ref()?;
            if &entry.key == key {
                return Some(&entry.value);
            }
            idx = (idx + 1) & mask;
            steps += 1;
        }
        None
    }

    /// True if any entry matches `key`.
    pub fn contains_key(&self, key: &K) -> bool {
        self.get(key).is_some()
    }

    /// All values matching `key`, in insertion order. Stops at the first
    /// empty slot along the probe sequence, which proves no further match
    /// exists (see module docs). Collects eagerly rather than returning a
    /// lazy iterator -- a custom iterator struct here would have to borrow
    /// `self`/`key`, and this codebase's qualified subset (`make
    /// check-mvl-limit`) forbids the explicit struct lifetime that would
    /// require.
    pub fn get_all(&self, key: &K) -> Vec<&V> {
        let mask = self.entries.len() - 1;
        let cap = self.entries.len();
        let mut idx = self.hash_of(key) & mask;
        let mut steps = 0;
        let mut matches = Vec::new();
        while steps < cap {
            let Some(entry) = self.entries[idx].as_ref() else {
                break; // empty slot proves no further match
            };
            if &entry.key == key {
                matches.push(&entry.value);
            }
            idx = (idx + 1) & mask;
            steps += 1;
        }
        matches
    }

    /// Batch probe, first match only per key -- a straightforward loop
    /// over [`Self::get`] for now; a real SIMD-batched probe (hash many
    /// keys at once, gather matches) is future work, tracked as the next
    /// step after this lands (see `t-rust-db/db-core` README).
    pub fn probe_batch(&self, keys: &[K]) -> Vec<Option<&V>> {
        keys.iter().map(|k| self.get(k)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_get_roundtrip() {
        let mut ht: JoinHashTable<i64, &str> = JoinHashTable::new();
        ht.insert(1, "a");
        ht.insert(2, "b");
        assert_eq!(ht.get(&1), Some(&"a"));
        assert_eq!(ht.get(&2), Some(&"b"));
        assert_eq!(ht.get(&3), None);
        assert_eq!(ht.len(), 2);
    }

    #[test]
    fn duplicate_keys_are_a_multimap() {
        let mut ht: JoinHashTable<i64, &str> = JoinHashTable::new();
        ht.insert(1, "a");
        ht.insert(1, "b");
        ht.insert(1, "c");
        let mut matches: Vec<&&str> = ht.get_all(&1);
        matches.sort();
        assert_eq!(matches, vec![&"a", &"b", &"c"]);
        assert_eq!(ht.len(), 3);
    }

    #[test]
    fn get_on_empty_table_is_none() {
        let ht: JoinHashTable<i64, &str> = JoinHashTable::new();
        assert_eq!(ht.get(&1), None);
    }

    #[test]
    fn grows_past_initial_capacity_without_losing_entries() {
        let mut ht: JoinHashTable<i64, i64> = JoinHashTable::with_capacity(4);
        for i in 0..1000 {
            ht.insert(i, i * 10);
        }
        assert_eq!(ht.len(), 1000);
        for i in 0..1000 {
            assert_eq!(ht.get(&i), Some(&(i * 10)));
        }
    }

    #[test]
    fn string_keys_work_without_manual_to_string_hashing() {
        let mut ht: JoinHashTable<String, u32> = JoinHashTable::new();
        ht.insert("alice".to_string(), 1);
        ht.insert("bob".to_string(), 2);
        assert_eq!(ht.get(&"alice".to_string()), Some(&1));
        assert_eq!(ht.get(&"carol".to_string()), None);
    }

    #[test]
    fn contains_key_matches_get() {
        let mut ht: JoinHashTable<i64, ()> = JoinHashTable::new();
        ht.insert(5, ());
        assert!(ht.contains_key(&5));
        assert!(!ht.contains_key(&6));
    }

    #[test]
    fn probe_batch_returns_first_match_per_key() {
        let mut ht: JoinHashTable<i64, &str> = JoinHashTable::new();
        ht.insert(1, "one");
        ht.insert(2, "two");
        let keys = vec![1, 2, 3];
        let results = ht.probe_batch(&keys);
        assert_eq!(results, vec![Some(&"one"), Some(&"two"), None]);
    }

    #[test]
    fn many_duplicates_of_one_key_still_terminate_probe() {
        // Stress the "stop at first empty slot" invariant when almost every
        // entry shares a key -- probes for a present key must still return
        // all matches, and probes for an absent key must terminate.
        let mut ht: JoinHashTable<i64, i64> = JoinHashTable::with_capacity(4);
        for i in 0..50 {
            ht.insert(1, i);
        }
        assert_eq!(ht.get_all(&1).len(), 50);
        assert_eq!(ht.get(&2), None);
    }
}
