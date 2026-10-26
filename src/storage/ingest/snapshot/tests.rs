use super::*;
use crate::storage::ingest::page::{encode_record, PageBody};
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
        for i in 0..page.lines().unwrap().len() {
            rows.push(String::from_utf8(page.lines().unwrap().raw_line(i).to_vec()).unwrap());
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
    assert!(
        err.to_string().contains("does not match the manifest"),
        "{err}"
    );
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
    let entry = w.flush_all().unwrap().remove(0);
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
    let pages = pages_from_records(Path::new("t"), Format::Jsonl, None, records).unwrap();
    assert_eq!(pages.len(), 2);
    assert_eq!((pages[0].lsn_lo, pages[0].lsn_hi), (1, max as u64));
    assert_eq!(pages[1].lines().unwrap().len(), 2);
    assert!(
        pages_from_records(Path::new("t"), Format::Jsonl, None, vec![])
            .unwrap()
            .is_empty()
    );
    let err = pages_from_records(
        Path::new("t"),
        Format::Jsonl,
        None,
        vec![(1, b"x".to_vec())],
    );
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

/// A reader's snapshot holds what a fresh [`Snapshot::open`] does.
fn assert_same_as_fresh(reader: &TableReader, table: &Path) {
    let fresh = Snapshot::open(table).unwrap();
    let held = reader.snapshot();
    assert_eq!(raws(held), raws(&fresh), "{held:?} vs {fresh:?}");
    assert_eq!(held.last_lsn(), fresh.last_lsn());
    assert_eq!(held.rows(), fresh.rows());
    assert_eq!(held.manifest(), fresh.manifest());
}

#[test]
#[allow(non_snake_case)]
fn mcdc__storage_ingest_snapshot_refresh_1d471319__v3_a_refresh_parses_only_the_records_added_since_the_last(
) {
    let t = TempTable::new();
    let mut w = open(&t);
    append(&mut w, 0..100);
    let mut reader = TableReader::open(&t.0).unwrap();
    assert_eq!(
        reader.stats(),
        ReaderStats {
            refreshes: 1,
            rebuilds: 1,
            records_parsed: 100
        }
    );
    append(&mut w, 100..110);
    reader.refresh().unwrap();
    assert_eq!(reader.stats().records_parsed, 110, "10 new, 100 kept");
    assert_eq!(reader.stats().rebuilds, 1);
    assert_same_as_fresh(&reader, &t.0);
    // Nothing new: nothing parsed.
    reader.refresh().unwrap();
    assert_eq!(reader.stats().records_parsed, 110);
    assert_eq!(reader.stats().refreshes, 3);
    assert_same_as_fresh(&reader, &t.0);
}

#[test]
#[allow(non_snake_case)]
fn mcdc__storage_ingest_snapshot_refresh_1d471319__v1_a_flush_makes_the_reader_read_the_tail_again()
{
    let t = TempTable::new();
    let mut w = open(&t);
    append(&mut w, 0..5);
    let mut reader = TableReader::open(&t.0).unwrap();
    w.flush_all().unwrap();
    append(&mut w, 5..8);
    reader.refresh().unwrap();
    assert_eq!(reader.stats().rebuilds, 2);
    assert_eq!(reader.snapshot().manifest().flushed_lsn, 5);
    assert_same_as_fresh(&reader, &t.0);
    // The three records above the flush were parsed again, then kept.
    append(&mut w, 8..10);
    reader.refresh().unwrap();
    assert_eq!(reader.stats().rebuilds, 2);
    assert_eq!(reader.stats().records_parsed, 5 + 3 + 2);
    assert_same_as_fresh(&reader, &t.0);
}

#[test]
#[allow(non_snake_case)]
fn mcdc__storage_ingest_snapshot_refresh_1d471319__v2_a_wal_segment_that_shrinks_is_read_again() {
    let t = TempTable::new();
    let mut w = open(&t);
    append(&mut w, 0..10);
    let mut reader = TableReader::open(&t.0).unwrap();
    // As after the writer truncated a torn tail: the file is shorter than
    // the position the reader saved.
    let segment =
        t.0.join(WAL_DIR)
            .join(super::super::recover::segment_name(1));
    let len = fs::metadata(&segment).unwrap().len();
    fs::OpenOptions::new()
        .write(true)
        .open(&segment)
        .unwrap()
        .set_len(len - 5)
        .unwrap();
    reader.refresh().unwrap();
    assert_eq!(reader.stats().rebuilds, 2);
    assert_eq!(reader.snapshot().last_lsn(), 9, "the cut frame is not read");
    assert_same_as_fresh(&reader, &t.0);
}

#[test]
fn a_reader_follows_the_wal_across_segments() {
    let t = TempTable::new();
    let mut w = TableWriter::open(
        &t.0,
        TableOptions {
            wal: crate::storage::ingest::WalOptions { segment_bytes: 200 },
            ..TableOptions::default()
        },
    )
    .unwrap();
    append(&mut w, 0..3);
    let mut reader = TableReader::open(&t.0).unwrap();
    for from in (3..60).step_by(7) {
        append(&mut w, from..from + 7);
        reader.refresh().unwrap();
        assert_same_as_fresh(&reader, &t.0);
    }
    assert!(fs::read_dir(t.0.join(WAL_DIR)).unwrap().count() > 3);
    assert_eq!(reader.stats().rebuilds, 1, "never a flush, never a rebuild");
    assert_eq!(reader.stats().records_parsed, 3 + 7 * 9);
}

#[test]
fn a_record_table_is_read_incrementally() {
    use crate::storage::ingest::RecordValue;
    let t = TempTable::new();
    let mut w = TableWriter::open(
        &t.0,
        TableOptions {
            schema: Some(vec![
                ColumnSpec::new("n", ColumnType::Int64, false),
                ColumnSpec::new("s", ColumnType::Utf8, true),
            ]),
            ..TableOptions::default()
        },
    )
    .unwrap();
    let row = |n: i64| vec![RecordValue::Int(n), RecordValue::Str(format!("r{n}"))];
    for n in 0..5 {
        w.append_row(row(n), Ack::Durable).unwrap();
    }
    let mut reader = TableReader::open(&t.0).unwrap();
    for n in 5..9 {
        w.append_row(row(n), Ack::Durable).unwrap();
    }
    reader.refresh().unwrap();
    assert_eq!(reader.stats().records_parsed, 9);
    assert_eq!(reader.stats().rebuilds, 1);
    let held = reader.snapshot();
    let fresh = Snapshot::open(&t.0).unwrap();
    assert_eq!(held.rows(), 9);
    assert_eq!(held.pages().len(), fresh.pages().len());
    assert_eq!(held.last_lsn(), fresh.last_lsn());
    for (a, b) in held.pages().iter().zip(fresh.pages()) {
        let (PageBody::Rows(a), PageBody::Rows(b)) = (&a.body, &b.body) else {
            panic!("a record table has row pages");
        };
        assert_eq!(a.columns(), b.columns());
    }
}

/// Property: a reader refreshed at random points among a writer's appends,
/// seals, flushes and compactions always holds what a fresh open does.
#[test]
fn a_reader_refreshed_at_random_points_matches_a_fresh_open() {
    for seed in 1..=3u64 {
        let t = TempTable::new();
        let mut rng = crate::test_rng::Rng::new(seed);
        let mut w = TableWriter::open(
            &t.0,
            TableOptions {
                wal: crate::storage::ingest::WalOptions { segment_bytes: 300 },
                ..TableOptions::default()
            },
        )
        .unwrap();
        let mut reader = TableReader::open(&t.0).unwrap();
        let mut next = 0usize;
        for step in 0..120 {
            match rng.below(10) {
                0..=5 => {
                    let n = 1 + usize::try_from(rng.below(6)).unwrap();
                    append(&mut w, next..next + n);
                    next += n;
                }
                6 => w.seal_head().unwrap(),
                7 => {
                    w.flush().unwrap();
                }
                8 => {
                    w.flush_all().unwrap();
                }
                _ => {}
            }
            if rng.below(3) == 0 {
                reader.refresh().unwrap();
                assert_same_as_fresh(&reader, &t.0);
            }
            assert!(
                reader.stats().rebuilds <= 1 + u64::try_from(step).unwrap(),
                "seed {seed}"
            );
        }
        reader.refresh().unwrap();
        assert_same_as_fresh(&reader, &t.0);
    }
}
