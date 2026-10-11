use super::*;
use crate::storage::parquet::ParquetFile;
use std::sync::atomic::{AtomicU64, Ordering};

struct TempTable(PathBuf);

impl TempTable {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("db-core-table-{}-{n}", std::process::id()));
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
    format!(r#"{{"i":{i},"level":"info","msg":"record {i}"}}"#)
}

fn append_lines(w: &mut TableWriter, range: std::ops::Range<usize>) -> Vec<String> {
    range
        .map(|i| {
            let l = line(i);
            w.append(l.as_bytes(), Ack::Durable).unwrap();
            l
        })
        .collect()
}

/// Every record the table holds: `raw` from each manifest file, then from
/// the sealed pages and the head.
fn all_rows(w: &TableWriter) -> Vec<String> {
    let mut rows = Vec::new();
    for file in &w.manifest().files {
        let bytes = fs::read(w.dir().join(&file.path)).unwrap();
        let parquet = ParquetFile::open(&bytes).unwrap();
        let raw = parquet.metadata().schema[1..]
            .iter()
            .position(|e| e.name == "raw")
            .unwrap();
        for rg in parquet.row_groups() {
            rows.extend(
                rg.read_string_column(raw)
                    .unwrap()
                    .into_iter()
                    .map(Option::unwrap),
            );
        }
    }
    for page in w.sealed_pages().iter().chain(w.head_page().iter()) {
        for i in 0..page.segment.len() {
            rows.push(String::from_utf8(page.segment.raw_line(i).to_vec()).unwrap());
        }
    }
    rows
}

/// Only listed files in data/, and nothing in tmp/.
fn assert_clean(w: &TableWriter) {
    let tmp_files = fs::read_dir(w.dir().join(TMP_DIR)).map_or(0, Iterator::count);
    assert_eq!(tmp_files, 0, "files left in tmp/");
    let mut on_disk: Vec<String> = fs::read_dir(w.dir().join(DATA_DIR))
        .map(|d| {
            d.map(|e| format!("{DATA_DIR}/{}", e.unwrap().file_name().to_string_lossy()))
                .collect()
        })
        .unwrap_or_default();
    on_disk.sort();
    let mut listed: Vec<String> = w.manifest().files.iter().map(|f| f.path.clone()).collect();
    listed.sort();
    assert_eq!(on_disk, listed);
}

#[test]
fn append_seal_flush_and_read_back() {
    let t = TempTable::new();
    let mut w = open(&t);
    let lines = append_lines(&mut w, 0..10);
    assert_eq!(w.head_page().unwrap().segment.len(), 10);
    let entry = w.flush_all().unwrap().unwrap();
    assert_eq!((entry.lsn_lo, entry.lsn_hi, entry.rows), (1, 10, 10));
    assert_eq!(entry.path, format!("data/{:020}-{:020}.parquet", 1, 10));
    assert!(entry.observed_ts.is_some());
    assert_eq!(w.manifest().flushed_lsn, 10);
    assert_eq!(w.watermarks().unwrap().flushed, 10);
    assert!(w.sealed_pages().is_empty());
    assert!(w.head_page().is_none());
    assert_eq!(all_rows(&w), lines);
    assert!(w.flush().unwrap().is_none(), "nothing sealed");
    assert_clean(&w);
}

#[test]
fn unflushed_records_come_back_as_pages_on_reopen() {
    let t = TempTable::new();
    let mut w = open(&t);
    let mut lines = append_lines(&mut w, 0..5);
    w.flush_all().unwrap();
    lines.extend(append_lines(&mut w, 5..12));
    w.seal_head().unwrap();
    lines.extend(append_lines(&mut w, 12..15));
    let observed_before = w.head_page().unwrap().segment.observed_ts_ns().to_vec();
    w.close().unwrap();

    let w = open(&t);
    assert_eq!(w.manifest().flushed_lsn, 5);
    assert_eq!(all_rows(&w), lines);
    // Replayed records keep their arrival time.
    let replayed = w.head_page().unwrap();
    let tail = &replayed.segment.observed_ts_ns()[replayed.segment.len() - 3..];
    assert_eq!(tail, &observed_before[..]);
}

