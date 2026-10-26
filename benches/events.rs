//! The high-volume event workload (#602): a seeded stream of events
//! ingested into a tiered record table sorted by `session_id,
//! request_id`, then the queries an event store answers. **Report only**
//! -- `make bench-events` (`tools/bench_events.sh`), not a CI gate (ADR
//! 0015, tier 6).
//!
//! One binary, one step per process, so the driving script can time each
//! step and read its peak memory from `/usr/bin/time`:
//!
//! - `ingest <dir>`: generates the events and appends them with group
//!   commit (`Ack::Memory`, `sync()` every `SYNC_EVERY` rows), flushing
//!   each hour as it closes (a periodic flusher), then compacts every
//!   closed day into `archive/`. Prints `key=value` timings.
//! - `csv <file>`: the same events as CSV, for the row-store baseline.
//! - `query <dir> <sql>`: one query over the table directory, the result
//!   as CSV on stdout and `elapsed_ms=` on stderr.
//! - `build <dir> <predicate>`: a join build side or window input read as
//!   `read_snapshot_where` does (#777), against the whole table: rows and
//!   files read each way, as `key=value`.
//! - `tail <dir>`: `TAIL` (default 1M) events in the WAL of a table that
//!   never flushes, as a tiered table's open hour is, read from another
//!   process's side two ways (#593): a fresh `Snapshot::open` and a
//!   long-lived `TableReader`, first and after `TAIL_MORE` (1000) more
//!   events. Prints the times as `key=value`.
//! - `sid <n>`: the `session_id` of the n-th session, for the script.
//! - `files <dir>`: the table's Parquet files, for the DuckDB baseline.
//! - `storage <dir>`: rows, bytes and bytes per event of those files.
//!
//! The workload (`EVENTS`, default 10M, over `DAYS` days, default 7,
//! `SEED` 42): sessions of 1-30 events, about 4096 of them open at once,
//! so a session spans about an hour of events and crosses hour
//! boundaries; 20 event names, skewed (`page_view` 40%); `amount` only on
//! `checkout`. Event time drives the table clock, so hours and days are
//! the events' own. `CODEC` (`snappy`, the default, `zstd` or
//! `uncompressed`) is the page codec of every file (#622). `STAT_COLUMNS`
//! (default `day`) lists the columns that get a per-file range (#777), and
//! `SESSION_KEY_SECS` turns on `session_key` (#599) with session ids that
//! carry their start time in the high 32 bits; it must cover the longest
//! session, or the answers differ from DuckDB's (a session spans about an
//! hour at 10M events, days at 200k).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "benches/ is unconstrained like tests/ (ADR 0015, tier 6)"
)]

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Instant;

use db_core::engine::column::table::{
    query_snapshot, query_snapshot_files, read_snapshot, read_snapshot_where, SnapshotFilter,
};
use db_core::engine::Cell;
use db_core::storage::ingest::layout::{SessionKey, DAY_NS, HOUR_NS};
use db_core::storage::ingest::wal::Ack;
use db_core::storage::ingest::TableReader;
use db_core::storage::ingest::{
    Layout, RecordValue, Snapshot, TableOptions, TableWriter, TierOptions,
};
use db_core::storage::parquet::writer::{ColumnSpec, ColumnType, Compression, WriterOptions};

/// 2026-10-11, the first event's day.
const BASE_NS: i64 = 20_737 * DAY_NS;
/// Sessions open at once.
const POOL: usize = 4096;
const OTHER_NAMES: [&str; 15] = [
    "login",
    "logout",
    "view_item",
    "remove_from_cart",
    "share",
    "rate",
    "wishlist",
    "coupon",
    "error",
    "retry",
    "scroll",
    "hover",
    "download",
    "upload",
    "settings",
];

static NOW: AtomicI64 = AtomicI64::new(BASE_NS);

fn clock() -> i64 {
    NOW.load(Ordering::SeqCst)
}

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// xorshift64*: seeded, the same stream on every machine.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

struct Event {
    session_id: i64,
    request_id: i64,
    ts_ns: i64,
    name: &'static str,
    user_id: i64,
    amount: Option<i64>,
}

impl Event {
    fn day(&self) -> i64 {
        self.ts_ns.div_euclid(DAY_NS)
    }
}

#[derive(Clone, Copy, Default)]
struct Session {
    id: i64,
    next_request: i64,
    remaining: u64,
    user_id: i64,
}

