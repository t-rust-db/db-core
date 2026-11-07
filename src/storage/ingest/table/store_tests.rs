//! The writer on a store that is not the local directory (#1015): the
//! manifest and the Parquet files live in an in-memory store, the WAL, the
//! lock and the scratch directory in a local one.

use super::*;
use crate::storage::ingest::layout::{TierOptions, DAY_NS, HOUR_NS};
use crate::storage::ingest::store::memory::MemoryStore;
use crate::storage::parquet::ParquetFile;
use std::sync::atomic::{AtomicU64, Ordering};

pub(super) fn clock() -> i64 {
    crate::test_clock::now(0)
}

/// Sets the clock to `day` days and `hour` hours past 2026-10-11, plus `ns`.
pub(super) fn set_clock(day: i64, hour: i64, ns: i64) {
    crate::test_clock::set((20_737 + day) * DAY_NS + hour * HOUR_NS + ns);
}

/// A fresh local directory and an empty in-memory store.
struct Fixture {
    local: PathBuf,
    mem: Arc<MemoryStore>,
}

impl Fixture {
    fn new() -> Self {
        Fixture::with(MemoryStore::default())
    }

    fn with(mem: MemoryStore) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let local =
            std::env::temp_dir().join(format!("db-core-store-w-{}-{n}", std::process::id()));
        fs::remove_dir_all(&local).ok();
        Fixture {
            local,
            mem: Arc::new(mem),
        }
    }

    /// A fixture whose store honours conditional puts, like AWS S3.
    fn conditional() -> Self {
        Fixture::with(MemoryStore::conditional())
    }

    fn store(&self) -> Store {
        Store::Memory(Arc::clone(&self.mem))
    }

    /// Another process's view of the same bucket: its own manifest fence.
    fn client(&self) -> Store {
        Store::Memory(Arc::new(self.mem.client()))
    }

    /// A second local directory, as on another machine.
    fn other_local(&self) -> PathBuf {
        let mut dir = self.local.clone().into_os_string();
        dir.push("-other");
        let dir = PathBuf::from(dir);
        fs::remove_dir_all(&dir).ok();
        dir
    }

    fn open(&self, options: TableOptions) -> Result<TableWriter, IngestError> {
        TableWriter::open_in(&self.local, self.store(), options)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.local).ok();
    }
}

pub(super) fn line(i: usize) -> String {
    let kind = ["c", "a", "b"][i % 3];
    format!(r#"{{"i":{i},"kind":"{kind}"}}"#)
}

pub(super) fn append(w: &mut TableWriter, range: std::ops::Range<usize>) -> Vec<String> {
    range
        .map(|i| {
            let l = line(i);
            w.append(l.as_bytes(), Ack::Durable).unwrap();
            l
        })
        .collect()
}

/// Every record's `raw` the writer holds (files in the store, then pages),
/// sorted.
pub(super) fn rows(w: &TableWriter) -> Vec<String> {
    let s = w.snapshot().unwrap();
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
        for i in 0..page.lines().unwrap().len() {
            rows.push(String::from_utf8(page.lines().unwrap().raw_line(i).to_vec()).unwrap());
        }
    }
    rows.sort();
    rows
}

pub(super) fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

/// The store's Parquet keys are exactly the manifest's files.
fn assert_store_matches_manifest(f: &Fixture, w: &TableWriter) {
    let mut listed: Vec<String> = w.manifest().files.iter().map(|e| e.path.clone()).collect();
    listed.sort();
    let mut stored: Vec<String> = f
        .mem
        .list("")
        .unwrap()
        .into_iter()
        .filter(|k| k.ends_with(".parquet"))
        .collect();
    stored.sort();
    assert_eq!(stored, listed);
}

