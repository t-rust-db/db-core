// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `DESC` index key columns (#945): db-core builds, maintains and reads
//! them in the order sqlite3 does. After every statement sqlite3 checks
//! the file db-core wrote (where the CLI is installed) and so does
//! db-core; query results must agree with a copy sqlite3 itself edited.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::string_slice,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "test code fails fast (db-core#230); clippy.toml's allow-*-in-tests does not reach helper fns outside #[test]"
)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use db_core::engine::row::RowEngine;
use db_core::engine::{Cell, Engine};

const FIXTURE: &str = "tests/fixtures/btrees/table_single_page.db";

static NEXT: AtomicU32 = AtomicU32::new(0);

struct TempDb(PathBuf);

impl TempDb {
    fn new(label: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "db-core-idx-desc-{label}-{}-{}.db",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::copy(FIXTURE, &path).expect("copy fixture");
        TempDb(path)
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
        std::fs::remove_file(format!("{}-journal", self.0.display())).ok();
    }
}

fn sqlite3(path: &Path, sql: &str) -> Option<Vec<String>> {
    let out = Command::new("sqlite3").arg(path).arg(sql).output().ok()?;
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

fn rows(e: &mut RowEngine, sql: &str) -> Vec<String> {
    e.run_query(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err}"))
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|c| match c {
                    Cell::Null => String::new(),
                    Cell::Int(i) => i.to_string(),
                    Cell::Text(s) => s.clone(),
                    other => panic!("{other:?}"),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

/// Runs `steps` on db-core and on sqlite3 (each on its own copy of a
/// database built by `setup`), reopening db-core after every statement so
/// sqlite3 can read the file it wrote; `probes` must answer the same on
/// both after each step.
fn run_steps(setup: &str, steps: &[&str], probes: &[&str]) {
    let ours = TempDb::new("ours");
    let theirs = TempDb::new("theirs");
    {
        let mut e = RowEngine::open(&ours.0).unwrap();
        e.run_query("DROP TABLE t").unwrap();
        e.run_query(setup).unwrap();
    }
    sqlite3(&theirs.0, "DROP TABLE t");
    sqlite3(&theirs.0, setup);
    let check = |label: &str| {
        let mut e = RowEngine::open(&ours.0).unwrap();
        assert_eq!(rows(&mut e, "PRAGMA integrity_check"), ["ok"], "{label}");
        if let Some(verdict) = sqlite3(&ours.0, "PRAGMA integrity_check") {
            assert_eq!(verdict, ["ok"], "sqlite3 on db-core's file: {label}");
        }
        for probe in probes {
            if let Some(expected) = sqlite3(&theirs.0, probe) {
                assert_eq!(rows(&mut e, probe), expected, "{probe} {label}");
            }
        }
    };
    check("after setup");
    for step in steps {
        {
            let mut e = RowEngine::open(&ours.0).unwrap();
            e.run_query(step)
                .unwrap_or_else(|err| panic!("{step}: {err}"));
        }
        sqlite3(&theirs.0, step);
        check(step);
    }
}

fn fill(count: u32) -> String {
    (0..count)
        .map(|i| {
            let key = (i * 7919) % count;
            let pad = "x".repeat((i % 13 * 3) as usize);
            format!(
                "INSERT INTO t VALUES ({i}, 'k{key:05}{pad}', {});",
                key % 17
            )
        })
        .collect()
}

const PROBES: &[&str] = &[
    "SELECT count(*), sum(c) FROM t",
    "SELECT a FROM t WHERE a = 'k00042' || substr('xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx', 1, 9)",
    "SELECT id FROM t WHERE a >= 'k00300' AND a < 'k00320' ORDER BY id",
    "SELECT id FROM t WHERE a < 'k00020' ORDER BY id",
    "SELECT id FROM t WHERE c = 3 ORDER BY id",
    "SELECT id FROM t WHERE c BETWEEN 4 AND 6 AND a > 'k00500' ORDER BY id",
    "SELECT id, a FROM t ORDER BY a DESC LIMIT 7",
    "SELECT id, a FROM t ORDER BY a LIMIT 7",
    "SELECT c, id FROM t ORDER BY c DESC, a LIMIT 9",
    "SELECT max(a), min(a) FROM t",
];

#[test]
fn a_desc_index_is_built_and_maintained_like_sqlite3() {
    let setup = format!(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, a TEXT, c INT); \
         CREATE INDEX t_a ON t(a DESC); CREATE INDEX t_ca ON t(c DESC, a); {}",
        fill(1500)
    );
    run_steps(
        &setup,
        &[
            "INSERT INTO t VALUES (5000, 'k00042', 1), (5001, 'zzz', 99), (5002, '', -5)",
            "UPDATE t SET a = 'k9' || a WHERE id % 4 = 0",
            "UPDATE t SET c = c + 1 WHERE id % 3 = 0",
            "DELETE FROM t WHERE id % 5 = 0",
            "DELETE FROM t WHERE a < 'k00500'",
            "DELETE FROM t",
            "INSERT INTO t VALUES (1, 'again', 1)",
        ],
        PROBES,
    );
}

#[test]
fn a_desc_index_built_over_existing_rows_and_a_unique_desc_index() {
    run_steps(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, a TEXT, c INT);",
        &[
            &fill(800),
            "CREATE INDEX t_a ON t(a DESC)",
            "CREATE UNIQUE INDEX t_u ON t(id DESC, c)",
            "INSERT INTO t VALUES (9000, 'late', 4)",
            "DELETE FROM t WHERE c = 4",
        ],
        PROBES,
    );
}

#[test]
fn a_unique_desc_index_still_rejects_duplicates() {
    let ours = TempDb::new("unique");
    let mut e = RowEngine::open(&ours.0).unwrap();
    e.run_query("DROP TABLE t").unwrap();
    e.run_query(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, a TEXT); CREATE UNIQUE INDEX t_a ON t(a DESC); \
         INSERT INTO t VALUES (1, 'x'), (2, 'y')",
    )
    .unwrap();
    assert!(e.run_query("INSERT INTO t VALUES (3, 'x')").is_err());
    assert_eq!(rows(&mut e, "SELECT count(*) FROM t"), ["2"]);
    assert_eq!(rows(&mut e, "PRAGMA integrity_check"), ["ok"]);
}