/// A session's id: its ordinal, or with `SESSION_KEY_SECS` its start in
/// seconds over its ordinal, as `session_key` expects.
fn session_id(ordinal: i64, start_ns: i64) -> i64 {
    if session_key_secs().is_some() {
        (start_ns / 1_000_000_000) << 32 | ordinal
    } else {
        ordinal
    }
}

/// `SESSION_KEY_SECS`: the table's `session_key` window, when set.
fn session_key_secs() -> Option<u32> {
    std::env::var("SESSION_KEY_SECS").ok()?.parse().ok()
}

/// The workload, in event-time order.
fn events() -> impl Iterator<Item = Event> {
    let total = env("EVENTS", 10_000_000);
    let span = env("DAYS", 7) as i64 * DAY_NS;
    let mut rng = Rng(env("SEED", 42) | 1);
    let mut pool = [Session::default(); POOL];
    let mut next_session = 0_i64;
    (0..total).map(move |i| {
        let ts_ns = BASE_NS + (i as i128 * span as i128 / total as i128) as i64;
        let slot = &mut pool[(rng.next() % POOL as u64) as usize];
        if slot.remaining == 0 {
            *slot = Session {
                id: session_id(next_session, ts_ns),
                next_request: 0,
                remaining: 1 + rng.next() % 30,
                user_id: (rng.next() % 100_000) as i64,
            };
            next_session += 1;
        }
        let request_id = slot.next_request;
        slot.next_request += 1;
        slot.remaining -= 1;
        let name = match rng.next() % 100 {
            0..=39 => "page_view",
            40..=54 => "click",
            55..=64 => "search",
            65..=69 => "add_to_cart",
            70..=72 => "checkout",
            r => OTHER_NAMES[(r % 15) as usize],
        };
        Event {
            session_id: slot.id,
            request_id,
            ts_ns,
            name,
            user_id: slot.user_id,
            amount: (name == "checkout").then(|| (rng.next() % 100_000) as i64),
        }
    })
}

fn schema() -> Vec<ColumnSpec> {
    vec![
        ColumnSpec::new("session_id", ColumnType::Int64, false),
        ColumnSpec::new("request_id", ColumnType::Int64, false),
        ColumnSpec::new("ts", ColumnType::TimestampMicros, false),
        ColumnSpec::new("day", ColumnType::Int32, false),
        ColumnSpec::new("name", ColumnType::Utf8, false),
        ColumnSpec::new("user_id", ColumnType::Int64, false),
        ColumnSpec::new("amount", ColumnType::Int64, true),
    ]
}

fn options() -> TableOptions {
    TableOptions {
        schema: Some(schema()),
        layout: Layout::Tiered(TierOptions {
            sort_key: vec!["session_id".into(), "request_id".into()],
            stat_columns: stat_columns(),
            session_key: session_key_secs().map(|max_session_secs| SessionKey {
                column: "session_id".into(),
                max_session_secs,
            }),
            ..TierOptions::default()
        }),
        parquet: WriterOptions {
            compression: codec(),
            ..WriterOptions::default()
        },
        clock,
        ..TableOptions::default()
    }
}

/// The columns outside the sort key that get a per-file range: `STAT_COLUMNS`
/// (comma-separated, empty for none), `day` when unset. A column that is
/// random across hours, like `user_id`, spans every file and prunes nothing.
fn stat_columns() -> Vec<String> {
    let list = std::env::var("STAT_COLUMNS").unwrap_or_else(|_| "day".into());
    list.split(',')
        .filter(|c| !c.is_empty())
        .map(String::from)
        .collect()
}

/// The page codec `CODEC` names; Snappy when unset.
fn codec() -> Compression {
    match std::env::var("CODEC").as_deref() {
        Ok("zstd") => Compression::Zstd,
        Ok("uncompressed") => Compression::Uncompressed,
        Ok("snappy") | Err(_) => Compression::Snappy,
        Ok(other) => panic!("CODEC={other}: expected snappy, zstd or uncompressed"),
    }
}

