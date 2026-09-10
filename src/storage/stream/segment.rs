//! `Segment`: a sealed, immutable, owned run of parsed log lines with its
//! skip indexes.
//!
//! A segment owns its bytes (`Arc<[u8]>`) and stores string columns as
//! offsets into them, so it is `'static` and can live in a ring behind an
//! `Arc`. Sealing parses a [`Block`] with a per-line parser into a
//! transient [`LogBatch`] and converts it to this owned form.

use std::ops::Range;
use std::sync::Arc;

use super::batch::{Facility, FieldColumn, LogBatch, Resource, Severity, Source};
use super::file::Block;
use super::syslog::SyslogParser;

/// Maximum rows per segment; a block with more lines is split.
pub const SEGMENT_MAX_ROWS: usize = 4096;

/// `(start, len)` into a segment's bytes.
pub type Span = (u32, u32);

/// Inclusive `(min, max)` over the non-null values of a column.
pub type MinMax = (i64, i64);

/// An owned Tier-3 column.
#[derive(Debug, Clone)]
pub enum OwnedColumn {
    /// Dictionary-encoded strings; the dictionary is small by construction.
    Dict {
        /// Unique values.
        dict: Vec<Arc<str>>,
        /// Per-row index into `dict`.
        indices: Vec<Option<u16>>,
    },
    /// High-cardinality strings as spans into the segment bytes.
    Str(Vec<Option<Span>>),
    /// Integers.
    Int(Vec<Option<i64>>),
    /// Floats.
    Float(Vec<Option<f64>>),
    /// Booleans.
    Bool(Vec<Option<bool>>),
}

impl OwnedColumn {
    /// Rows in the column.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Dict { indices, .. } => indices.len(),
            Self::Str(v) => v.len(),
            Self::Int(v) => v.len(),
            Self::Float(v) => v.len(),
            Self::Bool(v) => v.len(),
        }
    }

    /// Whether the column has no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether a dictionary column can contain `value` at all. `None` for
    /// non-dictionary columns (no skip decision possible).
    #[must_use]
    pub fn dict_contains(&self, value: &str) -> Option<bool> {
        match self {
            Self::Dict { dict, .. } => Some(dict.iter().any(|d| d.as_ref() == value)),
            _ => None,
        }
    }
}

/// A sealed segment.
#[derive(Debug)]
pub struct Segment {
    bytes: Arc<[u8]>,
    file_off: u64,
    lines: Vec<Span>,
    message: Vec<Option<Span>>,
    event_ts_ns: Vec<Option<i64>>,
    observed_ts_ns: Vec<Option<i64>>,
    severity: Vec<Option<Severity>>,
    facility: Vec<Option<Facility>>,
    field_names: Vec<Arc<str>>,
    field_cols: Vec<OwnedColumn>,
    minmax_event: Option<MinMax>,
    minmax_observed: Option<MinMax>,
    source: Source,
    resource: Resource,
}

impl Segment {
    /// Parse a block into one or more segments (split at
    /// [`SEGMENT_MAX_ROWS`]). Every line gets `observed_ts_ns`.
    #[must_use]
    pub fn seal_block(
        block: &Block,
        source: &Source,
        parser: &SyslogParser,
        observed_ts_ns: i64,
    ) -> Vec<Segment> {
        let mut out = Vec::new();
        let mut off: usize = 0;
        while off < block.bytes.len() {
            let rest = block.bytes.get(off..).unwrap_or(&[]);
            let (mut batch, consumed) = parser.parse_batch(source.clone(), rest, SEGMENT_MAX_ROWS);
            if consumed == 0 || batch.is_empty() {
                break;
            }
            batch.fill_observed_ts(observed_ts_ns);
            let seg_off = block.file_off.saturating_add(off as u64);
            out.push(Self::from_batch(
                &batch,
                Arc::clone(&block.bytes),
                off,
                seg_off,
            ));
            off = off.saturating_add(consumed);
        }
        out
    }

    /// Convert a transient batch borrowed from `bytes[base..]` into an owned
    /// segment. Strings that do not point into `bytes` become `None`.
    fn from_batch(batch: &LogBatch<'_>, bytes: Arc<[u8]>, base: usize, file_off: u64) -> Self {
        let span_of = |s: &[u8]| -> Option<Span> { span_in(&bytes, s) };
        let lines: Vec<Span> = batch
            .raw
            .iter()
            .map(|r| span_of(r).unwrap_or((base as u32, 0)))
            .collect();
        let message = batch
            .message
            .iter()
            .map(|m| m.and_then(|s| span_of(s.as_bytes())))
            .collect();

        let mut field_names = Vec::new();
        let mut field_cols = Vec::new();
        for name in batch.fields.names() {
            if let Some(col) = batch.fields.get(name) {
                field_names.push(Arc::<str>::from(name));
                field_cols.push(own_column(col, &bytes, batch.len()));
            }
        }

        Self {
            minmax_event: minmax(&batch.timestamp_ns),
            minmax_observed: minmax(&batch.observed_ts_ns),
            lines,
            message,
            event_ts_ns: batch.timestamp_ns.clone(),
            observed_ts_ns: batch.observed_ts_ns.clone(),
            severity: batch.severity.clone(),
            facility: batch.facility.clone(),
            field_names,
            field_cols,
            source: batch.source.clone(),
            resource: batch.resource.clone(),
            bytes,
            file_off,
        }
    }

