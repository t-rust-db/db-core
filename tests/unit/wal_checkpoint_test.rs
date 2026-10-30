// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Automatic WAL checkpoint, `PRAGMA wal_autocheckpoint` and `PRAGMA
//! wal_checkpoint(mode)` (#896). The mode results are compared with the
//! `sqlite3` CLI on a copy of the same files when it is installed.
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

/// Bytes per WAL frame at the default 4096-byte page size.
const FRAME: u64 = 24 + 4096;
const WAL_HEADER: u64 = 32;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "db-core-wal-ckpt-{}-{}",
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
    fn wal_frames(&self) -> u64 {
        let len = std::fs::metadata(self.wal()).map_or(0, |m| m.len());
        len.saturating_sub(WAL_HEADER) / FRAME
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

/// An empty WAL-mode database with one table, opened through `RowEngine`.
/// The file is created by the `sqlite3` CLI (`RowEngine` opens existing
/// databases only), so each test is a no-op where it is not installed.
fn open() -> Option<(TempDir, RowEngine)> {
    let dir = TempDir::new();
    sqlite3(
        &dir.db(),
        "PRAGMA journal_mode=WAL; CREATE TABLE t(a INTEGER);",
    )?;
    let e = RowEngine::open(&dir.db()).unwrap();
    Some((dir, e))
}

fn insert(e: &mut RowEngine, from: i64, to: i64) {
    for i in from..to {
        e.run_query(&format!("INSERT INTO t VALUES ({i})")).unwrap();
    }
}

fn ints(e: &mut RowEngine, sql: &str) -> Vec<i64> {
    e.run_query(sql).unwrap().rows[0]
        .iter()
        .map(|c| match c {
            Cell::Int(i) => *i,
            other => panic!("{other:?}"),
        })
        .collect()
}

fn count(e: &mut RowEngine) -> i64 {
    ints(e, "SELECT count(*) FROM t")[0]
}

#[test]
fn autocheckpoint_keeps_the_wal_bounded() {
    let Some((dir, mut e)) = open() else { return };
    insert(&mut e, 0, 1500);
    // Default threshold 1000 frames: the log is restarted, not appended to.
    assert!(
        dir.wal_frames() < 1100,
        "wal has {} frames",
        dir.wal_frames()
    );
    assert_eq!(count(&mut e), 1500);
    drop(e);
    if let Some(theirs) = sqlite3(&dir.db(), "SELECT count(*) FROM t") {
        assert_eq!(theirs, ["1500"]);
    }
}

#[test]
fn autocheckpoint_zero_disables_it() {
    let Some((dir, mut e)) = open() else { return };
    assert_eq!(ints(&mut e, "PRAGMA wal_autocheckpoint = 0"), [0]);
    insert(&mut e, 0, 1200);
    assert!(
        dir.wal_frames() > 1200,
        "wal has {} frames",
        dir.wal_frames()
    );
}

#[test]
fn autocheckpoint_pragma_reads_and_sets_the_threshold() {
    let Some((_dir, mut e)) = open() else { return };
    assert_eq!(ints(&mut e, "PRAGMA wal_autocheckpoint"), [1000]);
    assert_eq!(ints(&mut e, "PRAGMA wal_autocheckpoint = 20"), [20]);
    assert_eq!(ints(&mut e, "PRAGMA wal_autocheckpoint(30)"), [30]);
    assert_eq!(ints(&mut e, "PRAGMA wal_autocheckpoint = -1"), [-1]);
    assert_eq!(ints(&mut e, "PRAGMA wal_autocheckpoint"), [-1]);
}

#[test]
fn small_threshold_checkpoints_often() {
    let Some((dir, mut e)) = open() else { return };
    e.run_query("PRAGMA wal_autocheckpoint = 20").unwrap();
    insert(&mut e, 0, 200);
    assert!(dir.wal_frames() < 40, "wal has {} frames", dir.wal_frames());
    assert_eq!(count(&mut e), 200);
}

/// A `sqlite3` process holding a read transaction on `dir`'s database (POSIX
/// record locks never conflict inside one process, so a second `RowEngine`
/// would not count as a pinned reader).
fn pin_reader(dir: &TempDir) -> std::process::Child {
    use std::io::Write;
    use std::process::Stdio;

    let mut reader = Command::new("sqlite3")
        .arg(dir.db())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let stdin = reader.stdin.as_mut().unwrap();
    stdin
        .write_all(b"BEGIN; SELECT count(*) FROM t;\n")
        .unwrap();
    stdin.flush().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    reader
}

/// A reader that is pinned to the WAL stops the restart but never fails the
/// writer's commit, and nothing is lost.
#[test]
fn autocheckpoint_with_a_pinned_reader_neither_blocks_nor_fails() {
    let Some((dir, mut writer)) = open() else {
        return;
    };
    writer.run_query("PRAGMA wal_autocheckpoint = 20").unwrap();
    insert(&mut writer, 0, 10);
    let mut reader = pin_reader(&dir);
    insert(&mut writer, 10, 300);
    assert_eq!(count(&mut writer), 300);
    reader.kill().ok();
    reader.wait().ok();
    // The pinned reader stopped the restart: the log kept growing.
    assert!(
        dir.wal_frames() > 100,
        "wal has {} frames",
        dir.wal_frames()
    );
    drop(writer);
    if let Some(theirs) = sqlite3(&dir.db(), "SELECT count(*) FROM t") {
        assert_eq!(theirs, ["300"]);
    }
}

#[test]
fn not_wal_mode_reports_minus_one() {
    let dir = TempDir::new();
    if sqlite3(&dir.db(), "CREATE TABLE t(a);").is_none() {
        return;
    }
    let mut e = RowEngine::open(&dir.db()).unwrap();
    assert_eq!(ints(&mut e, "PRAGMA wal_checkpoint"), [0, -1, -1]);
}

/// Each mode's `(busy, log, checkpointed)` against stock `sqlite3`, run on a
/// snapshot of the same `-wal`.
#[test]
fn wal_checkpoint_modes_match_sqlite3() {
    for mode in ["", "(PASSIVE)", "(FULL)", "(RESTART)", "(TRUNCATE)"] {
        let Some((dir, mut e)) = open() else { return };
        e.run_query("PRAGMA wal_autocheckpoint = 0").unwrap();
        insert(&mut e, 0, 25);
        let copy = TempDir::new();
        std::fs::copy(dir.db(), copy.db()).unwrap();
        std::fs::copy(dir.wal(), copy.wal()).unwrap();

        let ours = ints(&mut e, &format!("PRAGMA wal_checkpoint{mode}"));
        if let Some(theirs) = sqlite3(&copy.db(), &format!("PRAGMA wal_checkpoint{mode}")) {
            let theirs: Vec<i64> = theirs[0].split('|').map(|n| n.parse().unwrap()).collect();
            assert_eq!(ours, theirs, "mode {mode:?}");
        }
        assert_eq!(count(&mut e), 25, "mode {mode:?}");
    }
}

#[test]
fn restart_checkpoint_is_followed_by_a_rewound_wal() {
    let Some((dir, mut e)) = open() else { return };
    e.run_query("PRAGMA wal_autocheckpoint = 0").unwrap();
    insert(&mut e, 0, 50);
    let before = std::fs::read(dir.wal()).unwrap();
    assert!(dir.wal_frames() >= 50);
    let row = ints(&mut e, "PRAGMA wal_checkpoint(RESTART)");
    assert_eq!(row[0], 0);
    insert(&mut e, 50, 51);
    // The next commit rewinds the log: a couple of frames, new salts.
    assert!(dir.wal_frames() <= 3, "wal has {} frames", dir.wal_frames());
    let after = std::fs::read(dir.wal()).unwrap();
    assert_ne!(before[16..24], after[16..24], "salts must change");
    assert_eq!(count(&mut e), 51);
    drop(e);
    if let Some(theirs) = sqlite3(&dir.db(), "SELECT count(*) FROM t") {
        assert_eq!(theirs, ["51"]);
    }
}

#[test]
fn truncate_checkpoint_empties_the_wal_and_writes_resume() {
    let Some((dir, mut e)) = open() else { return };
    e.run_query("PRAGMA wal_autocheckpoint = 0").unwrap();
    insert(&mut e, 0, 30);
    assert_eq!(ints(&mut e, "PRAGMA wal_checkpoint(TRUNCATE)"), [0, 0, 0]);
    assert_eq!(std::fs::metadata(dir.wal()).unwrap().len(), 0);
    insert(&mut e, 30, 40);
    assert_eq!(count(&mut e), 40);
    drop(e);
    if let Some(theirs) = sqlite3(&dir.db(), "SELECT count(*) FROM t") {
        assert_eq!(theirs, ["40"]);
    }
}

/// A pinned reader keeps RESTART from finishing: it reports busy and still checkpoints what it can.
#[test]
fn restart_checkpoint_with_a_pinned_reader_reports_busy() {
    let Some((dir, mut writer)) = open() else {
        return;
    };
    writer.run_query("PRAGMA wal_autocheckpoint = 0").unwrap();
    insert(&mut writer, 0, 10);
    let mut reader = pin_reader(&dir);
    insert(&mut writer, 10, 20);
    let row = ints(&mut writer, "PRAGMA wal_checkpoint(RESTART)");
    reader.kill().ok();
    reader.wait().ok();
    assert_eq!(row[0], 1, "reader pinned: {row:?}");
    assert_eq!(count(&mut writer), 20);
}
