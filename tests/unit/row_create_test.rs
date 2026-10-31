// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `RowEngine::create` (#915): a new database file that stock `sqlite3`
//! accepts, in WAL and rollback-journal modes, and that never touches an
//! existing file.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test code fails fast (db-core#230)"
)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use db_core::engine::row::{CreateJournal, CreateOptions, RowEngine};
use db_core::engine::{Cell, Engine, ErrorKind};

static NEXT: AtomicU32 = AtomicU32::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "db-core-create-{}-{}",
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

/// Runs stock `sqlite3`; `None` when it is not installed.
fn sqlite3(db: &Path, sql: &str) -> Option<String> {
    let out = Command::new("sqlite3").arg(db).arg(sql).output().ok()?;
    assert!(
        out.status.success(),
        "sqlite3 {sql}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

const SCHEMA: &str = "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT); CREATE INDEX t_v ON t(v)";

fn options(journal: CreateJournal, page_size: u32, reserved_bytes: u8) -> CreateOptions {
    CreateOptions {
        page_size,
        journal,
        reserved_bytes,
    }
}

#[test]
fn created_file_passes_sqlite3_integrity_check_in_both_journal_modes() {
    for (journal, mode) in [
        (CreateJournal::Wal, "wal"),
        (CreateJournal::Delete, "delete"),
    ] {
        for (page_size, reserved) in [(4096, 0), (1024, 0), (65536, 0), (4096, 12)] {
            let dir = TempDir::new();
            let mut e = RowEngine::create(&dir.db(), options(journal, page_size, reserved), SCHEMA)
                .unwrap();
            e.run_query("INSERT INTO t(v) VALUES ('a'), ('b')").unwrap();
            drop(e);
            let Some(check) = sqlite3(&dir.db(), "PRAGMA integrity_check") else {
                return;
            };
            assert_eq!(check, "ok", "{mode} page {page_size} reserved {reserved}");
            assert_eq!(sqlite3(&dir.db(), "PRAGMA journal_mode").unwrap(), mode);
            assert_eq!(
                sqlite3(&dir.db(), "PRAGMA page_size").unwrap(),
                page_size.to_string()
            );
            assert_eq!(sqlite3(&dir.db(), "SELECT count(*) FROM t").unwrap(), "2");
            // And it opens again through the engine.
            let mut again = RowEngine::open(&dir.db()).unwrap();
            let rows = again.run_query("SELECT count(*) FROM t").unwrap().rows;
            assert_eq!(rows, vec![vec![Cell::Int(2)]]);
        }
    }
}

#[test]
fn create_with_an_empty_schema_makes_an_empty_database() {
    let dir = TempDir::new();
    let e = RowEngine::create(&dir.db(), CreateOptions::default(), "").unwrap();
    assert!(e.tables().unwrap().is_empty());
    drop(e);
    if let Some(check) = sqlite3(&dir.db(), "PRAGMA integrity_check") {
        assert_eq!(check, "ok");
    }
}

#[test]
fn create_on_an_existing_file_fails_and_leaves_it_untouched() {
    let dir = TempDir::new();
    std::fs::write(dir.db(), b"not a database").unwrap();
    let err = RowEngine::create(&dir.db(), CreateOptions::default(), SCHEMA).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Open);
    assert_eq!(std::fs::read(dir.db()).unwrap(), b"not a database");

    let made = TempDir::new();
    drop(RowEngine::create(&made.db(), CreateOptions::default(), SCHEMA).unwrap());
    let before = std::fs::read(made.db()).unwrap();
    let err = RowEngine::create(&made.db(), CreateOptions::default(), "DROP TABLE t").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Open);
    assert_eq!(std::fs::read(made.db()).unwrap(), before);
}

#[test]
fn create_rejects_an_invalid_layout_without_creating_a_file() {
    for (page_size, reserved) in [(1000, 0), (256, 0), (131_072, 0), (4096, 0)] {
        let dir = TempDir::new();
        let result = RowEngine::create(
            &dir.db(),
            options(CreateJournal::Delete, page_size, reserved),
            "",
        );
        assert_eq!(result.is_err(), page_size != 4096, "page size {page_size}");
        assert_eq!(dir.db().exists(), page_size == 4096);
    }
    let dir = TempDir::new();
    let err = RowEngine::create(&dir.db(), options(CreateJournal::Delete, 512, 255), "");
    assert!(err.is_ok());
}

#[test]
fn create_removes_the_new_file_when_the_schema_fails() {
    let dir = TempDir::new();
    let err = RowEngine::create(&dir.db(), CreateOptions::default(), "CREATE TABLE").unwrap_err();
    assert_ne!(err.kind, ErrorKind::Open);
    assert!(!dir.db().exists());
}
