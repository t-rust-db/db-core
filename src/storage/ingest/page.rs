//! Pages (ADR 0027): the in-memory tier between the WAL and Parquet.
//!
//! Every record is one line (a JSON object, a logfmt line, plain text...,
//! per the table's format). The head page collects lines with their LSNs
//! and observed timestamps; sealing parses it into an ADR 0018
//! [`Segment`] (Tier-1 `raw`, Tier-2 timestamps/severity/message, Tier-3
//! fields), immutable and shared. A page covers a contiguous LSN range.
//!
//! In the WAL a record is `[observed_ts i64 LE][line]`, so a replayed
//! record keeps the arrival time it was given.
//!
//! A record table's (#594) pages hold typed rows instead: a [`RowPage`].

use std::sync::Arc;

use super::record::{RecordValue, RowPage};
use crate::storage::parquet::writer::ColumnSpec;
use crate::storage::stream::detect::DetectedParser;
use crate::storage::stream::segment::SEGMENT_MAX_ROWS;
use crate::storage::stream::{Segment, Source};

/// A sealed page: its rows and the LSNs it holds.
#[derive(Debug, Clone)]
pub struct Page {
    /// First LSN.
    pub lsn_lo: u64,
    /// Last LSN.
    pub lsn_hi: u64,
    /// The rows, one per LSN.
    pub body: PageBody,
}

/// A page's rows: parsed lines, or a record table's typed rows.
#[derive(Debug, Clone)]
pub enum PageBody {
    /// A lines table's: the parsed segment.
    Lines(Arc<Segment>),
    /// A record table's (#594).
    Rows(Arc<RowPage>),
}

impl Page {
    /// Rows in the page.
    pub fn len(&self) -> usize {
        match &self.body {
            PageBody::Lines(segment) => segment.len(),
            PageBody::Rows(rows) => rows.len(),
        }
    }

    /// No rows.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The parsed segment of a lines table's page.
    pub fn lines(&self) -> Option<&Arc<Segment>> {
        match &self.body {
            PageBody::Lines(segment) => Some(segment),
            PageBody::Rows(_) => None,
        }
    }

    /// Each row's arrival time (ns).
    pub fn observed_ts_ns(&self) -> Vec<Option<i64>> {
        match &self.body {
            PageBody::Lines(segment) => segment.observed_ts_ns().to_vec(),
            PageBody::Rows(rows) => rows.observed_ts_ns().iter().copied().map(Some).collect(),
        }
    }

    /// The first row's arrival time.
    pub fn first_observed(&self) -> Option<i64> {
        match &self.body {
            PageBody::Lines(segment) => segment.observed_ts_ns().first().copied().flatten(),
            PageBody::Rows(rows) => rows.observed_ts_ns().first().copied(),
        }
    }
}

/// The WAL payload for a record.
pub(crate) fn encode_record(observed_ts_ns: i64, line: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(line.len().saturating_add(8));
    out.extend_from_slice(&observed_ts_ns.to_le_bytes());
    out.extend_from_slice(line);
    out
}

/// The observed timestamp and line of a WAL payload; `None` if it is
/// shorter than the timestamp.
pub(crate) fn decode_record(payload: &[u8]) -> Option<(i64, &[u8])> {
    let ts: [u8; 8] = payload.get(..8)?.try_into().ok()?;
    Some((i64::from_le_bytes(ts), payload.get(8..)?))
}

/// Why a line cannot be a record.
pub(crate) fn invalid_line(line: &[u8]) -> Option<String> {
    if line.is_empty() {
        return Some("a record is one non-empty line".to_string());
    }
    if line.contains(&b'\n') {
        return Some("a record is one line: it contains a newline".to_string());
    }
    None
}

/// The mutable head: lines (or, for a record table, rows) not yet sealed.
#[derive(Debug, Default)]
pub(crate) struct HeadPage {
    lsn_lo: u64,
    bytes: Vec<u8>,
    observed: Vec<i64>,
    /// A record table's rows; `None` for a lines table.
    rows: Option<RowPage>,
}

impl HeadPage {
    /// An empty head for a record table with `schema`.
    pub(crate) fn for_records(schema: Arc<[ColumnSpec]>) -> HeadPage {
        HeadPage {
            rows: Some(RowPage::new(schema)),
            ..HeadPage::default()
        }
    }

