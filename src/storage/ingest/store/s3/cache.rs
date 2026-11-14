// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! The ranges a query read from the store, kept under a byte budget and
//! evicted least recently used first (#1052, ADR 0045). A listed object
//! never changes, so a range is keyed by its object and offset and never
//! goes stale. A read is answered by any held range that contains it
//! (#1059): one coalesced GET over several column chunks answers each
//! chunk's read.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError};

/// One object's held ranges by offset: the bytes and the tick of their
/// last use.
type Ranges = BTreeMap<u64, (Arc<[u8]>, u64)>;

/// Ranges read from the store, up to `budget` bytes in all.
#[derive(Debug)]
pub(super) struct RangeCache {
    budget: u64,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Per object, its ranges by offset: the bytes and the tick of their
    /// last use.
    objects: HashMap<String, Ranges>,
    /// The ranges by last use, oldest first: (object, offset).
    by_use: BTreeMap<u64, (String, u64)>,
    /// The bytes held.
    bytes: u64,
    /// The next use's tick.
    tick: u64,
}

impl Inner {
    /// The offset of a held range of `key` that holds `len` bytes from
    /// `offset`, if any.
    fn covering(&self, key: &str, offset: u64, len: usize) -> Option<u64> {
        let end = offset.checked_add(u64::try_from(len).ok()?)?;
        self.objects
            .get(key)?
            .range(..=offset)
            .rev()
            .find(|(start, (bytes, _))| {
                u64::try_from(bytes.len())
                    .ok()
                    .and_then(|n| start.checked_add(n))
                    .is_some_and(|stop| stop >= end)
            })
            .map(|(start, _)| *start)
    }
}

