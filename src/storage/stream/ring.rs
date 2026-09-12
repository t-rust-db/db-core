//! `Ring`: the bounded set of hot segments over a live file.
//!
//! New segments enter at the head, older ones may be filled in at the
//! tail, and whole segments are evicted from the tail when the byte budget
//! is exceeded. The ring is a cache over the file: an evicted segment is a
//! byte range that can be rebuilt on demand.

use std::collections::VecDeque;
use std::ops::Range;
use std::sync::Arc;

use super::segment::{MinMax, Segment, SegmentSummary};

/// Default horizon (#308): how far back retained summaries survive
/// eviction before this `Ring` drops them too, absent an explicit
/// [`Ring::set_summary_horizon`] call. Matches ADR 0018's own "count(*)
/// ... since 1d" example.
pub const DEFAULT_SUMMARY_HORIZON_NS: i64 = 24 * 60 * 60 * 1_000_000_000;

/// One evicted segment's surviving state (#308 "summaries survive
/// eviction"): its universal per-column stats plus both timestamp
/// minmaxes, so a query wider than the ring can still be answered for
/// `COUNT`/`SUM`/`MIN`/`MAX` without re-reading the segment's bytes.
#[derive(Debug, Clone)]
pub struct EvictedSummary {
    /// The evicted segment's `minmax_event`.
    pub minmax_event: Option<MinMax>,
    /// The evicted segment's `minmax_observed`.
    pub minmax_observed: Option<MinMax>,
    /// The evicted segment's universal summary.
    pub summary: SegmentSummary,
}

/// Hot segments, oldest at the front.
#[derive(Debug)]
pub struct Ring {
    segs: VecDeque<Arc<Segment>>,
    bytes: usize,
    budget: usize,
    /// Retained state for segments evicted from `segs`, oldest first,
    /// pruned to `summary_horizon_ns` behind the newest known observed
    /// time -- a second, unbounded-by-bytes structure (summaries are a
    /// few `f64`s each, not segment bytes) but still bounded in time.
    evicted: VecDeque<EvictedSummary>,
    summary_horizon_ns: i64,
}

impl Ring {
    /// A ring holding at most `budget` bytes of segment data, retaining
    /// evicted summaries for [`DEFAULT_SUMMARY_HORIZON_NS`].
    #[must_use]
    pub fn new(budget: usize) -> Self {
        Self {
            segs: VecDeque::new(),
            bytes: 0,
            budget,
            evicted: VecDeque::new(),
            summary_horizon_ns: DEFAULT_SUMMARY_HORIZON_NS,
        }
    }

    /// How far behind the newest known observed time a retained summary
    /// may fall before this ring drops it.
    pub fn set_summary_horizon(&mut self, horizon_ns: i64) {
        self.summary_horizon_ns = horizon_ns;
        self.prune_summaries();
    }

    /// Retained summaries for segments no longer held, oldest first.
    pub fn evicted_summaries(&self) -> impl Iterator<Item = &EvictedSummary> {
        self.evicted.iter()
    }

    /// Retained summaries whose `minmax_observed` overlaps `range`
    /// (a summary with no observed timestamps is conservatively kept,
    /// mirroring [`Segment::overlaps_event`]).
    pub fn summaries_overlapping(&self, range: &Range<i64>) -> Vec<&EvictedSummary> {
        self.evicted
            .iter()
            .filter(|e| match e.minmax_observed {
                Some((lo, hi)) => lo < range.end && hi >= range.start,
                None => true,
            })
            .collect()
    }

    /// The newest observed-time upper bound this ring knows about, from
    /// the live head if any, else the newest retained summary -- the
    /// reference point [`Self::prune_summaries`] measures the horizon
    /// against (data-time-driven, not wall-clock: a `Ring` with no
    /// `Clock` dependency stays trivially testable).
    fn newest_observed_hi(&self) -> Option<i64> {
        self.segs
            .back()
            .and_then(|s| s.minmax_observed())
            .map(|(_, hi)| hi)
            .or_else(|| {
                self.evicted
                    .back()
                    .and_then(|e| e.minmax_observed)
                    .map(|(_, hi)| hi)
            })
    }