#[test]
fn successive_flushes_keep_the_manifest_contiguous() {
    let t = TempTable::new();
    let mut w = open(&t);
    let mut lines = Vec::new();
    for batch in 0..4 {
        lines.extend(append_lines(&mut w, batch * 7..batch * 7 + 7));
        w.flush_all().unwrap();
    }
    let m = w.manifest().clone();
    assert_eq!(m.files.len(), 4);
    m.check().unwrap();
    assert_eq!(m.flushed_lsn, 28);
    assert_eq!(all_rows(&w), lines);
    drop(w);
    let w = open(&t);
    assert_eq!(w.manifest(), &m);
    assert_eq!(all_rows(&w), lines);
    // The WAL keeps nothing that is flushed.
    let wal_files = fs::read_dir(t.0.join("wal")).unwrap().count();
    assert!(wal_files <= 1, "{wal_files} WAL files left");
}

#[test]
fn a_crash_at_every_flush_step_recovers_every_record_once() {
    for step in [1u8, 2, 3, 5] {
        let t = TempTable::new();
        let mut w = open(&t);
        let mut lines = append_lines(&mut w, 0..6);
        w.flush_all().unwrap();
        lines.extend(append_lines(&mut w, 6..20));
        w.seal_head().unwrap();
        w.crash_after = Some(step);
        let err = w.flush().unwrap_err();
        assert!(
            err.to_string()
                .contains(&format!("after flush step {step}")),
            "{err}"
        );
        drop(w); // the crash: nothing after the failed step runs

        let mut w = open(&t);
        assert_eq!(all_rows(&w), lines, "after a crash at step {step}");
        assert_clean(&w);
        let committed = step >= 3;
        assert_eq!(
            w.manifest().flushed_lsn,
            if committed { 20 } else { 6 },
            "step {step}"
        );
        // And the table carries on.
        lines.extend(append_lines(&mut w, 20..25));
        w.flush_all().unwrap();
        assert_eq!(all_rows(&w), lines, "step {step}");
        w.manifest().check().unwrap();
    }
}

#[test]
fn flushed_ack_returns_after_the_commit() {
    let t = TempTable::new();
    let mut w = open(&t);
    w.append(line(0).as_bytes(), Ack::Memory).unwrap();
    let lsn = w.append(line(1).as_bytes(), Ack::Flushed).unwrap();
    assert_eq!(lsn, 2);
    assert_eq!(w.manifest().flushed_lsn, 2);
}

#[test]
fn a_full_head_seals_itself() {
    let t = TempTable::new();
    let mut w = open(&t);
    let max = crate::storage::stream::segment::SEGMENT_MAX_ROWS;
    for i in 0..max + 3 {
        w.append(line(i).as_bytes(), Ack::Memory).unwrap();
    }
    assert_eq!(w.sealed_pages().len(), 1);
    assert_eq!(w.sealed_pages()[0].segment.len(), max);
    assert_eq!(w.head_page().unwrap().segment.len(), 3);
}

#[test]
fn invalid_records_and_formats_are_refused() {
    let t = TempTable::new();
    let mut w = open(&t);
    for bad in [&b""[..], b"two\nlines"] {
        assert!(matches!(
            w.append(bad, Ack::Durable),
            Err(IngestError::InvalidRecord(_))
        ));
    }
    drop(w);
    let options = TableOptions {
        format: Format::Logfmt,
        ..TableOptions::default()
    };
    let err = TableWriter::open(&t.0, options).unwrap_err();
    assert_eq!(err.to_string(), "the table's format is json, not logfmt");
}

#[test]
fn a_second_writer_is_locked_out() {
    let t = TempTable::new();
    let _w = open(&t);
    let err = TableWriter::open(&t.0, TableOptions::default()).unwrap_err();
    assert!(matches!(err, IngestError::Wal(WalError::Locked)), "{err}");
}

#[test]
fn a_wal_payload_that_is_not_a_record_is_corruption() {
    let t = TempTable::new();
    drop(open(&t)); // creates the manifest
    let (wal, _) = Wal::open(&t.0, 0, WalOptions::default()).unwrap();
    wal.append(b"short", Ack::Durable).unwrap();
    wal.close().unwrap();
    let err = TableWriter::open(&t.0, TableOptions::default()).unwrap_err();
    assert_eq!(err.to_string(), "table: WAL record 1 is not a record");
}

#[test]
fn error_messages() {
    assert_eq!(
        IngestError::from(io::Error::other("disk")).to_string(),
        "table: disk"
    );
    assert_eq!(
        IngestError::from(WalError::Closed).to_string(),
        "wal: closed"
    );
    assert_eq!(
        IngestError::from(ManifestError::Malformed("x".into())).to_string(),
        "manifest: malformed: x"
    );
    assert_eq!(
        IngestError::from(WriteError::TooLarge("x".into())).to_string(),
        "too large for parquet: x"
    );
    assert_eq!(IngestError::Corrupt("why".into()).to_string(), "table: why");
}