fn ingest(dir: &Path) {
    std::fs::remove_dir_all(dir).ok();
    let sync_every = env("SYNC_EVERY", 10_000);
    let mut writer = TableWriter::open(dir, options()).expect("open table");
    let (mut rows, mut syncs) = (0_u64, 0_u64);
    let mut flush_s = 0.0;
    let mut hour = BASE_NS / HOUR_NS;
    let start = Instant::now();
    for e in events() {
        NOW.store(e.ts_ns, Ordering::SeqCst);
        if e.ts_ns / HOUR_NS != hour {
            // A periodic flusher: write the hour that just closed.
            hour = e.ts_ns / HOUR_NS;
            let t = Instant::now();
            writer.flush().expect("flush");
            flush_s += t.elapsed().as_secs_f64();
        }
        writer.append_row(row(&e), Ack::Memory).expect("append");
        rows += 1;
        if rows % sync_every == 0 {
            writer.sync().expect("sync");
            syncs += 1;
        }
    }
    writer.sync().expect("sync");
    let ingest_s = start.elapsed().as_secs_f64();
    // Past the last day: every hour and day is closed.
    NOW.fetch_add(DAY_NS, Ordering::SeqCst);
    let t = Instant::now();
    writer.compact().expect("compact");
    let compact_s = t.elapsed().as_secs_f64();
    writer.close().expect("close");
    println!("events={rows}");
    println!("group_commits={}", syncs + 1);
    println!("ingest_s={ingest_s:.3}");
    println!("events_per_s={:.0}", rows as f64 / ingest_s);
    println!("hour_flush_s={flush_s:.3}");
    println!("day_compact_s={compact_s:.3}");
}

fn csv(path: &Path) {
    let mut out = BufWriter::new(File::create(path).expect("create csv"));
    writeln!(out, "session_id,request_id,ts,day,name,user_id,amount").unwrap();
    for e in events() {
        let amount = e.amount.map(|a| a.to_string()).unwrap_or_default();
        writeln!(
            out,
            "{},{},{},{},{},{},{amount}",
            e.session_id,
            e.request_id,
            e.ts_ns / 1_000,
            e.day(),
            e.name,
            e.user_id
        )
        .unwrap();
    }
    out.flush().unwrap();
}