    /// Appends row `lsn` of a record table (checked against its schema),
    /// which must follow the head's last LSN.
    pub(crate) fn push_row(&mut self, lsn: u64, observed_ts_ns: i64, row: Vec<RecordValue>) {
        if self.observed.is_empty() {
            self.lsn_lo = lsn;
        }
        self.observed.push(observed_ts_ns);
        if let Some(rows) = &mut self.rows {
            rows.push(observed_ts_ns, row);
        }
    }

    /// Rows in the head.
    pub(crate) fn len(&self) -> usize {
        self.observed.len()
    }

    /// The first row's arrival time, if any.
    pub(crate) fn first_observed(&self) -> Option<i64> {
        self.observed.first().copied()
    }

    pub(crate) fn is_full(&self) -> bool {
        self.len() >= SEGMENT_MAX_ROWS
    }

    /// Appends record `lsn`, which must follow the head's last LSN.
    pub(crate) fn push(&mut self, lsn: u64, observed_ts_ns: i64, line: &[u8]) {
        if self.observed.is_empty() {
            self.lsn_lo = lsn;
        }
        self.bytes.extend_from_slice(line);
        self.bytes.push(b'\n');
        self.observed.push(observed_ts_ns);
    }

    /// The head as a page, leaving it unchanged (a query's copy); `None`
    /// when empty or when the parser rejects the lines.
    pub(crate) fn snapshot(&self, source: &Source, parser: &DetectedParser) -> Option<Page> {
        let count = u64::try_from(self.len()).ok()?;
        let lsn_hi = self.lsn_lo.checked_add(count)?.checked_sub(1)?;
        if let Some(rows) = &self.rows {
            return Some(Page {
                lsn_lo: self.lsn_lo,
                lsn_hi,
                body: PageBody::Rows(Arc::new(rows.clone())),
            });
        }
        let segment = Segment::seal_lines(
            Arc::from(self.bytes.as_slice()),
            self.lsn_lo,
            source,
            parser,
            &self.observed,
        )?;
        Some(Page {
            lsn_lo: self.lsn_lo,
            lsn_hi,
            body: PageBody::Lines(Arc::new(segment)),
        })
    }

    /// Empties the head.
    pub(crate) fn clear(&mut self) {
        self.bytes.clear();
        self.observed.clear();
        if let Some(rows) = &mut self.rows {
            *rows = rows.cleared();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::stream::detect::Format;
    use crate::storage::stream::SourceKind;

    #[test]
    fn records_round_trip_through_the_wal_payload() {
        let payload = encode_record(-5, b"{\"a\":1}");
        assert_eq!(decode_record(&payload), Some((-5, &b"{\"a\":1}"[..])));
        assert_eq!(decode_record(&payload[..7]), None);
        assert_eq!(decode_record(&encode_record(9, b"")), Some((9, &b""[..])));
    }

    #[test]
    fn lines_must_be_one_non_empty_line() {
        assert!(invalid_line(b"").is_some());
        assert!(invalid_line(b"a\nb").is_some());
        assert!(invalid_line(b"a b\tc\r").is_none());
    }

    #[test]
    fn the_head_seals_into_a_page_over_its_lsns() {
        let source = Source::new(SourceKind::Stdin, "t");
        let parser = DetectedParser::for_format(Format::Jsonl);
        let mut head = HeadPage::default();
        assert!(head.snapshot(&source, &parser).is_none());
        head.push(41, 100, b"{\"level\":\"info\",\"n\":1}");
        head.push(42, 200, b"not json at all");
        assert!(!head.is_full());
        let page = head.snapshot(&source, &parser).unwrap();
        assert_eq!((page.lsn_lo, page.lsn_hi), (41, 42));
        assert_eq!(page.lines().unwrap().len(), 2);
        assert_eq!(
            page.lines().unwrap().observed_ts_ns(),
            &[Some(100), Some(200)]
        );
        head.clear();
        assert_eq!(head.len(), 0);
        head.push(43, 300, b"{}");
        assert_eq!(head.snapshot(&source, &parser).unwrap().lsn_lo, 43);
    }
}