/// The ordered index walk follows the stored order: `ORDER BY a DESC`
/// reads a `DESC` index forwards, `ORDER BY a` backwards, with no sorter.
#[test]
fn order_by_walks_a_desc_index_without_a_sorter() {
    let ours = TempDb::new("order");
    let mut e = RowEngine::open(&ours.0).unwrap();
    e.run_query("DROP TABLE t").unwrap();
    e.run_query(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, a TEXT, c INT); CREATE INDEX t_a ON t(a DESC); \
         INSERT INTO t VALUES (1, 'b', 0), (2, 'c', 0), (3, 'a', 0), (4, NULL, 0)",
    )
    .unwrap();
    let ops = |e: &RowEngine, sql: &str| -> Vec<String> {
        e.explain_opcodes(sql)
            .unwrap()
            .iter()
            .flat_map(|s| s.rows.iter().map(|r| r.opcode.clone()))
            .collect()
    };
    for (sql, walk) in [
        ("SELECT a FROM t ORDER BY a DESC LIMIT 3", "IdxRewind"),
        ("SELECT a FROM t ORDER BY a LIMIT 3", "IdxLast"),
    ] {
        let o = ops(&e, sql);
        assert!(o.iter().any(|x| x == walk), "{sql}: {o:?}");
        assert!(!o.iter().any(|x| x == "SorterOpen"), "{sql}: {o:?}");
    }
    assert_eq!(
        rows(&mut e, "SELECT a FROM t ORDER BY a DESC LIMIT 3"),
        ["c", "b", "a"]
    );
    assert_eq!(
        rows(&mut e, "SELECT a FROM t ORDER BY a LIMIT 2"),
        ["", "a"]
    );
}
