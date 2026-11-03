// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! INTEGER PRIMARY KEY affinity and a view's database scope (#946), end to end through
//! `engine::row::RowEngine`. Each script also runs through the `sqlite3`
//! CLI when it is installed, and the tables it leaves behind must agree;
//! without the CLI only the expectations written here are checked.
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

struct TempDb(PathBuf);

/// Tells apart temp files made in the same clock tick: `ours` reuses one
/// label, and a coarse clock gave parallel tests the same path.
static NEXT: AtomicU32 = AtomicU32::new(0);

impl TempDb {
    fn new(label: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "db-core-ipk-affinity-{label}-{}-{}-{}.db",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::copy(FIXTURE, &path).expect("copy fixture");
        TempDb(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
        std::fs::remove_file(format!("{}-journal", self.0.display())).ok();
    }
}

fn cell(c: &Cell) -> String {
    match c {
        Cell::Null => String::new(),
        Cell::Int(i) => i.to_string(),
        Cell::Text(s) => s.clone(),
        Cell::Real(r) => format!("{r:?}"),
        other => panic!("{other:?}"),
    }
}

fn engine() -> (TempDb, RowEngine) {
    let db = TempDb::new("e");
    let mut e = RowEngine::open(db.path()).expect("open");
    e.run_query("DROP TABLE t").expect("drop the fixture table");
    e.run_query("CREATE TABLE t(id INTEGER PRIMARY KEY, b)")
        .unwrap();
    (db, e)
}

/// What the `sqlite3` CLI prints for `script`: stdout on success, the
/// stderr on failure; `None` when it is not installed.
fn oracle(script: &str) -> Option<Result<String, String>> {
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg(script)
        .output()
        .ok()?;
    Some(if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).to_string())
    })
}

const SETUP: &str = "CREATE TABLE t(id INTEGER PRIMARY KEY, b); ";

fn rows(e: &mut RowEngine, sql: &str) -> Vec<String> {
    e.run_query(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err}"))
        .rows
        .iter()
        .map(|row| row.iter().map(cell).collect::<Vec<_>>().join("|"))
        .collect()
}

#[test]
fn a_lossless_value_is_stored_as_an_integer() {
    for (write, stored) in [
        ("INSERT INTO t VALUES ('12', 2)", "12|integer"),
        ("INSERT INTO t VALUES (3.0, 2)", "3|integer"),
        ("INSERT INTO t VALUES (' 5 ', 2)", "5|integer"),
        ("INSERT INTO t VALUES ('4.0', 2)", "4|integer"),
        (
            "INSERT INTO t VALUES (1, 2); UPDATE t SET id = '7'",
            "7|integer",
        ),
        (
            "INSERT INTO t VALUES (1, 2); UPDATE t SET id = 7.0",
            "7|integer",
        ),
    ] {
        let (_db, mut e) = engine();
        e.run_query(write)
            .unwrap_or_else(|err| panic!("{write}: {err}"));
        assert_eq!(
            rows(&mut e, "SELECT id, typeof(id) FROM t"),
            [stored],
            "{write}"
        );
        if let Some(Ok(out)) = oracle(&format!(
            "{SETUP}{write}; SELECT id || '|' || typeof(id) FROM t"
        )) {
            assert_eq!(out.trim_end(), stored, "sqlite3: {write}");
        }
    }
}

#[test]
fn a_lossy_value_is_a_datatype_mismatch() {
    for write in [
        "INSERT INTO t VALUES ('x', 2)",
        "INSERT INTO t VALUES (1.5, 2)",
        "INSERT INTO t VALUES (x'31', 2)",
        "INSERT INTO t VALUES (1, 2); UPDATE t SET id = 'x'",
        "INSERT INTO t VALUES (1, 2); UPDATE t SET id = 1.5",
    ] {
        let (_db, mut e) = engine();
        let err = e.run_query(write).expect_err(write);
        assert!(err.message.contains("datatype mismatch"), "{write}: {err}");
        if let Some(theirs) = oracle(&format!("{SETUP}{write}")) {
            let theirs = theirs.expect_err(write);
            assert!(theirs.contains("datatype mismatch"), "{theirs}");
        }
    }
}

#[test]
fn a_view_may_only_read_the_main_database() {
    for (create, ok) in [
        ("CREATE VIEW v AS SELECT * FROM r.t", false),
        ("CREATE VIEW v AS SELECT * FROM temp.t", false),
        (
            "CREATE VIEW v AS SELECT * FROM t WHERE id IN (SELECT id FROM r.t)",
            false,
        ),
        ("CREATE VIEW v AS SELECT * FROM main.t", true),
        ("CREATE VIEW v AS SELECT * FROM t", true),
    ] {
        let (_db, mut e) = engine();
        let ours = e.run_query(create);
        if let Some(theirs) = oracle(&format!("{SETUP}{create}")) {
            assert_eq!(theirs.is_ok(), ok, "sqlite3: {create}");
        }
        if ok {
            ours.unwrap_or_else(|err| panic!("{create}: {err}"));
        } else {
            let err = ours.expect_err(create);
            assert!(
                err.message
                    .contains("view v cannot reference objects in database"),
                "{create}: {err}"
            );
        }
    }
}

#[test]
fn blob_operands_are_read_as_text_then_as_a_number() {
    for sql in [
        "SELECT x'35' * 1, x'35' - 0, -x'35', x'3132' / 2, abs(x'2D35')",
        "SELECT 10 * x'35', 10 + x'35', 10 / CAST('5' AS BLOB), x'35' % 3",
        "SELECT 60529 / CAST(-693 AS BLOB), x'ff' + 1, x'' + 1, x'31322e35' + 1",
        "SELECT CASE WHEN CAST(-693 AS BLOB) / 7 THEN 1 ELSE 0 END",
    ] {
        let (_db, mut e) = engine();
        let got = rows(&mut e, sql);
        if let Some(Ok(out)) = oracle(sql) {
            let want: Vec<&str> = out.lines().collect();
            assert_eq!(got, want, "{sql}");
        }
    }
}
