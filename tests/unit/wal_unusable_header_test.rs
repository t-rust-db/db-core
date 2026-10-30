// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! A `-wal` with an unusable header is treated as empty (#895), as stock
//! SQLite's `walIndexRecover` does: reads return the database file's
//! contents and the first write restarts the WAL. Each case is run through
//! the `sqlite3` CLI too when it is installed.
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
            "db-core-wal-header-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
    fn db(&self) -> PathBuf {
        self.0.join("t.db")
    }
    fn wal(&self) -> PathBuf {
        self.0.join("t.db-wal")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn sqlite3(db: &Path, sql: &str) -> Option<Vec<String>> {
    let out = Command::new("sqlite3").arg(db).arg(sql).output().ok()?;
    assert!(
        out.status.success(),
        "sqlite3 failed on {sql}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
    )
}

/// A WAL-mode database with rows 1..=3 checkpointed into the main file and
/// no `-wal` left behind; `None` when the `sqlite3` CLI is not installed.
fn build() -> Option<TempDir> {
    let dir = TempDir::new();
    sqlite3(
        &dir.db(),
        "PRAGMA journal_mode=WAL; CREATE TABLE t(a); \
         INSERT INTO t VALUES (1),(2),(3); PRAGMA wal_checkpoint(TRUNCATE);",
    )?;
    std::fs::remove_file(dir.wal()).ok();
    std::fs::remove_file(dir.0.join("t.db-shm")).ok();
    Some(dir)
}

fn rows(e: &mut RowEngine) -> Vec<String> {
    e.run_query("SELECT a FROM t ORDER BY a")
        .unwrap()
        .rows
        .iter()
        .map(|r| match &r[0] {
            Cell::Int(i) => i.to_string(),
            other => panic!("{other:?}"),
        })
        .collect()
}

/// Damaged `-wal` contents, each an unusable header (a real WAL header is
/// 32 bytes: magic, version, page size, seq, salts, checksum).
fn damaged_wals() -> Vec<(&'static str, Vec<u8>)> {
    let mut wrong_magic = vec![0u8; 64];
    wrong_magic[..4].copy_from_slice(&0xdead_beefu32.to_be_bytes());
    let mut valid_magic_bad_rest = vec![0u8; 64];
    valid_magic_bad_rest[..4].copy_from_slice(&0x377f_0682u32.to_be_bytes());
    valid_magic_bad_rest[4..8].copy_from_slice(&3_007_000u32.to_be_bytes());
    valid_magic_bad_rest[8..12].copy_from_slice(&4096u32.to_be_bytes());
    let mut bad_page_size = valid_magic_bad_rest.clone();
    bad_page_size[8..12].copy_from_slice(&600u32.to_be_bytes());
    vec![
        ("zero bytes", vec![]),
        ("short", vec![0u8; 10]),
        ("zeroed header", vec![0u8; 64]),
        ("wrong magic", wrong_magic),
        ("bad page size", bad_page_size),
        ("bad checksum", valid_magic_bad_rest),
    ]
}

#[test]
fn unusable_wal_header_reads_as_the_database_file() {
    if build().is_none() {
        return;
    }
    for (label, bytes) in damaged_wals() {
        let dir = build().unwrap();
        std::fs::write(dir.wal(), &bytes).unwrap();
        let mut e = RowEngine::open(&dir.db()).unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(rows(&mut e), ["1", "2", "3"], "db-core: {label}");
        drop(e);
        // sqlite3 recovers the same, on its own copy of the damaged files.
        let theirs = sqlite3(&dir.db(), "SELECT a FROM t ORDER BY a").unwrap();
        assert_eq!(theirs, ["1", "2", "3"], "sqlite3: {label}");
    }
}

#[test]
fn unusable_wal_header_restarts_on_first_write() {
    if build().is_none() {
        return;
    }
    for (label, bytes) in damaged_wals() {
        let dir = build().unwrap();
        std::fs::write(dir.wal(), &bytes).unwrap();
        let mut e = RowEngine::open(&dir.db()).unwrap_or_else(|e| panic!("{label}: {e}"));
        e.run_query("INSERT INTO t VALUES (4)")
            .unwrap_or_else(|err| panic!("{label}: {err}"));
        assert_eq!(rows(&mut e), ["1", "2", "3", "4"], "db-core: {label}");
        drop(e);
        // The restarted WAL has a valid header: stock sqlite3 reads it.
        let theirs = sqlite3(&dir.db(), "SELECT a FROM t ORDER BY a").unwrap();
        assert_eq!(theirs, ["1", "2", "3", "4"], "sqlite3 after write: {label}");
    }
}