    fn prune_summaries(&mut self) {
        let Some(newest) = self.newest_observed_hi() else {
            return;
        };
        while let Some(front) = self.evicted.front() {
            let hi = front.minmax_observed.map_or(i64::MIN, |(_, hi)| hi);
            if newest.saturating_sub(hi) > self.summary_horizon_ns {
                self.evicted.pop_front();
            } else {
                break;
            }
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

    /// Drop every segment and every retained summary (a truncation/
    /// rotation invalidates history the same way it invalidates the
    /// ring itself -- there is no "old file's tail" left to summarize).
    pub fn clear(&mut self) {
        self.segs.clear();
        self.bytes = 0;
        self.evicted.clear();
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
                self.evicted.push_back(EvictedSummary {
                    minmax_event: seg.minmax_event(),
                    minmax_observed: seg.minmax_observed(),
                    summary: seg.summary().clone(),
                });
                evicted.push(seg);
            }
        }
        // A single segment larger than the budget is kept: the ring always
        // holds the head.
        if !evicted.is_empty() {
            self.prune_summaries();
        }
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
    fn eviction_retains_a_summary_with_the_right_row_count() {
        let mut ring = Ring::new(usize::MAX);
        let a = seg(0, 1, 10);
        let a_rows = a.summary().rows;
        let size = a.byte_len();
        ring.push_head(a);
        ring.push_head(seg(1000, 2, 10));
        ring.set_budget(size); // evicts the first (only one fits now)
        let summaries: Vec<_> = ring.evicted_summaries().collect();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].summary.rows, a_rows);
    }

    #[test]
    fn summaries_overlapping_filters_by_observed_range_like_segments_do() {
        let mut ring = Ring::new(usize::MAX);
        ring.push_head(seg(0, 1, 5));
        ring.push_head(seg(1000, 5, 5));
        ring.push_head(seg(2000, 9, 5));
        // All three share one observed_ts (seal_block's `observed_ts_ns`
        // arg is `0` for every `seg()` in this test file), so the range
        // must cover it for any to match.
        let (lo, hi) = ring.segments().next().unwrap().minmax_observed().unwrap();
        ring.set_budget(1); // evict everything but the head
        assert!(!ring.evicted_summaries().collect::<Vec<_>>().is_empty());
        assert_eq!(ring.summaries_overlapping(&(lo..hi + 1)).len(), 2);
        assert_eq!(ring.summaries_overlapping(&(hi + 10..hi + 20)).len(), 0);
    }

    #[test]
    fn summary_horizon_drops_evicted_state_older_than_the_horizon() {
        let mut ring = Ring::new(usize::MAX);
        // Every `seg()` here shares `observed_ts_ns == 0` (the `seal_block`
        // arg below is always `0`), so exercise the horizon through event
        // time isn't possible with this helper; instead shrink the horizon
        // to before `newest_observed_hi()` (also `0`) so everything with
        // `hi <= 0 - 1` is pruned -- i.e. a horizon of `-1` prunes all.
        ring.push_head(seg(0, 1, 5));
        ring.push_head(seg(1000, 5, 5));
        ring.set_budget(1);
        assert_eq!(ring.evicted_summaries().count(), 1);
        ring.set_summary_horizon(-1);
        assert_eq!(ring.evicted_summaries().count(), 0);
    }

    #[test]
    fn clear_drops_retained_summaries_too() {
        let mut ring = Ring::new(usize::MAX);
        ring.push_head(seg(0, 1, 5));
        ring.push_head(seg(1000, 5, 5));
        ring.set_budget(1);
        assert_eq!(ring.evicted_summaries().count(), 1);
        ring.clear();
        assert_eq!(ring.evicted_summaries().count(), 0);
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

#[cfg(test)]
#[allow(non_snake_case)]
mod mcdc_vectors {
    //! Tagged MC/DC vectors for this file's multi-leaf decisions
    //! (`mcdc__<id>__vN`, joined to `tests/mcdc/obligations.json`
    //! by `make test-mcdc`; db-core#299 MC/DC backfill).

    use super::Ring;
    use crate::storage::stream::segment::Segment;
    use crate::storage::stream::{Block, Source, SourceKind, SyslogParser};
    use std::sync::Arc;

    fn make_seg(off: u64, secs: u32, n: usize) -> Arc<Segment> {
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

    // storage_stream_ring_evict_over_budget_9dad8d76: `self.bytes > self.budget && self.segs.len() > 1`
    #[test]
    fn mcdc__storage_stream_ring_evict_over_budget_9dad8d76__v1_both_true_evicts() {
        let a = make_seg(0, 1, 10);
        let size = a.byte_len();
        let mut ring = Ring::new(size + 1);
        ring.push_head(a);
        // Pushing a second segment makes bytes > budget (true) and
        // segs.len() > 1 (true) -- both leafs true, so it evicts.
        let ev = ring.push_head(make_seg(1000, 2, 10));
        assert_eq!(ev.len(), 1);
    }

    #[test]
    fn mcdc__storage_stream_ring_evict_over_budget_9dad8d76__v2_bytes_not_over_budget_no_eviction()
    {
        let a = make_seg(0, 1, 10);
        let size = a.byte_len();
        let mut ring = Ring::new(size * 10);
        ring.push_head(a);
        // bytes > budget is false (plenty of room) regardless of len(),
        // so no eviction happens.
        let ev = ring.push_head(make_seg(1000, 2, 10));
        assert!(ev.is_empty());
    }

    #[test]
    fn mcdc__storage_stream_ring_evict_over_budget_9dad8d76__v3_over_budget_but_single_segment_kept(
    ) {
        let mut ring = Ring::new(1);
        // bytes > budget is true (any segment exceeds budget of 1 byte),
        // but segs.len() > 1 is false (only one segment held) -- the
        // single head segment is always kept.
        let ev = ring.push_head(make_seg(0, 1, 10));
        assert!(ev.is_empty());
    }
}
