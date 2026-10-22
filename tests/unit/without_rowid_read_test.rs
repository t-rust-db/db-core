// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Reading `WITHOUT ROWID` tables through SQL (#722). Such a table's
//! b-tree is an index b-tree whose entries hold whole rows, the `PRIMARY
//! KEY` columns first; a query used to find no rows in it at all. Every
//! query here runs on db-core and, where the `sqlite3` CLI is installed,
//! on sqlite3 over the same file, and the rows must agree.
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

/// Written by sqlite3 3.53.4: `w1(a, b, c, PRIMARY KEY (c, a))` (the key
/// is not the leading columns; 3000 rows, several b-tree levels),
/// `w2(k INTEGER PRIMARY KEY, v)`, `w3(a, b, PRIMARY KEY (a, b))` (every
/// column in the key), `w4(id PRIMARY KEY, g, n)` with a secondary and a
/// unique index, and a rowid table `r` to join against.
const FIXTURE: &str = "tests/fixtures/btrees/without_rowid_tables.db";

static NEXT: AtomicU32 = AtomicU32::new(0);

struct TempDb(PathBuf);

impl TempDb {
    fn new(label: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "db-core-wr-read-{label}-{}-{}.db",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
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
        other => panic!("{other:?}"),
    }
}

fn ours(e: &mut RowEngine, sql: &str) -> Vec<String> {
    e.run_query(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err}"))
        .rows
        .iter()
        .map(|row| row.iter().map(cell).collect::<Vec<_>>().join("|"))
        .collect()
}

fn theirs(path: &Path, sql: &str) -> Option<Vec<String>> {
    let out = Command::new("sqlite3").arg(path).arg(sql).output().ok()?;
    assert!(
        out.status.success(),
        "sqlite3 rejected {sql}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
    )
}

/// `sql` returns the same rows in db-core and in sqlite3.
fn agree(e: &mut RowEngine, db: &TempDb, sql: &str) -> Vec<String> {
    let rows = ours(e, sql);
    if let Some(expected) = theirs(db.path(), sql) {
        assert_eq!(rows, expected, "{sql}");
    }
    rows
}

#[test]
fn counts_and_aggregates() {
    let db = TempDb::new("counts");
    let mut e = RowEngine::open(db.path()).expect("open");
    for (table, count) in [("w1", "3000"), ("w2", "300"), ("w3", "200"), ("w4", "400")] {
        assert_eq!(
            agree(&mut e, &db, &format!("SELECT count(*) FROM {table}")),
            [count]
        );
    }
    agree(
        &mut e,
        &db,
        "SELECT count(*), sum(b), min(a), max(c) FROM w1",
    );
    agree(
        &mut e,
        &db,
        "SELECT c, count(*), max(b) FROM w1 GROUP BY c ORDER BY c LIMIT 7",
    );
    agree(&mut e, &db, "SELECT count(DISTINCT c) FROM w1");
}

#[test]
fn columns_come_back_in_declared_order_whatever_the_key_is() {
    let db = TempDb::new("order");
    let mut e = RowEngine::open(db.path()).expect("open");
    // `w1` is stored c, a, b but declared a, b, c.
    assert_eq!(
        agree(&mut e, &db, "SELECT * FROM w1 WHERE b = 77"),
        ["a77|77|c77"]
    );
    assert_eq!(
        agree(&mut e, &db, "SELECT c, b, a FROM w1 WHERE b = 77"),
        ["c77|77|a77"]
    );
    agree(&mut e, &db, "SELECT a, b, c FROM w1 ORDER BY b LIMIT 10");
    agree(&mut e, &db, "SELECT * FROM w2 ORDER BY k LIMIT 5");
    agree(&mut e, &db, "SELECT * FROM w3 ORDER BY a, b LIMIT 12");
}

#[test]
fn filters_on_key_and_non_key_columns() {
    let db = TempDb::new("filters");
    let mut e = RowEngine::open(db.path()).expect("open");
    for sql in [
        "SELECT a, b FROM w1 WHERE c = 'c5' ORDER BY a",
        "SELECT a, b FROM w1 WHERE c = 'c5' AND a = 'a105'",
        "SELECT count(*) FROM w1 WHERE c >= 'c90' AND c < 'c95'",
        "SELECT a FROM w1 WHERE b BETWEEN 100 AND 110 ORDER BY b",
        "SELECT a FROM w1 WHERE b IN (3, 30, 300, 3000) ORDER BY b",
        "SELECT count(*) FROM w1 WHERE a LIKE 'a1%'",
        "SELECT a, b FROM w3 WHERE a = 3 ORDER BY b",
        "SELECT v FROM w2 WHERE k = 42",
        "SELECT v FROM w2 WHERE k = 43",
        "SELECT k FROM w2 WHERE k > 880 ORDER BY k",
        "SELECT k, v FROM w2 WHERE v = 'v10'",
    ] {
        agree(&mut e, &db, sql);
    }
}

