// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Files that reserve bytes at the end of every page (header byte 20, as
//! macOS's system `sqlite3` does) must keep those bytes free (#904): stock
//! SQLite's `PRAGMA integrity_check` is the oracle, so each test is a
//! no-op where the `sqlite3` CLI (or its `reserve_bytes` file control) is
//! not available.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "test code fails fast (db-core#230); clippy.toml's allow-*-in-tests does not reach helper fns outside #[test]"
)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use db_core::engine::row::RowEngine;
use db_core::engine::{Cell, Engine};

static NEXT: AtomicU32 = AtomicU32::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "db-core-reserved-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
    fn db(&self) -> PathBuf {
        self.0.join("t.db")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn sqlite3(db: &Path, args: &[&str]) -> Option<Vec<String>> {
    let out = Command::new("sqlite3").arg(db).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
    )
}

fn reserved_byte(db: &Path) -> u8 {
    std::fs::read(db).unwrap()[20]
}

/// A rollback-journal database whose pages reserve `reserve` bytes, made by
/// the `sqlite3` CLI; `None` when the CLI cannot produce that reserve.
fn create(reserve: u8) -> Option<TempDir> {
    let dir = TempDir::new();
    let db = dir.db();
    sqlite3(
        &db,
        &["CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b TEXT);"],
    )?;
    sqlite3(
        &db,
        &[&format!(".filectrl reserve_bytes {reserve}"), "VACUUM;"],
    )?;
    sqlite3(
        &db,
        &["CREATE INDEX t_a ON t (a); CREATE INDEX t_b ON t (b);"],
    )?;
    (reserved_byte(&db) == reserve).then_some(dir)
}

fn run(reserve: u8) {
    let Some(dir) = create(reserve) else { return };
    let mut e = RowEngine::open(&dir.db()).unwrap();
    // Enough single-row commits to split leaf and interior pages of the
    // table and both indexes, with a few overflow-sized rows mixed in.
    for i in 0..1500_usize {
        let b = if i % 97 == 0 {
            "x".repeat(6000)
        } else {
            format!("b{:05}", i * 7919 % 10007)
        };
        e.run_query(&format!(
            "INSERT INTO t (id, a, b) VALUES ({i}, 'row-{:05}-{}', '{b}')",
            i * 31 % 1499,
            "pad".repeat(i % 13)
        ))
        .unwrap();
    }
    e.run_query("UPDATE t SET a = a || 'u' WHERE id % 5 = 0")
        .unwrap();
    e.run_query("DELETE FROM t WHERE id % 3 = 0").unwrap();
    drop(e);

    assert_eq!(reserved_byte(&dir.db()), reserve, "reserve byte changed");
    assert_eq!(
        sqlite3(&dir.db(), &["PRAGMA integrity_check"]).unwrap(),
        ["ok"]
    );
    assert_eq!(
        sqlite3(&dir.db(), &["SELECT count(*) FROM t"]).unwrap(),
        ["1000"]
    );

    let mut e = RowEngine::open(&dir.db()).unwrap();
    let rows = e.run_query("SELECT count(*) FROM t").unwrap().rows;
    assert_eq!(rows[0][0], Cell::Int(1000));
}

#[test]
fn reserve_zero_stays_valid() {
    run(0);
}

#[test]
fn reserve_twelve_keeps_the_reserved_tail_free() {
    run(12);
}

#[test]
fn reserve_survives_wal_checkpoint() {
    let Some(dir) = create(12) else { return };
    sqlite3(&dir.db(), &["PRAGMA journal_mode=WAL"]).unwrap();
    let mut e = RowEngine::open(&dir.db()).unwrap();
    for i in 0..600 {
        e.run_query(&format!(
            "INSERT INTO t (id, a, b) VALUES ({i}, 'a{i:05}', 'b{i:05}')"
        ))
        .unwrap();
    }
    e.run_query("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    drop(e);
    assert_eq!(reserved_byte(&dir.db()), 12);
    assert_eq!(
        sqlite3(&dir.db(), &["PRAGMA integrity_check"]).unwrap(),
        ["ok"]
    );
}