fn cell(c: &Cell) -> String {
    match c {
        Cell::Null => "NULL".into(),
        Cell::Int(i) => i.to_string(),
        Cell::Real(f) => format!("{f}"),
        Cell::Bool(b) => b.to_string(),
        Cell::Text(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

fn query(dir: &Path, sql: &str) {
    let start = Instant::now();
    let snapshot = Snapshot::open(dir).expect("open snapshot");
    let result = query_snapshot(&snapshot, "events", sql).expect("query");
    let elapsed = start.elapsed();
    let opened = query_snapshot_files(&snapshot, "events", sql).expect("count files");
    let mut out = BufWriter::new(std::io::stdout().lock());
    for row in &result.rows {
        let line: Vec<String> = row.iter().map(cell).collect();
        writeln!(out, "{}", line.join(",")).unwrap();
    }
    out.flush().unwrap();
    eprintln!("elapsed_ms={:.1}", elapsed.as_secs_f64() * 1e3);
    eprintln!("files_opened={opened}");
    eprintln!("files_total={}", snapshot.files().count());
}

/// A build side read with and without the filter, as a join over `dir`
/// would: its key columns for the rows `predicate` can match.
fn build(dir: &Path, predicate: &str) {
    let snapshot = Snapshot::open(dir).expect("open snapshot");
    let columns = ["session_id".to_string(), "request_id".to_string()];
    let filter = SnapshotFilter::parse(&snapshot, "events", predicate).expect("predicate");
    let t = Instant::now();
    let all = read_snapshot(&snapshot, &columns).expect("read");
    let all_ms = t.elapsed().as_secs_f64() * 1e3;
    let t = Instant::now();
    let some = read_snapshot_where(&snapshot, &columns, &filter).expect("read where");
    let some_ms = t.elapsed().as_secs_f64() * 1e3;
    let opened = db_core::engine::column::snapshot_files_where(&snapshot, &filter)
        .expect("files")
        .len();
    println!("rows_all={}", all.num_rows);
    println!("rows_filtered={}", some.num_rows);
    println!("files_opened={opened}");
    println!("files_total={}", snapshot.files().count());
    println!("all_ms={all_ms:.1}");
    println!("filtered_ms={some_ms:.1}");
}

/// The events as record rows.
fn row(e: &Event) -> Vec<RecordValue> {
    vec![
        RecordValue::Int(e.session_id),
        RecordValue::Int(e.request_id),
        RecordValue::Int(e.ts_ns / 1_000),
        RecordValue::Int(e.day()),
        RecordValue::Str(e.name.to_string()),
        RecordValue::Int(e.user_id),
        e.amount.map_or(RecordValue::Null, RecordValue::Int),
    ]
}

/// A big unflushed tail read fresh and incrementally (#593), then again
/// after the hour flushed and a second hour's tail is left.
fn tail(dir: &Path) {
    std::fs::remove_dir_all(dir).ok();
    let (tail, more) = (env("TAIL", 1_000_000), env("TAIL_MORE", 1_000));
    let mut writer = TableWriter::open(dir, options()).expect("open table");
    let mut source = events();
    let mut append = |writer: &mut TableWriter, n: u64| {
        for e in source.by_ref().take(n as usize) {
            writer.append_row(row(&e), Ack::Memory).expect("append");
        }
        writer.sync().expect("sync");
    };
    let ms = |t: Instant| t.elapsed().as_secs_f64() * 1e3;
    append(&mut writer, tail);
    let t = Instant::now();
    let fresh = Snapshot::open(dir).expect("open snapshot");
    println!("tail_records={}", fresh.rows());
    println!("fresh_open_ms={:.1}", ms(t));
    let t = Instant::now();
    let mut reader = TableReader::open(dir).expect("open reader");
    println!("reader_open_ms={:.1}", ms(t));
    append(&mut writer, more);
    let t = Instant::now();
    let fresh = Snapshot::open(dir).expect("open snapshot");
    println!("fresh_after_more_ms={:.1}", ms(t));
    let t = Instant::now();
    let held = reader.refresh().expect("refresh");
    println!("refresh_after_more_ms={:.1}", ms(t));
    assert_eq!(held.rows(), fresh.rows());
    // The next hour fills; when the first closes and is flushed, the reader
    // drops those records and keeps the second hour's, which a fresh read
    // would parse again.
    NOW.fetch_add(HOUR_NS, Ordering::SeqCst);
    append(&mut writer, tail);
    reader.refresh().expect("refresh");
    writer.flush().expect("flush");
    append(&mut writer, more);
    let t = Instant::now();
    let fresh = Snapshot::open(dir).expect("open snapshot");
    println!("fresh_after_flush_ms={:.1}", ms(t));
    let t = Instant::now();
    let held = reader.refresh().expect("refresh");
    println!("refresh_after_flush_ms={:.1}", ms(t));
    assert_eq!(held.rows(), fresh.rows());
    println!("records_parsed={}", reader.stats().records_parsed);
    println!("rebuilds={}", reader.stats().rebuilds);
}

/// The id of the n-th session.
fn sid(n: i64) {
    let id = events()
        .map(|e| e.session_id)
        .find(|id| id & 0xFFFF_FFFF == n || (session_key_secs().is_none() && *id == n))
        .expect("session");
    println!("{id}");
}

fn files(dir: &Path) {
    let snapshot = Snapshot::open(dir).expect("open snapshot");
    for entry in &snapshot.manifest().files {
        println!("{}", dir.join(&entry.path).display());
    }
}

fn storage(dir: &Path) {
    let snapshot = Snapshot::open(dir).expect("open snapshot");
    let files = &snapshot.manifest().files;
    let rows: u64 = files.iter().map(|f| f.rows).sum();
    let bytes: u64 = files.iter().map(|f| f.bytes).sum();
    println!("files={}", files.len());
    println!("rows={rows}");
    println!("bytes={bytes}");
    println!("bytes_per_event={:.2}", bytes as f64 / rows.max(1) as f64);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `cargo bench` passes `--bench`; ignore flags.
    let args: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| !a.starts_with("--"))
        .collect();
    match args.as_slice() {
        ["ingest", dir] => ingest(Path::new(dir)),
        ["csv", file] => csv(Path::new(file)),
        ["query", dir, sql] => query(Path::new(dir), sql),
        ["build", dir, predicate] => build(Path::new(dir), predicate),
        ["tail", dir] => tail(Path::new(dir)),
        ["sid", n] => sid(n.parse().expect("session ordinal")),
        ["files", dir] => files(Path::new(dir)),
        ["storage", dir] => storage(Path::new(dir)),
        _ => eprintln!(
            "usage: events ingest <dir> | csv <file> | query <dir> <sql> | build <dir> <predicate> | tail <dir> | sid <n> | files <dir> | storage <dir> \
             (run through tools/bench_events.sh)"
        ),
    }
}
