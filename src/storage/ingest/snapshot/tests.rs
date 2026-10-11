use super::*;
use crate::storage::ingest::page::encode_record;
use crate::storage::ingest::{Ack, TableOptions, TableWriter};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

struct TempTable(PathBuf);

impl TempTable {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("db-core-snapshot-{}-{n}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        TempTable(dir)
    }
}

impl Drop for TempTable {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn open(t: &TempTable) -> TableWriter {
    TableWriter::open(&t.0, TableOptions::default()).unwrap()
}

fn line(i: usize) -> String {
    format!(r#"{{"i":{i},"msg":"record {i}"}}"#)
}

fn append(w: &mut TableWriter, range: std::ops::Range<usize>) -> Vec<String> {
    range
        .map(|i| {
            let l = line(i);
            w.append(l.as_bytes(), Ack::Durable).unwrap();
            l
        })
        .collect()
}

/// Every record's `raw`, files then pages, and the LSNs they cover.
fn raws(s: &Snapshot) -> (Vec<String>, Vec<(u64, u64)>) {
    let mut rows = Vec::new();
    let mut ranges = Vec::new();
    for (entry, bytes) in s.files() {
        let file = ParquetFile::open(bytes).unwrap();
        let raw = file.metadata().schema[1..]
            .iter()
            .position(|e| e.name == "raw")
            .unwrap();
        for rg in file.row_groups() {
            rows.extend(rg.read_string_column(raw).unwrap().into_iter().flatten());
        }
        ranges.push((entry.lsn_lo, entry.lsn_hi));
    }
    for page in s.pages() {
        for i in 0..page.segment.len() {
            rows.push(String::from_utf8(page.segment.raw_line(i).to_vec()).unwrap());
        }
        ranges.push((page.lsn_lo, page.lsn_hi));
    }
    (rows, ranges)
}

/// The ranges are contiguous from LSN 1 to `last`.
fn assert_contiguous(ranges: &[(u64, u64)], last: u64) {
    let mut next = 1;
    for &(lo, hi) in ranges {
        assert_eq!(lo, next, "{ranges:?}");
        next = hi + 1;
    }
    assert_eq!(next - 1, last, "{ranges:?}");
}

#[test]
fn in_process_and_out_of_process_snapshots_hold_every_record_once() {
    let t = TempTable::new();
    let mut w = open(&t);
    let mut lines = append(&mut w, 0..5);
    w.flush_all().unwrap();
    lines.extend(append(&mut w, 5..9));
    w.seal_head().unwrap();
    lines.extend(append(&mut w, 9..12));

    for s in [w.snapshot().unwrap(), Snapshot::open(&t.0).unwrap()] {
        let (rows, ranges) = raws(&s);
        assert_eq!(rows, lines, "{s:?}");
        assert_contiguous(&ranges, 12);
        assert_eq!(s.last_lsn(), 12);
        assert_eq!(s.rows(), 12);
        assert_eq!(s.manifest().flushed_lsn, 5);
    }
    // A snapshot is fixed: later appends are not in it.
    let s = Snapshot::open(&t.0).unwrap();
    append(&mut w, 12..13);
    assert_eq!(s.last_lsn(), 12);
    assert_eq!(Snapshot::open(&t.0).unwrap().last_lsn(), 13);
}

#[test]
fn a_flush_interrupted_at_any_step_is_read_once() {
    for step in [1u8, 2, 3, 5] {
        let t = TempTable::new();
        let mut w = open(&t);
        let mut lines = append(&mut w, 0..4);
        w.flush_all().unwrap();
        lines.extend(append(&mut w, 4..10));
        w.seal_head().unwrap();
        w.crash_after = Some(step);
        w.flush().unwrap_err();
        // A reader while the crashed writer still holds the table: the
        // stray file in tmp/ or data/ is not listed, and the WAL still
        // holds what the manifest does not.
        let s = Snapshot::open(&t.0).unwrap();
        let (rows, ranges) = raws(&s);
        assert_eq!(rows, lines, "step {step}");
        assert_contiguous(&ranges, 10);
        assert_eq!(s.manifest().flushed_lsn, if step >= 3 { 10 } else { 4 });
    }
}

#[test]
fn a_wal_that_never_reaches_the_manifest_is_an_error() {
    let t = TempTable::new();
    let mut w = open(&t);
    append(&mut w, 0..2);
    w.seal_head().unwrap();
    append(&mut w, 2..4);
    w.close().unwrap();
    // LSNs 1-2 are gone, but the manifest says nothing is flushed.
    let first =
        t.0.join(WAL_DIR)
            .join(super::super::recover::segment_name(1));
    fs::remove_file(first).unwrap();
    let err = Snapshot::open(&t.0).unwrap_err();
    assert!(err.to_string().contains("does not continue"), "{err}");
}

#[test]
fn a_directory_without_a_manifest_is_not_a_table() {
    let t = TempTable::new();
    fs::create_dir_all(&t.0).unwrap();
    let err = Snapshot::open(&t.0).unwrap_err();
    assert!(err.to_string().contains("not a table"), "{err}");
}

#[test]
fn a_missing_or_unreadable_file_is_an_error() {
    let t = TempTable::new();
    let mut w = open(&t);
    append(&mut w, 0..3);
    let entry = w.flush_all().unwrap().unwrap();
    let path = t.0.join(&entry.path);
    fs::write(&path, b"not parquet").unwrap();
    let s = w.snapshot().unwrap();
    assert!(s.schema().unwrap_err().to_string().contains("data/"));
    fs::remove_file(&path).unwrap();
    assert!(Snapshot::open(&t.0).is_err());
}

#[test]
fn the_schema_is_the_union_widened() {
    let t = TempTable::new();
    let mut w = open(&t);
    for l in [r#"{"a":1,"b":"x"}"#, r#"{"a":2,"c":true}"#] {
        w.append(l.as_bytes(), Ack::Memory).unwrap();
    }
    w.flush_all().unwrap();
    for l in [r#"{"a":2.5,"d":7}"#, r#"{"c":"yes","raw":"shadow"}"#] {
        w.append(l.as_bytes(), Ack::Memory).unwrap();
    }
    let schema = w.snapshot().unwrap().schema().unwrap();
    let names: Vec<&str> = schema.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        &names[7..],
        &["a", "b", "c", "d"],
        "fields after the predefined seven, raw not repeated"
    );
    let type_of = |n: &str| schema.iter().find(|s| s.name == n).unwrap().column_type;
    assert_eq!(type_of("a"), ColumnType::Double);
    assert_eq!(type_of("b"), ColumnType::Utf8);
    assert_eq!(type_of("c"), ColumnType::Utf8);
    assert_eq!(type_of("d"), ColumnType::Int64);
    assert_eq!(type_of("timestamp"), ColumnType::TimestampNanos);
}

#[test]
fn pages_from_records_seal_like_the_writer_and_refuse_garbage() {
    let max = crate::storage::stream::segment::SEGMENT_MAX_ROWS;
    let records: Vec<Record> = (1..=max as u64 + 2)
        .map(|lsn| {
            (
                lsn,
                encode_record(lsn as i64, line(lsn as usize).as_bytes()),
            )
        })
        .collect();
    let pages = pages_from_records(Path::new("t"), Format::Jsonl, records).unwrap();
    assert_eq!(pages.len(), 2);
    assert_eq!((pages[0].lsn_lo, pages[0].lsn_hi), (1, max as u64));
    assert_eq!(pages[1].segment.len(), 2);
    assert!(pages_from_records(Path::new("t"), Format::Jsonl, vec![])
        .unwrap()
        .is_empty());
    let err = pages_from_records(Path::new("t"), Format::Jsonl, vec![(1, b"x".to_vec())]);
    assert!(err.unwrap_err().to_string().contains("WAL record 1"));
}

#[test]
fn cast_follows_the_flush_rules() {
    let cases = [
        (Value::Null, ColumnType::Utf8, Value::Null),
        (Value::Int(3), ColumnType::Utf8, Value::Str("3".into())),
        (
            Value::Str("s".into()),
            ColumnType::Utf8,
            Value::Str("s".into()),
        ),
        (Value::Int(3), ColumnType::Double, Value::Float(3.0)),
        (Value::Float(1.5), ColumnType::Double, Value::Float(1.5)),
        (Value::Int(3), ColumnType::TimestampNanos, Value::Int(3)),
        (Value::Int(3), ColumnType::Int32, Value::Int(3)),
        (Value::Int(i64::MAX), ColumnType::Int32, Value::Null),
        (Value::Bool(true), ColumnType::Boolean, Value::Bool(true)),
        (Value::Bool(true), ColumnType::Int64, Value::Null),
        (Value::Float(1.0), ColumnType::Float, Value::Null),
    ];
    for (v, t, expected) in cases {
        assert_eq!(cast(v.clone(), t), expected, "{v:?} as {t:?}");
    }
}

#[test]
fn physical_and_column_types_round_trip() {
    for t in [
        ColumnType::Boolean,
        ColumnType::Int32,
        ColumnType::Int64,
        ColumnType::Float,
        ColumnType::Double,
        ColumnType::Utf8,
    ] {
        assert_eq!(column_type(physical_type(t)), Some(t));
    }
    assert_eq!(
        physical_type(ColumnType::TimestampMicros),
        PhysicalType::Int64
    );
    assert_eq!(
        physical_type(ColumnType::TimestampNanos),
        PhysicalType::Int64
    );
    for pt in [
        PhysicalType::Int96,
        PhysicalType::FixedLenByteArray,
        PhysicalType::Unknown(9),
    ] {
        assert_eq!(column_type(pt), None);
    }
}