#[test]
fn a_secondary_index_on_a_without_rowid_table_is_not_used_to_fetch_rows() {
    // Its entries end in the key columns, not a rowid, so the rowid-based
    // index walks must not run on it.
    let db = TempDb::new("secondary");
    let mut e = RowEngine::open(db.path()).expect("open");
    for sql in [
        "SELECT id FROM w4 WHERE g = 'g3' ORDER BY id",
        "SELECT id, g, n FROM w4 WHERE n = 40",
        "SELECT id FROM w4 WHERE g BETWEEN 'g1' AND 'g2' ORDER BY id",
        "SELECT id FROM w4 WHERE g > 'g15' ORDER BY id",
        "SELECT id FROM w4 WHERE g IN ('g1', 'g16') ORDER BY id",
        "SELECT id FROM w4 WHERE g LIKE 'g1%' ORDER BY id LIMIT 20",
        "SELECT id FROM w4 WHERE g = 'g3' AND n > 100 ORDER BY id",
        "SELECT g, count(*) FROM w4 GROUP BY g ORDER BY g",
        "SELECT id FROM w4 ORDER BY g, id LIMIT 15",
        "SELECT DISTINCT g FROM w4 ORDER BY g",
        "SELECT count(*) FROM w4 WHERE g = 'g3'",
        "SELECT min(n), max(n) FROM w4",
        "SELECT id FROM w4 WHERE n >= 100 AND n < 130 ORDER BY id",
    ] {
        agree(&mut e, &db, sql);
    }
}

#[test]
fn joins_and_subqueries_over_a_without_rowid_table() {
    let db = TempDb::new("joins");
    let mut e = RowEngine::open(db.path()).expect("open");
    for sql in [
        "SELECT r.id, w4.g FROM r JOIN w4 ON w4.id = r.k ORDER BY r.id",
        "SELECT r.id, w2.v FROM r JOIN w2 ON w2.k = r.k * 3 ORDER BY r.id",
        "SELECT w4.id, r.id FROM w4 JOIN r ON r.k = w4.id ORDER BY w4.id, r.id",
        "SELECT count(*) FROM w1 JOIN w4 ON w1.b = w4.id",
        "SELECT w3.a, count(*) FROM w3 JOIN r ON r.k = w3.b GROUP BY w3.a ORDER BY w3.a",
        "SELECT count(*) FROM r WHERE k IN (SELECT id FROM w4 WHERE n > 100)",
        "SELECT id FROM r WHERE EXISTS (SELECT 1 FROM w2 WHERE w2.k = r.k) ORDER BY id",
        "SELECT (SELECT max(n) FROM w4 WHERE w4.g = 'g3') FROM r LIMIT 1",
        "SELECT g FROM (SELECT g, count(*) AS c FROM w4 GROUP BY g) WHERE c > 23 ORDER BY g",
        "SELECT a, b FROM w1 UNION SELECT a, b FROM w3 ORDER BY 1, 2 LIMIT 5",
    ] {
        agree(&mut e, &db, sql);
    }
}

#[test]
fn limit_offset_order_and_distinct() {
    let db = TempDb::new("order-limit");
    let mut e = RowEngine::open(db.path()).expect("open");
    for sql in [
        "SELECT a FROM w1 ORDER BY a LIMIT 10 OFFSET 25",
        "SELECT a FROM w1 ORDER BY c DESC, a LIMIT 10",
        "SELECT DISTINCT c FROM w1 ORDER BY c LIMIT 8",
        "SELECT b FROM w1 LIMIT 3 OFFSET 2990",
        "SELECT k FROM w2 ORDER BY k DESC LIMIT 4",
    ] {
        agree(&mut e, &db, sql);
    }
}

#[test]
fn rowid_is_not_a_column_of_a_without_rowid_table() {
    let db = TempDb::new("no-rowid");
    let mut e = RowEngine::open(db.path()).expect("open");
    let err = e.run_query("SELECT rowid FROM w1").expect_err("no rowid");
    assert!(err.message.contains("rowid"), "{}", err.message);
}

#[test]
fn integrity_check_accepts_a_valid_without_rowid_file() {
    // Its tables' root pages are index b-tree pages, and a secondary index
    // ends in the key columns rather than a rowid; neither is a problem.
    let db = TempDb::new("integrity");
    let mut e = RowEngine::open(db.path()).expect("open");
    assert_eq!(ours(&mut e, "PRAGMA integrity_check"), ["ok"]);
    assert_eq!(ours(&mut e, "PRAGMA quick_check"), ["ok"]);
    drop(e);
    if let Some(verdict) = theirs(db.path(), "PRAGMA integrity_check") {
        assert_eq!(verdict, ["ok"]);
    }
}

#[test]
fn analyze_skips_without_rowid_tables_and_still_analyzes_the_rest() {
    let db = TempDb::new("analyze");
    let mut e = RowEngine::open(db.path()).expect("open");
    e.run_query("ANALYZE")
        .expect("ANALYZE must not fail over a WITHOUT ROWID table");
    // The rowid table was analyzed; the WITHOUT ROWID ones were not.
    assert_eq!(
        ours(&mut e, "SELECT tbl FROM sqlite_stat1 ORDER BY tbl, idx"),
        ["r"]
    );
    assert_eq!(ours(&mut e, "PRAGMA integrity_check"), ["ok"]);
    drop(e);
    if let Some(verdict) = theirs(db.path(), "PRAGMA integrity_check") {
        assert_eq!(verdict, ["ok"]);
    }
}
