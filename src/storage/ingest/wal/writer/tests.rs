//! WAL and replay tests over real files in a temp directory.

use super::*;
use crate::storage::ingest::frame;
use crate::storage::ingest::recover;
use std::fs;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

/// A fresh table directory per test.
struct TempTable(PathBuf);

impl TempTable {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("db-core-wal-{}-{n}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        TempTable(dir)
    }

    fn wal_dir(&self) -> PathBuf {
        self.0.join(WAL_DIR)
    }

    /// Segment file names, sorted.
    fn files(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.wal_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }
}

impl Drop for TempTable {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn open(t: &TempTable, flushed: u64) -> (Wal, Vec<(u64, Vec<u8>)>) {
    Wal::open(&t.0, flushed, WalOptions::default()).unwrap()
}

fn records(n: u64) -> Vec<(u64, Vec<u8>)> {
    (1..=n)
        .map(|i| (i, format!("record {i}").into_bytes()))
        .collect()
}

/// Writes `n` records with one segment per `per_file`, then closes.
fn write_records(t: &TempTable, n: u64, per_file: u64) {
    let (wal, replayed) = open(t, 0);
    assert!(replayed.is_empty());
    for (lsn, payload) in records(n) {
        assert_eq!(wal.append(&payload, Ack::Durable).unwrap(), lsn);
        if lsn % per_file == 0 {
            wal.roll().unwrap();
        }
    }
    wal.close().unwrap();
}

#[test]
fn appends_replay_after_reopen_and_lsns_continue() {
    let t = TempTable::new();
    write_records(&t, 5, 100);
    let (wal, replayed) = open(&t, 0);
    assert_eq!(replayed, records(5));
    assert_eq!(
        wal.watermarks().unwrap(),
        Watermarks {
            visible: 5,
            durable: 5,
            flushed: 0
        }
    );
    assert_eq!(wal.append(b"six", Ack::Durable).unwrap(), 6);
    drop(wal);
    let (_wal, replayed) = open(&t, 4);
    assert_eq!(
        replayed,
        vec![(5, b"record 5".to_vec()), (6, b"six".to_vec())]
    );
}

#[test]
fn flushed_beyond_the_wal_sets_the_next_lsn() {
    // A table whose WAL was entirely flushed and unlinked.
    let t = TempTable::new();
    let (wal, replayed) = open(&t, 41);
    assert!(replayed.is_empty());
    assert_eq!(wal.append(b"x", Ack::Durable).unwrap(), 42);
}

#[test]
fn group_commit_covers_many_appends_per_fsync() {
    let t = TempTable::new();
    let (wal, _) = open(&t, 0);
    let threads = 8;
    let each = 200;
    let mut lsns: Vec<u64> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|i| {
                let wal = &wal;
                s.spawn(move || {
                    (0..each)
                        .map(|j| {
                            wal.append(format!("{i}:{j}").as_bytes(), Ack::Durable)
                                .unwrap()
                        })
                        .collect::<Vec<u64>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });
    lsns.sort_unstable();
    assert_eq!(lsns, (1..=threads * each).collect::<Vec<u64>>());
    let marks = wal.watermarks().unwrap();
    assert_eq!((marks.visible, marks.durable), (1600, 1600));
    assert!(
        wal.syncs() < 1600,
        "{} fsyncs for 1600 appends",
        wal.syncs()
    );
    drop(wal);
    assert_eq!(open(&t, 0).1.len(), 1600);
}

#[test]
fn memory_acks_become_durable_on_sync() {
    let t = TempTable::new();
    let (wal, _) = open(&t, 0);
    for i in 0..50u64 {
        assert_eq!(wal.append(b"m", Ack::Memory).unwrap(), i + 1);
        let m = wal.watermarks().unwrap();
        assert!(m.flushed <= m.durable && m.durable <= m.visible);
    }
    assert_eq!(wal.sync().unwrap(), 50);
    assert_eq!(wal.watermarks().unwrap().durable, 50);
}

#[test]
fn flushed_ack_waits_for_mark_flushed() {
    let t = TempTable::new();
    let (wal, _) = open(&t, 0);
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|s| {
        s.spawn(|| tx.send(wal.append(b"f", Ack::Flushed)).unwrap());
        // Durable soon, but not flushed until marked.
        while wal.watermarks().unwrap().durable < 1 {
            std::thread::yield_now();
        }
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
        wal.mark_flushed(1).unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap(), 1);
    });
}

