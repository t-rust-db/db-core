// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! The ranges a query read from the store, kept under a byte budget and
//! evicted least recently used first (#1052, ADR 0045). A listed object
//! never changes, so a range is keyed by its object, offset and length and
//! never goes stale.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError};

/// An object's key, a range's offset and its length.
type RangeKey = (String, u64, usize);

/// Ranges read from the store, up to `budget` bytes in all.
#[derive(Debug)]
pub(super) struct RangeCache {
    budget: u64,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Each range's bytes and the tick of its last use.
    ranges: HashMap<RangeKey, (Arc<[u8]>, u64)>,
    /// The ranges by last use, oldest first.
    by_use: BTreeMap<u64, RangeKey>,
    /// The bytes held.
    bytes: u64,
    /// The next use's tick.
    tick: u64,
}

impl RangeCache {
    pub(super) fn new(budget: u64) -> RangeCache {
        RangeCache {
            budget,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// The range of `key` at `offset` of `len` bytes, if held; it becomes
    /// the most recently used.
    pub(super) fn get(&self, key: &str, offset: u64, len: usize) -> Option<Arc<[u8]>> {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let inner = &mut *inner;
        let range = (key.to_string(), offset, len);
        let tick = inner.tick;
        let (bytes, used) = inner.ranges.get_mut(&range)?;
        let bytes = Arc::clone(bytes);
        let last = std::mem::replace(used, tick);
        inner.by_use.remove(&last);
        inner.by_use.insert(tick, range);
        inner.tick = tick.saturating_add(1);
        Some(bytes)
    }

    /// Holds `bytes`, read from `key` at `offset`, evicting the least
    /// recently used ranges to stay within the budget. A range larger than
    /// the whole budget is not held.
    pub(super) fn insert(&self, key: &str, offset: u64, bytes: Arc<[u8]>) {
        let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if size > self.budget {
            return;
        }
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let inner = &mut *inner;
        let range = (key.to_string(), offset, bytes.len());
        let tick = inner.tick;
        inner.tick = tick.saturating_add(1);
        if let Some((_, last)) = inner.ranges.insert(range.clone(), (bytes, tick)) {
            // Read twice at once: the first copy goes, the bytes are the same.
            inner.by_use.remove(&last);
            inner.bytes = inner.bytes.saturating_sub(size);
        }
        inner.by_use.insert(tick, range);
        inner.bytes = inner.bytes.saturating_add(size);
        while inner.bytes > self.budget {
            let Some((_, oldest)) = inner.by_use.pop_first() else {
                break;
            };
            if let Some((old, _)) = inner.ranges.remove(&oldest) {
                let freed = u64::try_from(old.len()).unwrap_or(u64::MAX);
                inner.bytes = inner.bytes.saturating_sub(freed);
            }
        }
    }

    /// The bytes held.
    #[cfg(test)]
    pub(super) fn held(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .bytes
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn bytes(n: usize, fill: u8) -> Arc<[u8]> {
        Arc::from(vec![fill; n])
    }

    #[test]
    fn a_range_is_found_by_object_offset_and_length() {
        let c = RangeCache::new(100);
        c.insert("a", 0, bytes(10, 1));
        assert_eq!(c.get("a", 0, 10).unwrap()[0], 1);
        assert!(c.get("a", 0, 9).is_none());
        assert!(c.get("a", 1, 10).is_none());
        assert!(c.get("b", 0, 10).is_none());
    }

    #[test]
    fn the_least_recently_used_range_goes_first() {
        let c = RangeCache::new(30);
        c.insert("a", 0, bytes(10, 1));
        c.insert("b", 0, bytes(10, 2));
        c.insert("c", 0, bytes(10, 3));
        // `a` is used again, so `b` is now the oldest.
        assert!(c.get("a", 0, 10).is_some());
        c.insert("d", 0, bytes(10, 4));
        assert!(c.get("b", 0, 10).is_none());
        assert!(c.get("a", 0, 10).is_some());
        assert!(c.get("c", 0, 10).is_some());
        assert!(c.get("d", 0, 10).is_some());
        assert_eq!(c.held(), 30);
    }

    #[test]
    fn a_range_larger_than_the_budget_is_not_held() {
        let c = RangeCache::new(5);
        c.insert("a", 0, bytes(6, 1));
        assert!(c.get("a", 0, 6).is_none());
        assert_eq!(c.held(), 0);
    }

    #[test]
    fn the_same_range_inserted_twice_is_held_once() {
        let c = RangeCache::new(100);
        c.insert("a", 0, bytes(10, 1));
        c.insert("a", 0, bytes(10, 1));
        assert_eq!(c.held(), 10);
        // Eviction still finds every range it accounts for.
        c.insert("b", 0, bytes(95, 2));
        assert!(c.get("a", 0, 10).is_none());
        assert_eq!(c.held(), 95);
    }

    #[test]
    fn a_zero_budget_holds_nothing() {
        let c = RangeCache::new(0);
        c.insert("a", 0, bytes(1, 1));
        assert!(c.get("a", 0, 1).is_none());
        assert_eq!(c.held(), 0);
    }
}
