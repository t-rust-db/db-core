//! The table handle and its line sources.

use super::*;
use crate::storage::ingest::layout::{Layout, TierOptions};
use crate::storage::parquet::ParquetFile;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("db-core-handle-{}-{n}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn table(&self) -> PathBuf {
        self.0.join("events")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

/// Every record's `raw`, files then pages, in that order.
fn raws(table: &Table) -> Vec<String> {
    let s = table.snapshot().unwrap();
    let mut rows = Vec::new();
    for (_, bytes) in s.files() {
        let file = ParquetFile::open(bytes).unwrap();
        let raw = file.metadata().schema[1..]
            .iter()
            .position(|e| e.name == "raw")
            .unwrap();
        for rg in file.row_groups() {
            rows.extend(rg.read_string_column(raw).unwrap().into_iter().flatten());
        }
    }
    for page in s.pages() {
        let lines = page.lines().unwrap();
        for i in 0..lines.len() {
            rows.push(String::from_utf8(lines.raw_line(i).to_vec()).unwrap());
        }
    }
    rows
}

/// Waits up to 10 s for `table` to hold `n` records.
fn wait_for(table: &Table, n: usize) {
    let start = Instant::now();
    while raws(table).len() < n {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out at {n}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Waits up to 10 s for the one resume file under `table` to say `offset`.
fn wait_for_offset(table: &Path, offset: u64) {
    let start = Instant::now();
    loop {
        let saved = fs::read_dir(table.join(FOLLOW_DIR))
            .ok()
            .and_then(|mut d| {
                d.find(|e| {
                    e.as_ref()
                        .is_ok_and(|e| e.path().extension().is_some_and(|x| x == "offset"))
                })
            })
            .and_then(|e| fs::read_to_string(e.ok()?.path()).ok())
            .and_then(|t| t.split_whitespace().nth(2)?.parse::<u64>().ok());
        if saved == Some(offset) {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "offset {saved:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn append_to(path: &Path, text: &str) {
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    f.write_all(text.as_bytes()).unwrap();
}

#[test]
fn appends_flushes_and_snapshots_through_the_handle() {
    let t = TempDir::new();
    let table = Table::open(t.table(), TableOptions::default()).unwrap();
    assert_eq!(table.dir(), t.table());
    assert_eq!(table.append(br#"{"a":1}"#, Ack::Durable).unwrap(), 1);
    let last = table
        .append_batch(&[br#"{"a":2}"#.as_slice(), br#"{"a":3}"#], Ack::Durable)
        .unwrap();
    assert_eq!(last, Some(3));
    assert_eq!(
        table.append_batch::<&[u8]>(&[], Ack::Durable).unwrap(),
        None
    );
    assert_eq!(table.sync().unwrap(), 3);
    let flushed = table.flush().unwrap();
    assert_eq!(flushed.len(), 1);
    assert_eq!(table.snapshot().unwrap().manifest().flushed_lsn, 3);
    assert_eq!(
        table.compact().unwrap(),
        Vec::new(),
        "flat: nothing to compact"
    );
    assert_eq!(raws(&table), [r#"{"a":1}"#, r#"{"a":2}"#, r#"{"a":3}"#]);
}

#[test]
fn a_batch_with_an_invalid_line_appends_none() {
    let t = TempDir::new();
    let table = Table::open(t.table(), TableOptions::default()).unwrap();
    let err = table
        .append_batch(&[b"ok".as_slice(), b""], Ack::Memory)
        .unwrap_err();
    assert!(matches!(err, IngestError::InvalidRecord(_)), "{err}");
    assert_eq!(table.snapshot().unwrap().rows(), 0);
}

#[test]
fn record_rows_go_through_the_handle() {
    use crate::storage::parquet::schema::{ColumnSpec, ColumnType};
    let t = TempDir::new();
    let options = TableOptions {
        schema: Some(vec![ColumnSpec::new("n", ColumnType::Int64, false)]),
        ..TableOptions::default()
    };
    let table = Table::open(t.table(), options).unwrap();
    assert_eq!(
        table
            .append_row(vec![RecordValue::Int(7)], Ack::Durable)
            .unwrap(),
        1
    );
    assert!(
        table.append(b"x", Ack::Memory).is_err(),
        "a record table takes rows"
    );
    assert_eq!(table.snapshot().unwrap().rows(), 1);
}

#[test]
fn a_clone_shares_the_table_and_close_closes_every_clone() {
    let t = TempDir::new();
    let table = Table::open(t.table(), TableOptions::default()).unwrap();
    let other = table.clone();
    other.append(b"one", Ack::Durable).unwrap();
    assert_eq!(table.snapshot().unwrap().rows(), 1);
    assert!(!table.is_closed());
    other.close().unwrap();
    assert!(table.is_closed());
    for err in [
        table.append(b"two", Ack::Memory).unwrap_err(),
        table.snapshot().unwrap_err(),
        table.flush().map(|_| ()).unwrap_err(),
    ] {
        assert!(matches!(err, IngestError::Wal(WalError::Closed)), "{err}");
    }
    table.close().unwrap();
    // The lock is released: the table opens again with its record.
    let again = Table::open(t.table(), TableOptions::default()).unwrap();
    assert_eq!(raws(&again), ["one"]);
}

#[test]
fn ingest_lines_reads_to_the_end_and_skips_blank_lines() {
    let t = TempDir::new();
    let table = Table::open(t.table(), TableOptions::default()).unwrap();
    let input = "{\"a\":1}\r\n\n{\"a\":2}\n   \nlast without newline";
    let n = table.ingest_lines(io::Cursor::new(input)).unwrap();
    assert_eq!(n, 4);
    assert_eq!(
        raws(&table),
        [r#"{"a":1}"#, r#"{"a":2}"#, "   ", "last without newline"]
    );
    let marks = table.with(|w| w.watermarks()).unwrap();
    assert_eq!(marks.durable, 4, "durable at the end of input");
    assert_eq!(table.ingest_lines(io::empty()).unwrap(), 0);
}

#[test]
fn ingest_lines_into_a_tiered_table() {
    let t = TempDir::new();
    let options = TableOptions {
        layout: Layout::Tiered(TierOptions::default()),
        ..TableOptions::default()
    };
    let table = Table::open(t.table(), options).unwrap();
    let input: String = (0..100).map(|i| format!("{{\"i\":{i}}}\n")).collect();
    assert_eq!(table.ingest_lines(input.as_bytes()).unwrap(), 100);
    assert_eq!(raws(&table).len(), 100);
}

#[test]
fn follow_reads_new_lines_until_closed_and_resumes_where_it_stopped() {
    let t = TempDir::new();
    let log = t.0.join("app.log");
    append_to(&log, "one\ntwo\n");
    let table = Table::open(t.table(), TableOptions::default()).unwrap();
    let follower = table.clone();
    let path = log.clone();
    let thread = std::thread::spawn(move || follower.follow(&path));
    wait_for(&table, 2);
    // A partial line waits for its newline.
    append_to(&log, "thr");
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(raws(&table).len(), 2);
    append_to(&log, "ee\n");
    wait_for(&table, 3);
    table.close().unwrap();
    assert_eq!(thread.join().unwrap().unwrap(), 3);

    // Lines written while nobody follows are read on the next follow, and
    // nothing already appended is appended again.
    append_to(&log, "four\n");
    let table = Table::open(t.table(), TableOptions::default()).unwrap();
    let follower = table.clone();
    let path = log.clone();
    let thread = std::thread::spawn(move || follower.follow(&path));
    wait_for(&table, 4);
    table.close().unwrap();
    assert_eq!(thread.join().unwrap().unwrap(), 1);
    let table = Table::open(t.table(), TableOptions::default()).unwrap();
    assert_eq!(raws(&table), ["one", "two", "three", "four"]);
    let state = fs::read_dir(t.table().join(FOLLOW_DIR)).unwrap().count();
    assert_eq!(state, 1, "one resume file per followed path");
}

#[test]
fn follow_reads_a_rotated_file_and_a_truncated_one_from_their_start() {
    let t = TempDir::new();
    let log = t.0.join("app.log");
    append_to(&log, "a\n");
    let table = Table::open(t.table(), TableOptions::default()).unwrap();
    let follower = table.clone();
    let path = log.clone();
    let thread = std::thread::spawn(move || follower.follow(&path));
    wait_for(&table, 1);

    // Rename-and-recreate: the old file's last line, then the new file.
    append_to(&log, "b\n");
    fs::rename(&log, t.0.join("app.log.1")).unwrap();
    append_to(&log, "c\n");
    wait_for(&table, 3);
    wait_for_offset(&t.table(), 2);

    // Truncate in place (copytruncate): read again from the start. Only a
    // shrink can be seen, so wait until the follower has, before writing.
    fs::write(&log, "").unwrap();
    wait_for_offset(&t.table(), 0);
    append_to(&log, "d\n");
    wait_for(&table, 4);
    table.close().unwrap();
    assert_eq!(thread.join().unwrap().unwrap(), 4);
    let table = Table::open(t.table(), TableOptions::default()).unwrap();
    assert_eq!(raws(&table), ["a", "b", "c", "d"]);
}

#[test]
fn follow_of_a_missing_file_fails() {
    let t = TempDir::new();
    let table = Table::open(t.table(), TableOptions::default()).unwrap();
    let err = table.follow(t.0.join("nope.log")).unwrap_err();
    assert!(matches!(err, IngestError::Io(_)), "{err}");
}

#[test]
fn resume_files_are_named_per_path() {
    let a = state_name(Path::new("/var/log/app.log"));
    let b = state_name(Path::new("/srv/log/app.log"));
    assert!(a.starts_with("app.log-") && a.ends_with(".offset"), "{a}");
    assert_ne!(a, b);
    assert_eq!(a, state_name(Path::new("/var/log/app.log")), "stable");
    assert!(state_name(Path::new("/x/a b?.log")).starts_with("a_b_.log-"));
}

#[test]
fn a_damaged_or_stale_resume_file_starts_from_the_beginning() {
    let t = TempDir::new();
    let log = t.0.join("app.log");
    append_to(&log, "one\ntwo\n");
    let state = t.0.join("state.offset");
    for text in ["garbage", "1 2", "0 0 4"] {
        fs::write(&state, text).unwrap();
        assert_eq!(resume(&log, &state).unwrap().head_off(), 0, "{text}");
    }
    let id = FileIdentity::of(&log).unwrap();
    save(&state, id, 4).unwrap();
    assert_eq!(resume(&log, &state).unwrap().head_off(), 4);
    save(&state, id, 99).unwrap();
    assert_eq!(resume(&log, &state).unwrap().head_off(), 0, "past the end");
}

#[test]
fn trimming_drops_the_newline_and_one_carriage_return() {
    assert_eq!(trimmed(b"a\r\n"), Some(b"a".as_slice()));
    assert_eq!(trimmed(b"a\r\r\n"), Some(b"a\r".as_slice()));
    assert_eq!(trimmed(b"a"), Some(b"a".as_slice()));
    assert_eq!(trimmed(b"\r\n"), None);
    assert_eq!(trimmed(b""), None);
}

/// The name part of [`state_name`] for a file called `name`.
fn state_stem(name: &str) -> String {
    let full = state_name(&Path::new("/d").join(name));
    full.rsplit_once('-').unwrap().0.to_string()
}

#[test]
#[allow(non_snake_case)]
fn mcdc__storage_ingest_handle_state_name_8a402fee__v1_an_alphanumeric_is_kept() {
    assert_eq!(state_stem("a1"), "a1");
}

#[test]
#[allow(non_snake_case)]
fn mcdc__storage_ingest_handle_state_name_8a402fee__v2_a_dot_is_kept() {
    assert_eq!(state_stem("x.y"), "x.y");
}

#[test]
#[allow(non_snake_case)]
fn mcdc__storage_ingest_handle_state_name_8a402fee__v3_anything_else_is_replaced() {
    assert_eq!(state_stem("x.y"), "x.y");
}
