//! The writer on a real S3-compatible store (#1015). These need an
//! endpoint, so they are `#[ignore]`d: start Garage with
//! `tools/s3_garage.sh up`, export what it prints, and run
//! `cargo test --features storage-s3 --lib s3_garage -- --include-ignored`.
//!
//! Garage ignores conditional puts, so the store opens in
//! [`CommitMode::SingleWriter`]; the lease and the fence are covered against
//! the in-memory store (`store_tests`).

#![cfg(feature = "storage-s3")]

use super::store_tests::{
    append, retaining, retention_drops_the_oldest_days, rows, set_clock, sorted, tiered,
};
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
    assert_eq!(reader.snapshot().files().unwrap().len(), 1);
    lines.extend(append(&mut w, 5..12));
    w.flush_all().unwrap();
    assert_eq!(reader.refresh().unwrap().files().unwrap().len(), 2);
    let rows_in_files: u64 = reader
        .snapshot()
        .files()
        .unwrap()
        .into_iter()
        .map(|(e, _)| e.rows)
        .sum();
    assert_eq!(rows_in_files, lines.len() as u64);

    // A path is refused by `open`, not turned into a directory named "s3:".
    assert!(TableWriter::open(std::path::Path::new(&uri), TableOptions::default()).is_err());
}

#[test]
#[ignore = "needs an S3 endpoint: tools/s3_garage.sh up"]
fn s3_garage_retention_drops_the_oldest_days() {
    let g = Garage::new("retention");
    retention_drops_the_oldest_days(|| g.open(retaining(2)).unwrap(), &g.store);
}

#[cfg(feature = "engine-column")]
#[test]
#[ignore = "needs an S3 endpoint: tools/s3_garage.sh up"]
fn s3_garage_the_column_engine_queries_a_table_by_uri() {
    use crate::engine::column::table::TableEngine;
    use crate::engine::{Cell, Engine};
    let g = Garage::new("engine");
    let uri = format!(
        "s3://{}/{}/events?endpoint={}&region={}",
        env("DB_CORE_S3_BUCKET"),
        g.prefix,
        env("DB_CORE_S3_ENDPOINT"),
        env("DB_CORE_S3_REGION"),
    );
    let mut w =
        TableWriter::open_in(&g.local, Store::at(&uri).unwrap(), TableOptions::default()).unwrap();
    append(&mut w, 0..5);
    w.flush_all().unwrap();

    let mut engine = TableEngine::open_at(&uri).unwrap();
    assert_eq!(engine.table_name(), "events");
    assert_eq!(engine.path(), std::path::Path::new(&uri));
    let count = |e: &mut TableEngine| e.run_query("SELECT count(*) FROM events").unwrap().rows;
    assert_eq!(count(&mut engine), vec![vec![Cell::Int(5)]]);
    append(&mut w, 5..9);
    w.flush_all().unwrap();
    assert_eq!(count(&mut engine), vec![vec![Cell::Int(9)]]);
}

/// A store-backed `events` table of ids 1..=`n`, with the typed schema the
/// join tests below read, and the `s3://` URI it was written to.
#[cfg(feature = "engine-column")]
fn typed_events_by_uri(g: &Garage, n: i64) -> (String, TableWriter) {
    use crate::storage::parquet::schema::{ColumnSpec, ColumnType};
    let uri = format!(
        "s3://{}/{}/events?endpoint={}&region={}",
        env("DB_CORE_S3_BUCKET"),
        g.prefix,
        env("DB_CORE_S3_ENDPOINT"),
        env("DB_CORE_S3_REGION"),
    );
    let options = TableOptions {
        schema: Some(vec![ColumnSpec::new("event_id", ColumnType::Int64, false)]),
        ..TableOptions::default()
    };
    let mut w = TableWriter::open_in(&g.local, Store::at(&uri).unwrap(), options).unwrap();
    for id in 1..=n {
        w.append_row(vec![RecordValue::Int(id)], Ack::Durable)
            .unwrap();
    }
    w.flush_all().unwrap();
    (uri, w)
}

