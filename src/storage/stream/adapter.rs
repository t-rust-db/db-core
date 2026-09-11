//! Adapters that present stream storage to the batch VM: a sealed
//! [`Segment`] as a `vm::batch::Segment` (bounded queries over the ring)
//! and a [`TailSource`] as a `vm::batch::Source` (the live head).
//!
//! Materialization is by name and only for the columns a program loads
//! (`Program::columns_to_load()`), so a `WHERE severity >= 13` touches one
//! `Vec<Value>` per segment, not the whole row.
//!
//! # Column model
//!
//! | Column | `Value` | From |
//! |---|---|---|
//! | `timestamp` | `Int` (ns since epoch) | `Segment::event_ts_ns` |
//! | `observed_ts` | `Int` (ns since epoch) | `Segment::observed_ts_ns` |
//! | `severity` | `Int` (OTel code: Trace 1 … Fatal 21) | `Segment::severity` |
//! | `severity_text` | `Str` (`TRACE` … `FATAL`) | `Segment::severity` |
//! | `facility` | `Str` (`kern`, `auth`, `local0`, …) | `Segment::facility` |
//! | `message` | `Str` | `Segment::message` |
//! | `raw` | `Str` (lossy UTF-8) | `Segment::raw_line` |
//! | any Tier-3 name | `Str`/`Int`/`Float`/`Bool` | `Segment::field` |
//!
//! `severity` is numeric so that `>=` orders correctly; a planner rewrites
//! `severity >= 'WARN'` to its code. An unknown column is a load error, never
//! a column of NULLs.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::file::{LogFile, Refresh};
use super::segment::{OwnedColumn, Segment};
use super::syslog::SyslogParser;
use super::Source;
use crate::vm::batch::{Batch, Segment as VmSegment, Source as VmSource, Value, VmError};

/// The predefined column names every stream table has, in display order.
pub const PREDEFINED_COLUMNS: [&str; 7] = [
    "timestamp",
    "observed_ts",
    "severity",
    "severity_text",
    "facility",
    "message",
    "raw",
];

/// A column request: the key the program uses (possibly qualified,
/// `log.severity`) and the bare column name to materialize.
#[derive(Debug, Clone)]
pub struct ColumnRequest {
    /// Key in the resulting [`Batch`].
    pub key: String,
    /// Column name to read from the segment.
    pub name: String,
}

impl ColumnRequest {
    /// A request whose key and name are the same.
    #[must_use]
    pub fn bare(name: &str) -> Self {
        Self {
            key: name.to_string(),
            name: name.to_string(),
        }
    }
}

/// A sealed segment plus the columns to materialize on `load()`.
#[derive(Debug, Clone)]
pub struct StreamSegment {
    segment: Arc<Segment>,
    columns: Vec<ColumnRequest>,
}

impl StreamSegment {
    /// Wrap `segment`, materializing `columns` on load.
    #[must_use]
    pub fn new(segment: Arc<Segment>, columns: Vec<ColumnRequest>) -> Self {
        Self { segment, columns }
    }

    /// The underlying segment.
    #[must_use]
    pub fn segment(&self) -> &Arc<Segment> {
        &self.segment
    }
}

impl VmSegment for StreamSegment {
    fn load(&self) -> Result<Arc<Batch>, VmError> {
        materialize(&self.segment, &self.columns).map(Arc::new)
    }
}

/// Build a [`Batch`] with exactly `columns` from `segment`.
pub fn materialize(segment: &Segment, columns: &[ColumnRequest]) -> Result<Batch, VmError> {
    let mut batch = Batch::new(segment.len());
    for req in columns {
        let values = column_values(segment, &req.name).ok_or_else(|| VmError::SegmentLoad {
            reason: format!(
                "unknown column `{}` (segment at offset {})",
                req.name,
                segment.file_off()
            ),
        })?;
        batch = batch.with_column(req.key.clone(), values);
    }
    Ok(batch)
}