#[test]
fn mark_flushed_above_durable_is_refused() {
    let t = TempTable::new();
    let (wal, _) = open(&t, 0);
    wal.append(b"a", Ack::Durable).unwrap();
    let err = wal.mark_flushed(2).unwrap_err();
    assert_eq!(err.to_string(), "wal: flushed LSN 2 is above durable LSN 1");
    wal.mark_flushed(1).unwrap();
    wal.mark_flushed(0).unwrap(); // never moves backwards
    assert_eq!(wal.watermarks().unwrap().flushed, 1);
}

#[test]
fn roll_starts_files_and_mark_flushed_unlinks_whole_ones() {
    let t = TempTable::new();
    let (wal, _) = open(&t, 0);
    for i in 1..=6u64 {
        wal.append(b"r", Ack::Durable).unwrap();
        if matches!(i, 3 | 5) {
            wal.roll().unwrap();
        }
    }
    wal.sync().unwrap();
    let name = |lo: u64| recover::segment_name(lo);
    assert_eq!(t.files(), vec![name(1), name(4), name(6)]);
    wal.mark_flushed(4).unwrap(); // file 4 holds 4..=5: kept
    assert_eq!(t.files(), vec![name(4), name(6)]);
    wal.mark_flushed(6).unwrap(); // the newest file always stays
    assert_eq!(t.files(), vec![name(6)]);
    drop(wal);
    let (_wal, replayed) = open(&t, 6);
    assert!(replayed.is_empty());
}

#[test]
fn segments_roll_by_size() {
    let t = TempTable::new();
    let options = WalOptions { segment_bytes: 100 };
    let (wal, _) = Wal::open(&t.0, 0, options).unwrap();
    for _ in 0..10 {
        wal.append(&[7u8; 60], Ack::Durable).unwrap(); // 76-byte frames
    }
    drop(wal);
    assert_eq!(t.files().len(), 5, "{:?}", t.files());
    assert_eq!(open(&t, 0).1.len(), 10);
}

#[test]
fn a_second_writer_is_locked_out() {
    let t = TempTable::new();
    let (_wal, _) = open(&t, 0);
    let err = Wal::open(&t.0, 0, WalOptions::default()).unwrap_err();
    assert!(matches!(err, WalError::Locked), "{err}");
}

#[test]
fn closing_wakes_waiters_with_closed() {
    let t = TempTable::new();
    let (wal, _) = open(&t, 0);
    std::thread::scope(|s| {
        let waiter = s.spawn(|| wal.append(b"w", Ack::Flushed));
        while wal.watermarks().unwrap().visible < 1 {
            std::thread::yield_now();
        }
        wal.shared.lock().unwrap().closing = true;
        wal.shared.progress.notify_all();
        assert!(matches!(waiter.join().unwrap(), Err(WalError::Closed)));
    });
    assert!(matches!(
        wal.append(b"late", Ack::Memory),
        Err(WalError::Closed)
    ));
}

#[cfg(unix)]
#[test]
fn an_io_failure_stops_the_wal() {
    use std::os::unix::fs::PermissionsExt;
    let t = TempTable::new();
    let (wal, _) = open(&t, 0);
    // The first write must create a segment file; a read-only directory
    // makes that fail.
    fs::set_permissions(t.wal_dir(), fs::Permissions::from_mode(0o555)).unwrap();
    let err = wal.append(b"x", Ack::Durable).unwrap_err();
    fs::set_permissions(t.wal_dir(), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(err, WalError::Failed(_)), "{err}");
    assert!(matches!(
        wal.append(b"y", Ack::Memory),
        Err(WalError::Failed(_))
    ));
    assert!(matches!(wal.sync(), Err(WalError::Failed(_))));
    assert!(matches!(wal.close(), Err(WalError::Failed(_))));
}

// --- replay ------------------------------------------------------------------

fn last_file(t: &TempTable) -> PathBuf {
    t.wal_dir().join(t.files().last().unwrap())
}

#[test]
fn every_torn_tail_replays_its_complete_prefix() {
    let t = TempTable::new();
    write_records(&t, 4, 2); // files 1 (lsn 1-2) and 3 (lsn 3-4)
    let path = last_file(&t);
    let full = fs::read(&path).unwrap();
    let frame_len = full.len() / 2;
    for cut in 0..full.len() {
        fs::write(&path, &full[..cut]).unwrap();
        let (wal, replayed) = open(&t, 0);
        let complete = cut / frame_len;
        assert_eq!(replayed, records(2 + complete as u64), "cut at {cut}");
        drop(wal);
        // Truncated to the last whole frame (or removed when empty), and
        // a second open sees the same thing.
        let len = fs::metadata(&path).map_or(0, |m| m.len() as usize);
        assert_eq!(len, complete * frame_len, "cut at {cut}");
        assert_eq!(open(&t, 0).1, records(2 + complete as u64));
        fs::write(&path, &full).unwrap();
    }
}