/// A SQLite file at `path` with `machine(event_id, host)` for ids 2, 5, 9.
#[cfg(feature = "engine-column")]
fn machine_lookup(path: &std::path::Path) {
    use crate::engine::row::RowEngine;
    use crate::engine::Engine;
    fs::copy("tests/fixtures/btrees/table_single_page.db", path).unwrap();
    RowEngine::open(path)
        .unwrap()
        .run_query(
            "CREATE TABLE machine(event_id INTEGER PRIMARY KEY, host TEXT);\
             INSERT INTO machine VALUES (2, 'h2');\
             INSERT INTO machine VALUES (5, 'h5');\
             INSERT INTO machine VALUES (9, 'h9');",
        )
        .unwrap();
}

#[cfg(feature = "engine-column")]
#[test]
#[ignore = "needs an S3 endpoint: tools/s3_garage.sh up"]
fn s3_garage_a_store_backed_table_joins_a_sqlite_file() {
    use crate::engine::resolve::CrossModeEngine;
    use crate::engine::{Cell, Engine};
    let g = Garage::new("join");
    let (uri, mut w) = typed_events_by_uri(&g, 7);
    let db = g.local.with_extension("lookup.db");
    machine_lookup(&db);

    let mut e = CrossModeEngine::open_column_tables_sqlite(
        &[std::path::Path::new(&uri)],
        &[("lookup", &db)],
    )
    .unwrap();
    let sql = "SELECT machine.host FROM events \
               JOIN machine ON events.event_id = machine.event_id \
               ORDER BY machine.host";
    let hosts = |e: &mut CrossModeEngine| e.run_query(sql).unwrap().rows;
    let host = |h: &str| vec![Cell::Text(h.into())];
    assert_eq!(hosts(&mut e), vec![host("h2"), host("h5")]);

    // Each query reads a fresh snapshot: ids flushed later join too.
    for id in 8..=10 {
        w.append_row(vec![RecordValue::Int(id)], Ack::Durable)
            .unwrap();
    }
    w.flush_all().unwrap();
    assert_eq!(hosts(&mut e), vec![host("h2"), host("h5"), host("h9")]);
    fs::remove_file(&db).ok();
}

#[cfg(feature = "engine-column")]
#[test]
#[ignore = "needs an S3 endpoint: tools/s3_garage.sh up"]
fn s3_garage_a_store_table_and_a_local_table_join_in_one_session() {
    use crate::engine::resolve::CrossModeEngine;
    use crate::engine::{Cell, Engine};
    use crate::storage::parquet::schema::{ColumnSpec, ColumnType};
    let g = Garage::new("mixed");
    let (uri, _w) = typed_events_by_uri(&g, 10);
    let db = g.local.with_extension("lookup.db");
    machine_lookup(&db);

    // `plain` is a local table directory holding ids 4..=6.
    // The table is named after its directory.
    let holder = g.local.with_extension("holder");
    let plain = holder.join("plain");
    fs::remove_dir_all(&holder).ok();
    fs::create_dir_all(&holder).unwrap();
    let options = TableOptions {
        schema: Some(vec![ColumnSpec::new("event_id", ColumnType::Int64, false)]),
        ..TableOptions::default()
    };
    let mut p = TableWriter::open(&plain, options).unwrap();
    for id in 4..=6 {
        p.append_row(vec![RecordValue::Int(id)], Ack::Durable)
            .unwrap();
    }
    p.flush_all().unwrap();

    let mut e = CrossModeEngine::open_column_tables_sqlite(
        &[std::path::Path::new(&uri), plain.as_path()],
        &[("lookup", &db)],
    )
    .unwrap();
    assert_eq!(
        e.run_query("SELECT count(*) FROM events JOIN plain ON events.event_id = plain.event_id")
            .unwrap()
            .rows,
        vec![vec![Cell::Int(3)]]
    );
    fs::remove_file(&db).ok();
    fs::remove_dir_all(&holder).ok();
}
