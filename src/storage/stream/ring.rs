//! `Ring`: the bounded set of hot segments over a live file.
//!
//! New segments enter at the head, older ones may be filled in at the
//! tail, and whole segments are evicted from the tail when the byte budget
//! is exceeded. The ring is a cache over the file: an evicted segment is a
//! byte range that can be rebuilt on demand.

use std::collections::VecDeque;
use std::ops::Range;
use std::sync::Arc;

use super::segment::Segment;

/// Hot segments, oldest at the front.
#[derive(Debug)]
pub struct Ring {
    segs: VecDeque<Arc<Segment>>,
    bytes: usize,
    budget: usize,
}

impl Ring {
    /// A ring holding at most `budget` bytes of segment data.
    #[must_use]
    pub fn new(budget: usize) -> Self {
        Self {
            segs: VecDeque::new(),
            bytes: 0,
            budget,
        }
    }

    /// Byte budget.
    #[must_use]
    pub const fn budget(&self) -> usize {
        self.budget
    }

    /// Change the budget; evicts from the tail if now over.
    pub fn set_budget(&mut self, budget: usize) -> Vec<Arc<Segment>> {
        self.budget = budget;
        self.evict_over_budget()
    }

    /// Bytes currently held.
    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.bytes
    }

    /// Segments held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.segs.len()
    }

    /// Whether no segment is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.segs.is_empty()
    }

    /// Total rows held.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.segs.iter().map(|s| s.len()).sum()
    }

    /// Append a newer segment at the head. Returns segments evicted from
    /// the tail to stay within budget.
    pub fn push_head(&mut self, seg: Arc<Segment>) -> Vec<Arc<Segment>> {
        self.bytes = self.bytes.saturating_add(seg.byte_len());
        self.segs.push_back(seg);
        self.evict_over_budget()
    }

    /// Insert an older segment at the tail (backwards fill). If it does
    /// not fit, it is returned as evicted immediately and the caller should
    /// stop filling.
    pub fn push_tail(&mut self, seg: Arc<Segment>) -> Vec<Arc<Segment>> {
        self.bytes = self.bytes.saturating_add(seg.byte_len());
        self.segs.push_front(seg);
        self.evict_over_budget()
    }

    /// Drop every segment.
    pub fn clear(&mut self) {
        self.segs.clear();
        self.bytes = 0;
    }

    /// Segments oldest first.
    pub fn segments(&self) -> impl Iterator<Item = &Arc<Segment>> {
        self.segs.iter()
    }

    /// Segments whose event-time minmax overlaps `range` (segments without
    /// timestamps are kept).
    pub fn overlapping_event(&self, range: &Range<i64>) -> Vec<Arc<Segment>> {
        self.segs
            .iter()
            .filter(|s| s.overlaps_event(range))
            .cloned()
            .collect()
    }

    /// File offset of the earliest held line.
    #[must_use]
    pub fn tail_off(&self) -> Option<u64> {
        self.segs.front().map(|s| s.file_off())
    }

    /// File offset one past the latest held line.
    #[must_use]
    pub fn head_off(&self) -> Option<u64> {
        self.segs.back().map(|s| s.end_off())
    }

    fn evict_over_budget(&mut self) -> Vec<Arc<Segment>> {
        let mut evicted = Vec::new();
        while self.bytes > self.budget && self.segs.len() > 1 {
            if let Some(seg) = self.segs.pop_front() {
                self.bytes = self.bytes.saturating_sub(seg.byte_len());
                evicted.push(seg);
            }
        }
        // A single segment larger than the budget is kept: the ring always
        // holds the head.
        evicted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::stream::{Block, Source, SourceKind, SyslogParser};

    fn seg(off: u64, secs: u32, n: usize) -> Arc<Segment> {
        let mut text = String::new();
        for i in 0..n {
            text.push_str(&format!(
                "<134>Sep 10 08:00:{secs:02} h app[{i}]: msg {i}\n"
            ));
        }
        let b = Block {
            file_off: off,
            bytes: Arc::from(text.as_bytes()),
        };
        let mut v = Segment::seal_block(
            &b,
            &Source::new(SourceKind::File, "t"),
            &SyslogParser::with_year(2026),
            0,
        );
        Arc::new(v.remove(0))
    }

    #[test]
    fn evicts_oldest_whole_segments_at_budget() {
        let a = seg(0, 1, 10);
        let size = a.byte_len();
        let mut ring = Ring::new(size * 2 + 1);
        assert!(ring.push_head(a).is_empty());
        assert!(ring.push_head(seg(1000, 2, 10)).is_empty());
        let ev = ring.push_head(seg(2000, 3, 10));
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].file_off(), 0);
        assert_eq!(ring.len(), 2);
        assert_eq!(ring.tail_off(), Some(1000));
        assert!(ring.bytes() <= ring.budget());
    }

    #[test]
    fn head_is_always_kept_even_over_budget() {
        let mut ring = Ring::new(1);
        assert!(ring.push_head(seg(0, 1, 10)).is_empty());
        assert_eq!(ring.len(), 1);
    }

    #[test]
    fn push_tail_fills_backwards_and_reports_when_full() {
        let a = seg(1000, 2, 10);
        let size = a.byte_len();
        let mut ring = Ring::new(size + 1);
        ring.push_head(a);
        let ev = ring.push_tail(seg(0, 1, 10));
        assert_eq!(ev.len(), 1, "the older segment did not fit and comes back");
        assert_eq!(ev[0].file_off(), 0);
        assert_eq!(ring.tail_off(), Some(1000));
    }

    #[test]
    fn overlapping_event_prunes_by_minmax() {
        let mut ring = Ring::new(usize::MAX);
        ring.push_head(seg(0, 1, 5));
        ring.push_head(seg(1000, 5, 5));
        ring.push_head(seg(2000, 9, 5));
        let (lo, _) = ring.segments().nth(1).unwrap().minmax_event().unwrap();
        let hit = ring.overlapping_event(&(lo..lo + 1));
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].file_off(), 1000);
    }

    #[test]
    fn shrinking_budget_evicts() {
        let mut ring = Ring::new(usize::MAX);
        for i in 0..5u64 {
            ring.push_head(seg(i * 1000, i as u32 + 1, 10));
        }
        let one = ring.segments().next().unwrap().byte_len();
        let ev = ring.set_budget(one * 2);
        assert_eq!(ev.len(), 3);
        assert_eq!(ring.len(), 2);
    }
}
