// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0

//! A reader in another process (#572): while `tests/helpers/table_probe.rs`
//! appends, seals and flushes, this process reads the table over and over
//! and checks every read
//!
//! - reaches at least the last record the writer had acknowledged as
//!   durable when the read started;
//! - holds LSNs `1..=last` exactly once, files and pages together, each
//!   record the one the writer appended under that LSN;
//! - answers SQL the same way: `COUNT(*)`, `MIN`, `MAX` and `SUM` of `i`
//!   say each record is there once.
//!
//! A long-lived `TableReader` (#593) is refreshed alongside the fresh
//! opens and held to the same checks, and has to have read incrementally.
//!
//! The writer is then killed, and a last read sees every acknowledged
//! record. `TABLE_READER_READS` overrides the default of 150 reads (more
//! if none has seen both files and pages yet).

#![cfg(all(feature = "ingest", feature = "engine-column"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "test code fails fast (db-core#230)"
)]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use db_core::engine::column::table::TableEngine;
use db_core::engine::{Cell, Engine};
use db_core::storage::ingest::{Snapshot, TableReader};
use db_core::storage::ParquetFile;

const PROBE: &str = env!("CARGO_BIN_EXE_table_probe");

fn reads() -> u32 {
    std::env::var("TABLE_READER_READS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(150)
}

fn line(lsn: u64) -> String {
    let n = lsn - 1;
    format!(r#"{{"i":{n},"level":"info","msg":"record {n}"}}"#)
}

/// Checks `s` holds exactly LSNs `1..=s.last_lsn()`, each the writer's
/// record. Returns whether it had both files and pages.
fn check(s: &Snapshot) -> bool {
    let mut next = 1u64;
    for (entry, bytes) in s.files() {
        assert_eq!(entry.lsn_lo, next, "{s:?}");
        let file = ParquetFile::open(bytes).unwrap();
        let raw = file.metadata().schema[1..]
            .iter()
            .position(|e| e.name == "raw")
            .unwrap();
        for rg in file.row_groups() {
            for value in rg.read_string_column(raw).unwrap() {
                assert_eq!(value.unwrap(), line(next), "{s:?}");
                next += 1;
            }
        }
        assert_eq!(entry.lsn_hi + 1, next, "{s:?}");
    }
    for page in s.pages() {
        assert_eq!(page.lsn_lo, next, "{s:?}");
        for i in 0..page.lines().unwrap().len() {
            assert_eq!(
                page.lines().unwrap().raw_line(i),
                line(next).as_bytes(),
                "{s:?}"
            );
            next += 1;
        }
        assert_eq!(page.lsn_hi + 1, next, "{s:?}");
    }
    assert_eq!(next - 1, s.last_lsn());
    !s.pages().is_empty() && s.manifest().flushed_lsn > 0
}

/// `COUNT(*)`, `MIN(i)`, `MAX(i)`, `SUM(i)` for records `0..n`; returns n.
fn check_sql(engine: &mut TableEngine) -> u64 {
    let result = engine
        .run_query("SELECT COUNT(*), MIN(i), MAX(i), SUM(i) FROM logs")
        .unwrap();
    let Cell::Int(n) = result.rows[0][0] else {
        panic!("{result:?}")
    };
    let n = u64::try_from(n).unwrap();
    if n > 0 {
        let sum = i64::try_from(n * (n - 1) / 2).unwrap();
        assert_eq!(
            result.rows[0][1..],
            [
                Cell::Int(0),
                Cell::Int(i64::try_from(n - 1).unwrap()),
                Cell::Int(sum)
            ],
            "{n} records"
        );
    }
    n
}

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// Until the first record is acknowledged: before it, the table has no
/// `i` column to query.
fn wait_for_table(table: &Path, acked: &AtomicU64) {
    let start = Instant::now();
    while !table.join("manifest.json").exists() || acked.load(Ordering::SeqCst) == 0 {
        assert!(start.elapsed() < Duration::from_secs(30), "no table");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_reader_in_another_process_sees_every_durable_record_once() {
    let root =
        Dir(std::env::temp_dir().join(format!("db-core-table-reader-{}", std::process::id())));
    std::fs::remove_dir_all(&root.0).ok();
    let table = root.0.join("logs");
    let mut child = Command::new(PROBE)
        .arg(&table)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let acked = Arc::new(AtomicU64::new(0));
    let stdout = child.stdout.take().unwrap();
    let counter = Arc::clone(&acked);
    let collector = std::thread::spawn(move || {
        for l in BufReader::new(stdout).lines() {
            let Ok(Ok(lsn)) = l.map(|l| l.parse::<u64>()) else {
                break; // the kill can cut the last line
            };
            counter.fetch_max(lsn, Ordering::SeqCst);
        }
    });

    wait_for_table(&table, &acked);
    let mut engine = TableEngine::open(&table).unwrap();
    let mut reader = TableReader::open(&table).unwrap();
    // At least `reads()` reads, and until one has seen both flushed files
    // and pages: a loaded machine slows the writer down.
    let mut mixed = 0;
    let start = Instant::now();
    let mut read = 0;
    while read < reads() || (mixed == 0 && start.elapsed() < Duration::from_secs(60)) {
        read += 1;
        let floor = acked.load(Ordering::SeqCst);
        let s = Snapshot::open(&table).unwrap();
        assert!(
            s.last_lsn() >= floor,
            "read {read}: {s:?} below ack {floor}"
        );
        if check(&s) {
            mixed += 1;
        }
        let floor = acked.load(Ordering::SeqCst);
        let held = reader.refresh().unwrap();
        assert!(
            held.last_lsn() >= floor,
            "read {read}: reader {held:?} below ack {floor}"
        );
        check(held);
        let floor = acked.load(Ordering::SeqCst);
        assert!(check_sql(&mut engine) >= floor, "read {read}");
    }

    child.kill().unwrap();
    child.wait().unwrap();
    collector.join().unwrap();
    let s = Snapshot::open(&table).unwrap();
    check(&s);
    assert!(s.last_lsn() >= acked.load(Ordering::SeqCst));
    assert_eq!(check_sql(&mut engine), s.last_lsn());
    let held = reader.refresh().unwrap();
    check(held);
    assert!(held.last_lsn() >= acked.load(Ordering::SeqCst));
    // The probe flushes every 150 records, so some refreshes rebuild, but
    // most of the many refreshes must have read only new frames.
    let stats = reader.stats();
    assert!(
        stats.rebuilds < stats.refreshes,
        "every refresh read the whole tail: {stats:?}"
    );
    assert!(
        mixed > 0,
        "no read saw both flushed files and pages; the writer was too slow"
    );
}