#[test]
fn a_torn_tail_then_appends_continue() {
    let t = TempTable::new();
    write_records(&t, 3, 100);
    let path = last_file(&t);
    let full = fs::read(&path).unwrap();
    fs::write(&path, &full[..full.len() - 3]).unwrap();
    let (wal, replayed) = open(&t, 0);
    assert_eq!(replayed, records(2));
    assert_eq!(wal.append(b"next", Ack::Durable).unwrap(), 3);
    drop(wal);
    let replayed = open(&t, 0).1;
    assert_eq!(replayed.last().unwrap(), &(3, b"next".to_vec()));
}

#[test]
fn a_bad_last_frame_is_a_torn_tail() {
    let t = TempTable::new();
    write_records(&t, 3, 100);
    let path = last_file(&t);
    let mut bytes = fs::read(&path).unwrap();
    let n = bytes.len();
    bytes[n - 1] ^= 0xff;
    fs::write(&path, &bytes).unwrap();
    assert_eq!(open(&t, 0).1, records(2));
}

#[test]
fn corruption_before_valid_frames_refuses_to_open() {
    let t = TempTable::new();
    write_records(&t, 3, 100);
    let path = last_file(&t);
    let mut bytes = fs::read(&path).unwrap();
    bytes[frame::HEADER_LEN + 2] ^= 0x01; // inside record 1's payload
    fs::write(&path, &bytes).unwrap();
    let err = Wal::open(&t.0, 0, WalOptions::default()).unwrap_err();
    assert!(
        err.to_string()
            .contains("bad frame followed by valid frames"),
        "{err}"
    );
    assert_eq!(fs::read(&path).unwrap(), bytes, "nothing truncated");
}

#[test]
fn corruption_in_an_earlier_file_refuses_to_open() {
    let t = TempTable::new();
    write_records(&t, 4, 2);
    let first = t.wal_dir().join(&t.files()[0]);
    let mut bytes = fs::read(&first).unwrap();
    let n = bytes.len();
    bytes[n - 1] ^= 0xff; // its last frame: would look torn if it were last
    fs::write(&first, &bytes).unwrap();
    let err = Wal::open(&t.0, 0, WalOptions::default()).unwrap_err();
    assert!(
        err.to_string()
            .contains("complete before the next one started"),
        "{err}"
    );
}

#[test]
fn a_missing_file_refuses_to_open() {
    let t = TempTable::new();
    write_records(&t, 6, 2); // files 1, 3, 5
    fs::remove_file(t.wal_dir().join(recover::segment_name(3))).unwrap();
    let err = Wal::open(&t.0, 0, WalOptions::default()).unwrap_err();
    assert!(err.to_string().contains("frames are missing"), "{err}");
    // Missing at the front, below nothing flushed.
    let t = TempTable::new();
    write_records(&t, 4, 2);
    fs::remove_file(t.wal_dir().join(recover::segment_name(1))).unwrap();
    assert!(Wal::open(&t.0, 0, WalOptions::default()).is_err());
    // ...but fine once those LSNs are flushed.
    assert_eq!(
        open(&t, 2).1,
        vec![(3, b"record 3".to_vec()), (4, b"record 4".to_vec())]
    );
}

#[test]
fn an_lsn_gap_inside_a_file_refuses_to_open() {
    let t = TempTable::new();
    fs::create_dir_all(t.wal_dir()).unwrap();
    let mut bytes = Vec::new();
    frame::encode(&mut bytes, 1, b"a").unwrap();
    frame::encode(&mut bytes, 3, b"c").unwrap();
    fs::write(t.wal_dir().join(recover::segment_name(1)), &bytes).unwrap();
    let err = Wal::open(&t.0, 0, WalOptions::default()).unwrap_err();
    assert!(err.to_string().contains("LSN 3 follows 1"), "{err}");
}

#[test]
fn flushed_files_are_deleted_and_strays_ignored() {
    let t = TempTable::new();
    write_records(&t, 6, 2); // files 1, 3, 5
    fs::write(t.wal_dir().join("notes.txt"), b"not a segment").unwrap();
    fs::write(t.wal_dir().join("7.wal"), b"short name: not a segment").unwrap();
    let (_wal, replayed) = open(&t, 4);
    assert_eq!(
        replayed,
        vec![(5, b"record 5".to_vec()), (6, b"record 6".to_vec())]
    );
    let mut expected = vec![
        "7.wal".to_string(),
        recover::segment_name(5),
        "notes.txt".to_string(),
    ];
    expected.sort();
    assert_eq!(t.files(), expected);
}