/// One column of `segment` as `Value`s, or `None` if the name is unknown.
fn column_values(segment: &Segment, name: &str) -> Option<Vec<Value>> {
    let n = segment.len();
    let col = match name {
        "timestamp" => ints(segment.event_ts_ns()),
        "observed_ts" => ints(segment.observed_ts_ns()),
        "severity" => segment
            .severity()
            .iter()
            .map(|s| s.map_or(Value::Null, |s| Value::Int(i64::from(s as u8))))
            .collect(),
        "severity_text" => segment
            .severity()
            .iter()
            .map(|s| s.map_or(Value::Null, |s| Value::Str(s.as_str().into())))
            .collect(),
        "facility" => segment
            .facility()
            .iter()
            .map(|f| f.map_or(Value::Null, |f| Value::Str(f.as_str().into())))
            .collect(),
        "message" => (0..n)
            .map(|i| {
                segment
                    .message(i)
                    .map_or(Value::Null, |m| Value::Str(m.to_string().into()))
            })
            .collect(),
        "raw" => (0..n)
            .map(|i| {
                Value::Str(
                    String::from_utf8_lossy(segment.raw_line(i))
                        .into_owned()
                        .into(),
                )
            })
            .collect(),
        other => match segment.field(other)? {
            OwnedColumn::Dict { .. } | OwnedColumn::Str(_) => (0..n)
                .map(|i| {
                    segment
                        .field_str(other, i)
                        .map_or(Value::Null, |s| Value::Str(s.to_string().into()))
                })
                .collect(),
            OwnedColumn::Int(v) => ints(v),
            OwnedColumn::Float(v) => v
                .iter()
                .map(|x| x.map_or(Value::Null, Value::Float))
                .collect(),
            OwnedColumn::Bool(v) => v
                .iter()
                .map(|x| x.map_or(Value::Null, Value::Bool))
                .collect(),
        },
    };
    Some(col)
}

fn ints(v: &[Option<i64>]) -> Vec<Value> {
    v.iter()
        .map(|x| x.map_or(Value::Null, Value::Int))
        .collect()
}

/// Whether `name` is a column every segment can serve.
#[must_use]
pub fn is_predefined(name: &str) -> bool {
    PREDEFINED_COLUMNS.contains(&name)
}

/// Nanoseconds since the Unix epoch, saturating.
#[must_use]
pub fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_nanos()).ok())
        .unwrap_or(i64::MAX)
}

/// The live head of a file as a pull source: each `next_batch()` returns
/// the newly appended complete lines, materialized to `columns`. Polls the
/// file every `poll`; gives up (returns `None`) after `max_idle_polls`
/// consecutive polls without new data, or never if `None`.
#[derive(Debug)]
pub struct TailSource {
    file: LogFile,
    source: Source,
    parser: SyslogParser,
    columns: Vec<ColumnRequest>,
    poll: Duration,
    max_idle_polls: Option<u32>,
    /// Segments sealed on the last `next_batch`, for callers that want to
    /// keep them (e.g. push into a ring).
    last_sealed: Vec<Arc<Segment>>,
}

impl TailSource {
    /// Follow `file` from its current `head_off`.
    #[must_use]
    pub fn new(
        file: LogFile,
        source: Source,
        parser: SyslogParser,
        columns: Vec<ColumnRequest>,
        poll: Duration,
        max_idle_polls: Option<u32>,
    ) -> Self {
        Self {
            file,
            source,
            parser,
            columns,
            poll,
            max_idle_polls,
            last_sealed: Vec::new(),
        }
    }

    /// Segments sealed by the most recent `next_batch()`.
    pub fn take_sealed(&mut self) -> Vec<Arc<Segment>> {
        std::mem::take(&mut self.last_sealed)
    }

    /// The underlying file.
    #[must_use]
    pub const fn file(&self) -> &LogFile {
        &self.file
    }
}

