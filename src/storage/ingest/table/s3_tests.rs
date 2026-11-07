//! The writer on a real S3-compatible store (#1015). These need an
//! endpoint, so they are `#[ignore]`d: start Garage with
//! `tools/s3_garage.sh up`, export what it prints, and run
//! `cargo test --features storage-s3 --lib s3_garage -- --include-ignored`.
//!
//! Garage ignores conditional puts, so the store opens in
//! [`CommitMode::SingleWriter`]; the lease and the fence are covered against
//! the in-memory store (`store_tests`).

#![cfg(feature = "storage-s3")]

use super::store_tests::{append, rows, set_clock, sorted, tiered};
use super::*;
use crate::storage::column::VfsFile;
use crate::storage::ingest::store::s3::{CommitMode, Credentials, S3Config, S3Store};
use crate::storage::ingest::StoreReader;

fn env(name: &str) -> String {
    std::env::var(name)
        .map_err(|_| format!("{name} is not set: eval \"$(tools/s3_garage.sh up)\""))
        .unwrap()
}

/// A store over a fresh prefix of the test bucket, and a fresh local dir.
struct Garage {
    store: Store,
    s3: S3Store,
    local: PathBuf,
    prefix: String,
}

impl Garage {
    fn new(tag: &str) -> Garage {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let unique = format!("{tag}-{}-{nanos}", std::process::id());
        let mut cfg = S3Config::new(
            env("DB_CORE_S3_ENDPOINT"),
            env("DB_CORE_S3_REGION"),
            env("DB_CORE_S3_BUCKET"),
            Credentials::from_env().expect("AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY"),
        );
        cfg.prefix = format!("tests/{unique}");
        let prefix = cfg.prefix.clone();
        // Small parts, so a few MiB exercise the multipart path.
        cfg.multipart_threshold = 6 << 20;
        cfg.part_size = 5 << 20;
        let s3 = S3Store::new(cfg).unwrap();
        let local = std::env::temp_dir().join(format!("db-core-s3-{unique}"));
        fs::remove_dir_all(&local).ok();
        Garage {
            store: Store::S3(s3.clone()),
            s3,
            local,
            prefix,
        }
    }

    fn open(&self, options: TableOptions) -> Result<TableWriter, IngestError> {
        TableWriter::open_in(&self.local, self.store.clone(), options)
    }
}

impl Drop for Garage {
    fn drop(&mut self) {
        // Leave the bucket as found.
        for key in self.store.list("").unwrap_or_default() {
            self.store.delete(&key).ok();
        }
        fs::remove_dir_all(&self.local).ok();
    }
}

#[test]
#[ignore = "needs an S3 endpoint: tools/s3_garage.sh up"]
fn s3_garage_meets_the_table_store_contract() {
    let g = Garage::new("contract");
    super::super::store::conformance::run(&g.store);
}

#[test]
#[ignore = "needs an S3 endpoint: tools/s3_garage.sh up"]
fn s3_garage_probe_finds_no_conditional_puts_and_chooses_single_writer() {
    let g = Garage::new("probe");
    // Garage ignores If-None-Match, so Auto settles on the local lock.
    assert!(!g.s3.is_conditional());
    let mut cfg = S3Config::new(
        env("DB_CORE_S3_ENDPOINT"),
        env("DB_CORE_S3_REGION"),
        env("DB_CORE_S3_BUCKET"),
        Credentials::from_env().unwrap(),
    );
    cfg.commit = CommitMode::ConditionalPut;
    let err = S3Store::new(cfg).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::Unsupported, "{err}");
}

#[test]
#[ignore = "needs an S3 endpoint: tools/s3_garage.sh up"]
fn s3_garage_publishes_a_large_file_in_parts_and_reads_it_back_by_range() {
    let g = Garage::new("multipart");
    // 12 MiB with a position-dependent pattern: three parts.
    let data: Vec<u8> = (0..12u32 << 20)
        .map(|i| u8::try_from(i.wrapping_mul(2_654_435_761) >> 24).unwrap_or(0))
        .collect();
    fs::create_dir_all(&g.local).unwrap();
    let file = g.local.join("big");
    fs::write(&file, &data).unwrap();
    g.store.publish(&file, "data/big.bin").unwrap();
    assert!(!file.exists());
    let f = g.store.open("data/big.bin").unwrap();
    assert_eq!(f.size().unwrap(), data.len() as u64);
    // A range across a part boundary (5 MiB) and one at the very end.
    for (offset, len) in [
        (5 * 1024 * 1024 - 100u64, 300usize),
        (data.len() as u64 - 10, 10),
    ] {
        let mut buf = vec![0u8; len];
        assert_eq!(f.read_at(offset, &mut buf).unwrap(), len);
        let from = usize::try_from(offset).unwrap();
        assert_eq!(&buf[..], &data[from..from + len], "range at {offset}");
    }
    assert_eq!(&*f.mmap().unwrap(), &data[..]);
}