    /// Rows in the segment.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Whether the segment has no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Bytes owned by this segment (the whole block it was cut from).
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.bytes.len()
    }

    /// File offset of the first line.
    #[must_use]
    pub const fn file_off(&self) -> u64 {
        self.file_off
    }

    /// File offset one past the last line.
    #[must_use]
    pub fn end_off(&self) -> u64 {
        match self.lines.last() {
            Some(&(start, len)) => {
                let block_end = u64::from(start)
                    .saturating_add(u64::from(len))
                    .saturating_add(1);
                // `start` is relative to `bytes`; the block's file offset is
                // `file_off - first_line_start`.
                let first = self.lines.first().map_or(0, |&(s, _)| u64::from(s));
                self.file_off
                    .saturating_sub(first)
                    .saturating_add(block_end)
            }
            None => self.file_off,
        }
    }

    /// The raw line at `i` (empty if out of range).
    #[must_use]
    pub fn raw_line(&self, i: usize) -> &[u8] {
        self.lines.get(i).map_or(&[], |&s| self.slice(s))
    }

    /// The message at `i`.
    #[must_use]
    pub fn message(&self, i: usize) -> Option<&str> {
        let span = (*self.message.get(i)?)?;
        std::str::from_utf8(self.slice(span)).ok()
    }

    /// Event timestamps (nanoseconds since epoch).
    #[must_use]
    pub fn event_ts_ns(&self) -> &[Option<i64>] {
        &self.event_ts_ns
    }

    /// Observed timestamps (nanoseconds since epoch).
    #[must_use]
    pub fn observed_ts_ns(&self) -> &[Option<i64>] {
        &self.observed_ts_ns
    }

    /// Severities.
    #[must_use]
    pub fn severity(&self) -> &[Option<Severity>] {
        &self.severity
    }

    /// Facilities.
    #[must_use]
    pub fn facility(&self) -> &[Option<Facility>] {
        &self.facility
    }

    /// Min/max of the non-null event timestamps.
    #[must_use]
    pub const fn minmax_event(&self) -> Option<MinMax> {
        self.minmax_event
    }

    /// Min/max of the non-null observed timestamps.
    #[must_use]
    pub const fn minmax_observed(&self) -> Option<MinMax> {
        self.minmax_observed
    }

    /// Whether any event timestamp may fall in `range` (segments without
    /// timestamps are conservatively kept).
    #[must_use]
    pub fn overlaps_event(&self, range: &Range<i64>) -> bool {
        match self.minmax_event {
            Some((lo, hi)) => lo < range.end && hi >= range.start,
            None => true,
        }
    }

    /// Tier-3 field names.
    pub fn field_names(&self) -> impl Iterator<Item = &str> {
        self.field_names.iter().map(|s| s.as_ref())
    }

    /// A Tier-3 column by name.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&OwnedColumn> {
        let i = self.field_names.iter().position(|n| n.as_ref() == name)?;
        self.field_cols.get(i)
    }

    /// Resolve a `Str` span or `Dict` index of `column` at row `i`.
    #[must_use]
    pub fn field_str(&self, name: &str, i: usize) -> Option<&str> {
        match self.field(name)? {
            OwnedColumn::Dict { dict, indices } => {
                let idx = (*indices.get(i)?)?;
                dict.get(usize::from(idx)).map(|s| s.as_ref())
            }
            OwnedColumn::Str(spans) => {
                let span = (*spans.get(i)?)?;
                std::str::from_utf8(self.slice(span)).ok()
            }
            _ => None,
        }
    }

    /// Source metadata.
    #[must_use]
    pub const fn source(&self) -> &Source {
        &self.source
    }

    /// Resource metadata.
    #[must_use]
    pub const fn resource(&self) -> &Resource {
        &self.resource
    }

    fn slice(&self, (start, len): Span) -> &[u8] {
        let s = start as usize;
        let e = s.saturating_add(len as usize);
        self.bytes.get(s..e).unwrap_or(&[])
    }
}

/// Where `s` sits inside `bytes`, if it does.
fn span_in(bytes: &[u8], s: &[u8]) -> Option<Span> {
    let base = bytes.as_ptr() as usize;
    let p = s.as_ptr() as usize;
    let off = p.checked_sub(base)?;
    let end = off.checked_add(s.len())?;
    if end > bytes.len() {
        return None;
    }
    Some((u32::try_from(off).ok()?, u32::try_from(s.len()).ok()?))
}