pub(super) fn tiered() -> TableOptions {
    TableOptions {
        layout: Layout::Tiered(TierOptions {
            row_group_max_bytes: None,
            spill_bytes: None,
            sort_key: vec!["kind".to_string()],
            row_group_max_rows: 3,
            max_file_size: 1 << 30,
            delete_grace_ns: 60_000_000_000,
            spill_rows: usize::MAX,
            retention_days: None,
            session_key: None,
            stat_columns: Vec::new(),
        }),
        clock,
        ..TableOptions::default()
    }
}

#[test]
fn a_flush_lands_in_the_store_and_nothing_else_is_local() {
    let f = Fixture::new();
    let mut w = f.open(TableOptions::default()).unwrap();
    let lines = append(&mut w, 0..5);
    w.flush_all().unwrap();
    assert_eq!(w.manifest().files.len(), 1);
    assert_store_matches_manifest(&f, &w);
    assert!(f.mem.get("manifest.json").unwrap().is_some());
    assert!(
        !f.local.join("data").exists(),
        "no Parquet on the local disk"
    );
    assert!(!f.local.join("manifest.json").exists());
    assert_eq!(rows(&w), sorted(lines.clone()));
    // A reader of the store alone sees the flushed rows.
    let snapshot = Snapshot::open_in(&f.store()).unwrap();
    assert_eq!(*snapshot.manifest(), w.manifest());
    assert_eq!(snapshot.files().count(), 1);
}

#[test]
fn a_crash_at_every_flush_step_recovers_every_record_once_on_the_store() {
    for step in [1u8, 2, 3, 5] {
        let f = Fixture::new();
        let mut w = f.open(TableOptions::default()).unwrap();
        let mut lines = append(&mut w, 0..6);
        w.flush_all().unwrap();
        lines.extend(append(&mut w, 6..20));
        w.seal_head().unwrap();
        w.crash_after = Some(step);
        assert!(w.flush().is_err(), "step {step}");
        drop(w);
        if step == 2 {
            // Published, not committed: the orphan is in the store.
            assert_eq!(f.mem.list("data/").unwrap().len(), 2);
        }

        // The WAL is local, so the records come back from it; an orphan in
        // the store is deleted.
        let mut w = f.open(TableOptions::default()).unwrap();
        assert_eq!(
            rows(&w),
            sorted(lines.clone()),
            "after a crash at step {step}"
        );
        assert_store_matches_manifest(&f, &w);
        assert_eq!(w.manifest().flushed_lsn, if step >= 3 { 20 } else { 6 });
        lines.extend(append(&mut w, 20..25));
        w.flush_all().unwrap();
        assert_eq!(rows(&w), sorted(lines), "step {step}");
        w.manifest().check().unwrap();
    }
}

#[test]
fn a_crashed_manifest_commit_keeps_the_previous_manifest() {
    let f = Fixture::new();
    let mut w = f.open(TableOptions::default()).unwrap();
    append(&mut w, 0..3);
    w.flush_all().unwrap();
    let committed = w.manifest().clone();
    append(&mut w, 3..6);
    w.seal_head().unwrap();
    // The store dies at the commit: the Parquet file is published, then the
    // manifest put fails.
    f.mem.crash_after(1);
    assert!(w.flush().is_err());
    drop(w);
    f.mem.lift_crash();
    assert_eq!(Manifest::read_from(&f.store()).unwrap().unwrap(), committed);
}

#[test]
fn a_second_writer_on_the_same_local_directory_is_refused() {
    let f = Fixture::new();
    let _first = f.open(TableOptions::default()).unwrap();
    assert!(matches!(
        f.open(TableOptions::default()),
        Err(IngestError::Wal(WalError::Locked))
    ));
}