#[test]
#[ignore = "needs an S3 endpoint: tools/s3_garage.sh up"]
fn s3_garage_a_flat_table_flushes_recovers_from_a_crash_and_is_readable() {
    for step in [1u8, 2, 3, 5] {
        let g = Garage::new(&format!("flat{step}"));
        let mut w = g.open(TableOptions::default()).unwrap();
        let mut lines = append(&mut w, 0..6);
        w.flush_all().unwrap();
        lines.extend(append(&mut w, 6..20));
        w.seal_head().unwrap();
        w.crash_after = Some(step);
        assert!(w.flush().is_err(), "step {step}");
        drop(w);

        // The WAL is local; whatever the crash left in the bucket is
        // settled by recovery.
        let mut w = g.open(TableOptions::default()).unwrap();
        assert_eq!(
            rows(&w),
            sorted(lines.clone()),
            "after a crash at step {step}"
        );
        let mut listed: Vec<String> = w.manifest().files.iter().map(|e| e.path.clone()).collect();
        listed.sort();
        let stored: Vec<String> = g.store.list("data/").unwrap();
        assert_eq!(stored, listed, "step {step}");
        lines.extend(append(&mut w, 20..25));
        w.flush_all().unwrap();
        assert_eq!(rows(&w), sorted(lines), "step {step}");
        // A reader of the bucket alone sees the flushed files.
        let snapshot = Snapshot::open_in(&g.store).unwrap();
        assert_eq!(*snapshot.manifest(), w.manifest());
    }
}

#[test]
#[ignore = "needs an S3 endpoint: tools/s3_garage.sh up"]
fn s3_garage_hours_days_and_retention() {
    let g = Garage::new("tiered");
    set_clock(0, 10, 0);
    let mut w = g.open(tiered()).unwrap();
    let mut lines = append(&mut w, 0..4);
    set_clock(0, 11, 0);
    lines.extend(append(&mut w, 4..8));
    set_clock(1, 0, 0);
    lines.extend(append(&mut w, 8..9));
    w.flush().unwrap();
    assert!(g.store.list("live/").unwrap().len() >= 2);
    let merged = w.compact().unwrap();
    assert_eq!(merged.len(), 1);
    assert_eq!(rows(&w), sorted(lines.clone()));
    assert_eq!(
        g.store.list("live/").unwrap().len(),
        2,
        "replaced hours wait out the grace period"
    );
    set_clock(1, 0, 61_000_000_000);
    w.compact().unwrap();
    assert!(g.store.list("live/").unwrap().is_empty());
    assert_eq!(g.store.list("archive/").unwrap().len(), 1);
    assert_eq!(rows(&w), sorted(lines));
}

#[test]
#[ignore = "needs an S3 endpoint: tools/s3_garage.sh up"]
fn s3_garage_opens_by_uri_and_a_reader_follows_the_writer() {
    let g = Garage::new("uri");
    let uri = format!(
        "s3://{}/{}?endpoint={}&region={}",
        env("DB_CORE_S3_BUCKET"),
        g.prefix,
        env("DB_CORE_S3_ENDPOINT"),
        env("DB_CORE_S3_REGION"),
    );
    let mut w =
        TableWriter::open_in(&g.local, Store::at(&uri).unwrap(), TableOptions::default()).unwrap();
    let mut lines = append(&mut w, 0..5);
    w.flush_all().unwrap();

    let mut reader = StoreReader::open(Store::at(&uri).unwrap()).unwrap();
    assert_eq!(reader.snapshot().files().count(), 1);
    lines.extend(append(&mut w, 5..12));
    w.flush_all().unwrap();
    assert_eq!(reader.refresh().unwrap().files().count(), 2);
    let rows_in_files: u64 = reader.snapshot().files().map(|(e, _)| e.rows).sum();
    assert_eq!(rows_in_files, lines.len() as u64);

    // A path is refused by `open`, not turned into a directory named "s3:".
    assert!(TableWriter::open(std::path::Path::new(&uri), TableOptions::default()).is_err());
}