impl VmSource for TailSource {
    fn next_batch(&mut self) -> Option<Batch> {
        let mut idle: u32 = 0;
        loop {
            match self.file.refresh() {
                Ok(Refresh::New(blocks)) => {
                    let observed = now_ns();
                    let mut sealed: Vec<Arc<Segment>> = Vec::new();
                    for b in &blocks {
                        for s in Segment::seal_block(b, &self.source, &self.parser, observed) {
                            sealed.push(Arc::new(s));
                        }
                    }
                    // One batch per call: concatenate the sealed segments.
                    let total: usize = sealed.iter().map(|s| s.len()).sum();
                    let mut merged = Batch::new(total);
                    for req in &self.columns {
                        let mut values: Vec<Value> = Vec::with_capacity(total);
                        for s in &sealed {
                            values.extend(column_values(s, &req.name)?);
                        }
                        merged = merged.with_column(req.key.clone(), values);
                    }
                    self.last_sealed = sealed;
                    return Some(merged);
                }
                Ok(Refresh::Truncated) => {
                    // Rotation: keep following the new file from its end.
                    idle = 0;
                }
                Ok(Refresh::NoNew) => {
                    if let Some(max) = self.max_idle_polls {
                        if idle >= max {
                            return None;
                        }
                    }
                    idle = idle.saturating_add(1);
                    std::thread::sleep(self.poll);
                }
                Err(_) => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::stream::{Block, SourceKind};
    use std::io::Write;

    fn seg(text: &str) -> Arc<Segment> {
        let b = Block {
            file_off: 0,
            bytes: Arc::from(text.as_bytes()),
        };
        let mut v = Segment::seal_block(
            &b,
            &Source::new(SourceKind::File, "t"),
            &SyslogParser::with_year(2026),
            7,
        );
        Arc::new(v.remove(0))
    }

    #[test]
    fn materializes_only_requested_columns() {
        let s = seg(
            "<131>Sep 10 08:00:01 web01 nginx[12]: boom\n<134>Sep 10 08:00:02 web01 sshd[3]: ok\n",
        );
        let cols = vec![ColumnRequest::bare("severity"), ColumnRequest::bare("tag")];
        let b = materialize(&s, &cols).unwrap();
        assert_eq!(b.num_rows, 2);
        assert_eq!(b.columns.len(), 2);
        assert_eq!(b.columns["severity"][0], Value::Int(17)); // ERROR
        assert_eq!(b.columns["severity"][1], Value::Int(9)); // INFO
        assert_eq!(b.columns["tag"][1], Value::Str("sshd".into()));
    }

    #[test]
    fn qualified_key_is_preserved() {
        let s = seg("<134>Sep 10 08:00:01 h a: x\n");
        let cols = vec![ColumnRequest {
            key: "log.facility".into(),
            name: "facility".into(),
        }];
        let b = materialize(&s, &cols).unwrap();
        assert_eq!(b.columns["log.facility"][0], Value::Str("local0".into()));
    }

    #[test]
    fn unknown_column_is_an_error_not_nulls() {
        let s = seg("<134>Sep 10 08:00:01 h a: x\n");
        let err = materialize(&s, &[ColumnRequest::bare("nope")]).unwrap_err();
        assert!(matches!(err, VmError::SegmentLoad { .. }));
    }

    #[test]
    fn clf_where_status_and_method_runs_through_batch_vm() {
        use crate::codegen::batch::compile;
        use crate::parser::column::parse as parse_select;
        use crate::storage::stream::ClfParser;
        use crate::vm::batch::run_parallel;

        let text = "1.1.1.1 - - [10/Sep/2026:08:00:00 +0000] \"POST /submit HTTP/1.1\" 500 12 \"-\" \"-\"\n\
1.1.1.1 - - [10/Sep/2026:08:00:01 +0000] \"GET /health HTTP/1.1\" 200 3 \"-\" \"-\"\n\
1.1.1.1 - - [10/Sep/2026:08:00:02 +0000] \"POST /submit HTTP/1.1\" 200 5 \"-\" \"-\"\n";
        let b = Block {
            file_off: 0,
            bytes: Arc::from(text.as_bytes()),
        };
        let mut segs = Segment::seal_block(
            &b,
            &Source::new(SourceKind::File, "access"),
            &ClfParser::new(),
            0,
        );
        let seg = StreamSegment::new(
            Arc::new(segs.remove(0)),
            vec![ColumnRequest::bare("status"), ColumnRequest::bare("method")],
        );

        let select =
            parse_select("SELECT status, method FROM log WHERE status >= 500 AND method = 'POST'")
                .unwrap();
        let program = compile(&select).unwrap();
        let opcodes: Vec<_> = program.opcodes().cloned().collect();

        let rows = run_parallel(&[seg], &opcodes).unwrap();
        assert_eq!(
            rows.len(),
            1,
            "only the first line matches status>=500 AND method='POST'"
        );
        assert_eq!(rows[0][0], Value::Int(500));
        assert_eq!(rows[0][1], Value::Str("POST".into()));
    }

    #[test]
    fn tail_source_yields_appends_then_stops() {
        let mut p = std::env::temp_dir();
        p.push(format!("db-core-tail-{}.log", std::process::id()));
        std::fs::write(&p, b"<134>Sep 10 08:00:01 h a: first\n").unwrap();
        let file = LogFile::open(&p).unwrap();
        let mut src = TailSource::new(
            file,
            Source::new(SourceKind::File, "t"),
            SyslogParser::with_year(2026),
            vec![ColumnRequest::bare("message")],
            Duration::from_millis(5),
            Some(3),
        );
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"<134>Sep 10 08:00:02 h a: second\n<131>Sep 10 08:00:03 h a: third\n")
            .unwrap();
        let b = src.next_batch().expect("new lines");
        assert_eq!(b.num_rows, 2);
        assert_eq!(b.columns["message"][1], Value::Str("third".into()));
        assert_eq!(src.take_sealed().len(), 1);
        assert!(src.next_batch().is_none(), "gives up after idle polls");
        std::fs::remove_file(&p).ok();
    }
}
