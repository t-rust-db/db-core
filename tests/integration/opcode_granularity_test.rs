// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Side-by-side `EXPLAIN` of the same query shape -- scan, filter, group,
//! count -- in all three execution modes (db-studio#54: why the opcode
//! granularity differs so much between a log file and a SQLite file).
//!
//! Row (`vm::row`) is a cursor-stepping register machine: one instruction
//! per value moved, one loop iteration per row. Batch (`vm::batch`) is a
//! vectorized dataflow: one instruction per *column operation* over a whole
//! segment. Stream (`vm::stream`) reuses the batch opcodes over ring
//! segments, so a log scan reads at the same altitude as a Parquet scan.
//!
//! Run with `--nocapture` to see the three listings.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    reason = "test code fails fast (db-core#230); clippy.toml's allow-*-in-tests does not reach helper fns outside #[test]"
)]

use std::path::{Path, PathBuf};

use db_core::engine::column::BatchEngine;
use db_core::engine::row::RowEngine;
use db_core::engine::stream::StreamEngine;
use db_core::engine::{Engine, OpcodeSection};

const ROW_FIXTURE: &str = "tests/fixtures/btrees/table_single_page.db";
const COLUMN_FIXTURE: &str = "tests/fixtures/parquet/production.parquet";
const STREAM_FIXTURE: &str = "tests/fixtures/stream/syslog-1k.log";

/// A writable copy of the row fixture, removed on drop.
struct TempDb(PathBuf);

impl TempDb {
    fn new() -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "db-core-opcode-granularity-{}.db",
            std::process::id()
        ));
        std::fs::copy(ROW_FIXTURE, &path).expect("copy fixture");
        TempDb(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
    }
}

/// Prints one listing and returns its total instruction count.
fn show(mode: &str, sql: &str, sections: &[OpcodeSection]) -> usize {
    println!("\n=== {mode}: {sql}");
    let mut n = 0;
    for s in sections {
        println!("-- section {} (lane {})", s.label, s.lane);
        for r in &s.rows {
            println!("{:>4}  {:<16} {}", r.addr, r.opcode, r.operands);
            n += 1;
        }
    }
    println!("-- {n} instructions in {} section(s)", sections.len());
    n
}

#[test]
fn scan_a_sqlite_row_table() {
    let db = TempDb::new();
    let engine = RowEngine::open(db.path()).expect("open sqlite fixture");
    let sql = "SELECT b, count(*) FROM t WHERE a > 2 GROUP BY b";
    let sections = engine.explain_opcodes(sql).expect("explain");
    let n = show("row / SQLite", sql, &sections);
    for row in engine.explain_plan(sql).expect("plan") {
        println!("plan {} {} {}", row.id, row.parent, row.detail);
    }
    assert!(n > 10, "row programs are per-value: got only {n}");
    assert!(sections.iter().all(|s| s.lane == "row"));
}

#[test]
fn scan_a_parquet_column_file() {
    let engine = BatchEngine::open(Path::new(COLUMN_FIXTURE)).expect("open parquet fixture");
    let sql = "SELECT region, count(*) FROM production WHERE id > 4990 GROUP BY region";
    let sections = engine.explain_opcodes(sql).expect("explain");
    let n = show("batch / Parquet", sql, &sections);
    for row in engine.explain_plan(sql).expect("plan") {
        println!("plan {} {} {}", row.id, row.parent, row.detail);
    }
    assert!(n < 20, "batch programs are per-column-op: got {n}");
    assert!(sections.iter().all(|s| s.lane == "batch"));
}

#[test]
fn scan_a_log_stream_file() {
    let engine = StreamEngine::open(Path::new(STREAM_FIXTURE)).expect("open log fixture");
    let sql = "SELECT facility, count(*) FROM log WHERE severity >= 'WARN' GROUP BY facility";
    let sections = engine.explain_opcodes(sql).expect("explain");
    let n = show("stream / log", sql, &sections);
    for row in engine.explain_plan(sql).expect("plan") {
        println!("plan {} {} {}", row.id, row.parent, row.detail);
    }
    assert!(n < 20, "stream reuses the batch opcodes: got {n}");
}