#[test]
fn hours_days_and_retention_run_on_the_store() {
    let f = Fixture::new();
    set_clock(0, 10, 0);
    let mut w = f.open(tiered()).unwrap();
    let mut lines = append(&mut w, 0..4);
    set_clock(0, 11, 0);
    lines.extend(append(&mut w, 4..8));
    // The next day: both hours close, and day 0 merges into the archive.
    set_clock(1, 0, 0);
    lines.extend(append(&mut w, 8..9));
    w.flush().unwrap();
    assert_store_matches_manifest(&f, &w);
    assert!(f.mem.list("live/").unwrap().len() >= 2);
    assert!(!f.local.join("live").exists());
    let merged = w.compact().unwrap();
    assert_eq!(merged.len(), 1);
    assert!(merged[0].path.starts_with("archive/date="));
    assert_eq!(rows(&w), sorted(lines.clone()));

    // The replaced hours wait out the grace period, then leave the store.
    assert_eq!(w.pending_deletes().count(), 2);
    assert_eq!(f.mem.list("live/").unwrap().len(), 2);
    assert_eq!(f.mem.list("archive/").unwrap().len(), 1);
    set_clock(1, 0, 61_000_000_000);
    w.compact().unwrap();
    assert_eq!(w.pending_deletes().count(), 0);
    assert_store_matches_manifest(&f, &w);
    assert!(f.mem.list("live/").unwrap().is_empty());
    assert_eq!(rows(&w), sorted(lines));
}

const EXPIRED_LEASE: &[u8] = b"owner=gone\nexpires_ms=1\n";

#[test]
fn a_second_machine_on_the_same_prefix_is_refused_until_the_first_lets_go() {
    let f = Fixture::conditional();
    let other = f.other_local();
    let first = TableWriter::open_in(&f.local, f.client(), TableOptions::default()).unwrap();
    assert!(matches!(
        TableWriter::open_in(&other, f.client(), TableOptions::default()),
        Err(IngestError::Wal(WalError::Locked))
    ));
    drop(first);
    // A clean shutdown frees the lease at once.
    let second = TableWriter::open_in(&other, f.client(), TableOptions::default());
    assert!(second.is_ok());
    drop(second);
    fs::remove_dir_all(&other).ok();
}

#[test]
fn a_dead_writers_expired_lease_is_taken_over() {
    let f = Fixture::conditional();
    f.mem.put("LOCK", EXPIRED_LEASE).unwrap();
    assert!(TableWriter::open_in(&f.local, f.client(), TableOptions::default()).is_ok());
}

#[test]
fn a_stale_writer_cannot_overwrite_the_commit_of_the_one_that_took_over() {
    let f = Fixture::conditional();
    let other = f.other_local();
    let mut a = TableWriter::open_in(&f.local, f.client(), TableOptions::default()).unwrap();
    append(&mut a, 0..3);
    a.flush_all().unwrap();
    // A's lease runs out (it stalled), and B takes the table over.
    f.mem.put("LOCK", EXPIRED_LEASE).unwrap();
    let mut b = TableWriter::open_in(&other, f.client(), TableOptions::default()).unwrap();
    let mut lines = append(&mut b, 3..6);
    b.flush_all().unwrap();
    let committed = b.manifest();

    // A wakes up and flushes. Its file has the name B's flush used (both
    // count LSNs on from the same manifest), so publishing it must not
    // replace B's file; the manifest commit would be fenced as well. With
    // the inline flusher the refusal comes from `seal_head`.
    append(&mut a, 6..9);
    let err = a
        .seal_head()
        .and_then(|()| a.flush().map(|_| ()))
        .unwrap_err();
    assert!(err.to_string().contains("another writer"), "{err}");
    assert_eq!(Manifest::read_from(&f.store()).unwrap().unwrap(), committed);
    // B's data is intact: its rows (and what a had flushed) are all there.
    lines.extend((0..3).map(line));
    assert_eq!(rows(&b), sorted(lines));
    drop((a, b));
    fs::remove_dir_all(&other).ok();
}

#[test]
fn a_store_without_conditional_puts_has_no_lease_and_no_fence() {
    // Garage: the writer's local lock is the only single-writer protection.
    let f = Fixture::new();
    let other = f.other_local();
    let _a = TableWriter::open_in(&f.local, f.store(), TableOptions::default()).unwrap();
    assert!(TableWriter::open_in(&other, f.store(), TableOptions::default()).is_ok());
    fs::remove_dir_all(&other).ok();
}