impl RangeCache {
    pub(super) fn new(budget: u64) -> RangeCache {
        RangeCache {
            budget,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// The budget: no range larger than this is held.
    pub(super) fn budget(&self) -> u64 {
        self.budget
    }

    /// Copies the `buf.len()` bytes of `key` at `offset` into `buf` if a
    /// held range contains them, which becomes the most recently used;
    /// `false` if none does.
    pub(super) fn read_into(&self, key: &str, offset: u64, buf: &mut [u8]) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let inner = &mut *inner;
        let Some(start) = inner.covering(key, offset, buf.len()) else {
            return false;
        };
        let tick = inner.tick;
        let Some((bytes, used)) = inner.objects.get_mut(key).and_then(|m| m.get_mut(&start)) else {
            return false;
        };
        let from = usize::try_from(offset.saturating_sub(start)).ok();
        let Some(src) = from.and_then(|f| bytes.get(f..f.checked_add(buf.len())?)) else {
            return false;
        };
        buf.copy_from_slice(src);
        let last = std::mem::replace(used, tick);
        inner.by_use.remove(&last);
        inner.by_use.insert(tick, (key.to_string(), start));
        inner.tick = tick.saturating_add(1);
        true
    }

    /// Whether a held range contains the `len` bytes of `key` at `offset`.
    pub(super) fn holds(&self, key: &str, offset: u64, len: u64) -> bool {
        let inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        usize::try_from(len).is_ok_and(|len| inner.covering(key, offset, len).is_some())
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
        let tick = inner.tick;
        inner.tick = tick.saturating_add(1);
        let ranges = inner.objects.entry(key.to_string()).or_default();
        if let Some((old, last)) = ranges.insert(offset, (bytes, tick)) {
            // Read twice at once, or a wider read at the same offset: the
            // old one goes.
            inner.by_use.remove(&last);
            inner.bytes = inner
                .bytes
                .saturating_sub(u64::try_from(old.len()).unwrap_or(u64::MAX));
        }
        inner.by_use.insert(tick, (key.to_string(), offset));
        inner.bytes = inner.bytes.saturating_add(size);
        while inner.bytes > self.budget {
            let Some((_, (oldest_key, oldest_at))) = inner.by_use.pop_first() else {
                break;
            };
            let freed = inner
                .objects
                .get_mut(&oldest_key)
                .and_then(|m| m.remove(&oldest_at))
                .map_or(0, |(old, _)| u64::try_from(old.len()).unwrap_or(u64::MAX));
            inner.bytes = inner.bytes.saturating_sub(freed);
            if inner
                .objects
                .get(&oldest_key)
                .is_some_and(BTreeMap::is_empty)
            {
                inner.objects.remove(&oldest_key);
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

    /// The `len` bytes of `key` at `offset`, if held.
    fn get(c: &RangeCache, key: &str, offset: u64, len: usize) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; len];
        c.read_into(key, offset, &mut buf).then_some(buf)
    }

    #[test]
    fn a_read_is_answered_by_a_range_that_contains_it() {
        let c = RangeCache::new(100);
        c.insert("a", 10, Arc::from((0u8..20).collect::<Vec<u8>>()));
        assert_eq!(
            get(&c, "a", 10, 20).unwrap(),
            (0u8..20).collect::<Vec<u8>>()
        );
        assert_eq!(get(&c, "a", 15, 3).unwrap(), vec![5, 6, 7]);
        assert_eq!(get(&c, "a", 29, 1).unwrap(), vec![19]);
        // Past either end, or another object: not held.
        assert!(get(&c, "a", 9, 2).is_none());
        assert!(get(&c, "a", 29, 2).is_none());
        assert!(get(&c, "b", 15, 3).is_none());
        assert!(c.holds("a", 12, 5));
        assert!(!c.holds("a", 25, 10));
    }

    #[test]
    fn a_narrow_range_does_not_hide_a_wider_one_before_it() {
        let c = RangeCache::new(100);
        c.insert("a", 0, bytes(50, 1));
        c.insert("a", 20, bytes(5, 2));
        // The range at 20 ends at 25; the one at 0 holds 30..40.
        assert_eq!(get(&c, "a", 30, 10).unwrap(), vec![1; 10]);
        assert_eq!(get(&c, "a", 21, 2).unwrap(), vec![2; 2]);
    }

    #[test]
    fn the_least_recently_used_range_goes_first() {
        let c = RangeCache::new(30);
        c.insert("a", 0, bytes(10, 1));
        c.insert("b", 0, bytes(10, 2));
        c.insert("c", 0, bytes(10, 3));
        // `a` is used again, so `b` is now the oldest.
        assert!(get(&c, "a", 0, 10).is_some());
        c.insert("d", 0, bytes(10, 4));
        assert!(get(&c, "b", 0, 10).is_none());
        assert!(get(&c, "a", 0, 10).is_some());
        assert!(get(&c, "c", 0, 10).is_some());
        assert!(get(&c, "d", 0, 10).is_some());
        assert_eq!(c.held(), 30);
    }

    #[test]
    fn a_range_larger_than_the_budget_is_not_held() {
        let c = RangeCache::new(5);
        assert_eq!(c.budget(), 5);
        c.insert("a", 0, bytes(6, 1));
        assert!(get(&c, "a", 0, 6).is_none());
        assert_eq!(c.held(), 0);
    }

    #[test]
    fn the_same_offset_inserted_again_replaces_the_old_range() {
        let c = RangeCache::new(100);
        c.insert("a", 0, bytes(10, 1));
        c.insert("a", 0, bytes(20, 2));
        assert_eq!(c.held(), 20);
        assert_eq!(get(&c, "a", 15, 5).unwrap(), vec![2; 5]);
        // Eviction still finds every range it accounts for.
        c.insert("b", 0, bytes(90, 3));
        assert!(get(&c, "a", 0, 1).is_none());
        assert_eq!(c.held(), 90);
    }

    #[test]
    fn a_zero_budget_holds_nothing() {
        let c = RangeCache::new(0);
        c.insert("a", 0, bytes(1, 1));
        assert!(get(&c, "a", 0, 1).is_none());
        assert_eq!(c.held(), 0);
    }
}