#[test]
fn an_empty_last_file_is_removed() {
    let t = TempTable::new();
    write_records(&t, 2, 100);
    fs::write(t.wal_dir().join(recover::segment_name(3)), b"").unwrap();
    let (wal, replayed) = open(&t, 0);
    assert_eq!(replayed, records(2));
    assert_eq!(wal.append(b"3", Ack::Durable).unwrap(), 3);
}

// --- read-only tail (#572) ---------------------------------------------------

#[test]
fn read_tail_reads_above_flushed_and_changes_nothing() {
    let t = TempTable::new();
    write_records(&t, 6, 2); // files 1, 3, 5
    let before = t.files();
    for flushed in 0..=6 {
        let tail = recover::read_tail(&t.wal_dir(), flushed).unwrap().unwrap();
        assert_eq!(tail, records(6)[flushed as usize..].to_vec(), "{flushed}");
    }
    assert_eq!(t.files(), before);
    // No WAL at all: nothing above flushed.
    let empty = TempTable::new();
    assert_eq!(
        recover::read_tail(&empty.wal_dir(), 0).unwrap(),
        Some(vec![])
    );
}

#[test]
fn read_tail_stops_at_a_partial_frame_without_truncating() {
    let t = TempTable::new();
    write_records(&t, 4, 2);
    let path = last_file(&t);
    let full = fs::read(&path).unwrap();
    let frame_len = full.len() / 2;
    for cut in 0..full.len() {
        fs::write(&path, &full[..cut]).unwrap();
        let tail = recover::read_tail(&t.wal_dir(), 0).unwrap().unwrap();
        assert_eq!(tail, records(2 + (cut / frame_len) as u64), "cut at {cut}");
        assert_eq!(fs::read(&path).unwrap().len(), cut, "untouched");
    }
}

#[test]
fn read_tail_reports_a_flush_that_moved_on() {
    let t = TempTable::new();
    write_records(&t, 6, 2);
    // The writer flushed through 4 and unlinked files 1 and 3, but this
    // reader's manifest says 0.
    fs::remove_file(t.wal_dir().join(recover::segment_name(1))).unwrap();
    fs::remove_file(t.wal_dir().join(recover::segment_name(3))).unwrap();
    assert_eq!(recover::read_tail(&t.wal_dir(), 0).unwrap(), None);
    assert_eq!(
        recover::read_tail(&t.wal_dir(), 4).unwrap(),
        Some(records(6)[4..].to_vec())
    );
}

#[test]
fn read_tail_refuses_real_corruption() {
    let t = TempTable::new();
    write_records(&t, 4, 2);
    let first = t.wal_dir().join(&t.files()[0]);
    let mut bytes = fs::read(&first).unwrap();
    let n = bytes.len();
    bytes[n - 1] ^= 0xff;
    fs::write(&first, &bytes).unwrap();
    let err = recover::read_tail(&t.wal_dir(), 0).unwrap_err();
    assert!(
        err.to_string().contains("complete before the next"),
        "{err}"
    );

    let t = TempTable::new();
    fs::create_dir_all(t.wal_dir()).unwrap();
    let mut bytes = Vec::new();
    frame::encode(&mut bytes, 1, b"a").unwrap();
    frame::encode(&mut bytes, 3, b"c").unwrap();
    fs::write(t.wal_dir().join(recover::segment_name(1)), &bytes).unwrap();
    let err = recover::read_tail(&t.wal_dir(), 0).unwrap_err();
    assert!(err.to_string().contains("LSN 3 where 2"), "{err}");

    // A WAL path that is not a directory.
    let t = TempTable::new();
    fs::create_dir_all(&t.0).unwrap();
    fs::write(t.wal_dir(), b"x").unwrap();
    assert!(recover::read_tail(&t.wal_dir(), 0).is_err());
}

#[test]
fn error_messages() {
    let corrupt = WalError::Corrupt {
        file: PathBuf::from("wal/x.wal"),
        offset: 7,
        reason: "why".into(),
    };
    assert_eq!(corrupt.to_string(), "wal: wal/x.wal at byte 7: why");
    assert_eq!(
        WalError::TooLarge.to_string(),
        "wal: record larger than 4 GiB"
    );
    assert_eq!(
        WalError::Locked.to_string(),
        "wal: the table is locked by another writer"
    );
    assert_eq!(WalError::Closed.to_string(), "wal: closed");
    assert_eq!(
        WalError::from(io::Error::other("disk")).to_string(),
        "wal: disk"
    );
}