fn minmax(col: &[Option<i64>]) -> Option<MinMax> {
    let mut acc: Option<MinMax> = None;
    for v in col.iter().flatten() {
        acc = Some(match acc {
            None => (*v, *v),
            Some((lo, hi)) => (lo.min(*v), hi.max(*v)),
        });
    }
    acc
}

/// Convert a borrowed column to an owned one, padding to `rows`.
fn own_column(col: &FieldColumn<'_>, bytes: &[u8], rows: usize) -> OwnedColumn {
    let pad = |n: usize| rows.saturating_sub(n);
    match col {
        FieldColumn::Dict { dict, indices } => {
            let mut idx = indices.clone();
            idx.extend(std::iter::repeat_n(None, pad(idx.len())));
            OwnedColumn::Dict {
                dict: dict.iter().map(|s| Arc::<str>::from(*s)).collect(),
                indices: idx,
            }
        }
        FieldColumn::Str(strs) => {
            let mut spans: Vec<Option<Span>> = strs
                .iter()
                .map(|s| s.and_then(|s| span_in(bytes, s.as_bytes())))
                .collect();
            spans.extend(std::iter::repeat_n(None, pad(spans.len())));
            OwnedColumn::Str(spans)
        }
        FieldColumn::Int(v) => {
            let mut v = v.clone();
            v.extend(std::iter::repeat_n(None, pad(v.len())));
            OwnedColumn::Int(v)
        }
        FieldColumn::Float(v) => {
            let mut v = v.clone();
            v.extend(std::iter::repeat_n(None, pad(v.len())));
            OwnedColumn::Float(v)
        }
        FieldColumn::Bool(v) => {
            let mut v = v.clone();
            v.extend(std::iter::repeat_n(None, pad(v.len())));
            OwnedColumn::Bool(v)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::stream::SourceKind;

    fn block(text: &str) -> Block {
        Block {
            file_off: 1000,
            bytes: Arc::from(text.as_bytes()),
        }
    }
    fn src() -> Source {
        Source::new(SourceKind::File, "/var/log/t.log")
    }

    #[test]
    fn seal_owns_bytes_and_resolves_spans() {
        let b = block("<134>Sep 10 08:00:01 web01 nginx[12]: GET /a 200\n<131>Sep 10 08:00:02 web01 postgres[7]: ERROR: nope\n");
        let segs = Segment::seal_block(&b, &src(), &SyslogParser::with_year(2026), 42);
        assert_eq!(segs.len(), 1);
        let s = &segs[0];
        assert_eq!(s.len(), 2);
        assert_eq!(s.file_off(), 1000);
        assert_eq!(s.end_off(), 1000 + b.bytes.len() as u64);
        assert!(s.raw_line(0).starts_with(b"<134>"));
        assert_eq!(s.message(1), Some("ERROR: nope"));
        assert_eq!(s.severity()[1], Some(Severity::Error));
        assert_eq!(s.facility()[0], Some(Facility::Local0));
        assert_eq!(s.observed_ts_ns(), &[Some(42), Some(42)]);
        assert_eq!(s.field_str("tag", 0), Some("nginx"));
        assert_eq!(s.field_str("pid", 1), Some("7"));
        assert_eq!(s.field("tag").unwrap().dict_contains("nginx"), Some(true));
        assert_eq!(s.field("tag").unwrap().dict_contains("sshd"), Some(false));
    }

    #[test]
    fn minmax_and_overlap() {
        let b = block("<134>Sep 10 08:00:01 h a: x\n<134>Sep 10 08:00:05 h a: y\n");
        let segs = Segment::seal_block(&b, &src(), &SyslogParser::with_year(2026), 0);
        let s = &segs[0];
        let (lo, hi) = s.minmax_event().unwrap();
        assert!(hi > lo);
        assert!(s.overlaps_event(&(lo..hi + 1)));
        assert!(!s.overlaps_event(&(hi + 1..hi + 2)));
        assert!(!s.overlaps_event(&(lo - 2..lo)));
    }

    #[test]
    fn splits_at_max_rows() {
        let mut text = String::new();
        for i in 0..(SEGMENT_MAX_ROWS + 10) {
            text.push_str(&format!("<134>Sep 10 08:00:01 h a: {i}\n"));
        }
        let segs = Segment::seal_block(&block(&text), &src(), &SyslogParser::with_year(2026), 0);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].len(), SEGMENT_MAX_ROWS);
        assert_eq!(segs[1].len(), 10);
        assert_eq!(segs[0].end_off(), segs[1].file_off());
    }

    #[test]
    fn foreign_str_becomes_none_not_panic() {
        assert_eq!(span_in(b"abc", b"zzz"), None);
        let bytes = b"hello world";
        assert_eq!(span_in(bytes, &bytes[6..]), Some((6, 5)));
    }
}
