// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `INSERT ... ON CONFLICT ... DO NOTHING` (#696), end to end through
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
use db_core::engine::{Cell, Engine, ErrorKind};

const FIXTURE: &str = "tests/fixtures/btrees/table_single_page.db";

struct TempDb(PathBuf);

/// Tells apart temp files made in the same clock tick: `ours` reuses one
/// label, and a coarse clock gave parallel tests the same path.
static NEXT: AtomicU32 = AtomicU32::new(0);

impl TempDb {
    fn new(label: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "db-core-upsert-{label}-{}-{}-{}.db",
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
        other => panic!("{other:?}"),
    }
}

/// Runs `script` then `query` on db-core, returning each row as `a|b|..`.
fn ours(script: &str, query: &str) -> Vec<String> {
    let db = TempDb::new("ours");
    let mut e = RowEngine::open(db.path()).expect("open");
    e.run_query("DROP TABLE t")
        .expect("drop the fixture's table");
    e.run_query(script)
        .unwrap_or_else(|err| panic!("{script}: {err}"));
    e.run_query(query)
        .unwrap_or_else(|err| panic!("{query}: {err}"))
        .rows
        .iter()
        .map(|row| row.iter().map(cell).collect::<Vec<_>>().join("|"))
        .collect()
}

/// The same through the `sqlite3` CLI; `None` when it is not installed.
fn oracle(script: &str, query: &str) -> Option<Vec<String>> {
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg(format!("{script}; {query}"))
        .output()
        .ok()?;
    assert!(
        out.status.success(),
        "sqlite3 rejected {script}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
    )
}

/// What the `sqlite3` CLI says when `script` fails; `None` when it is not
/// installed. Panics if the script unexpectedly succeeds.
fn oracle_error(script: &str) -> Option<String> {
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg(script)
        .output()
        .ok()?;
    assert!(!out.status.success(), "sqlite3 accepted {script}");
    Some(String::from_utf8_lossy(&out.stderr).to_string())
}

/// `script` leaves `query` returning `expected`, in db-core and in sqlite3.
fn check(script: &str, query: &str, expected: &[&str]) {
    assert_eq!(ours(script, query), expected, "db-core: {script}");
    if let Some(theirs) = oracle(script, query) {
        assert_eq!(theirs, expected, "sqlite3: {script}");
    }
}

fn engine(label: &str) -> (TempDb, RowEngine) {
    let db = TempDb::new(label);
    let mut e = RowEngine::open(db.path()).expect("open");
    e.run_query("DROP TABLE t")
        .expect("drop the fixture's table");
    (db, e)
}

fn err_of(e: &mut RowEngine, sql: &str) -> (ErrorKind, String) {
    let err = e.run_query(sql).expect_err(sql);
    (err.kind, err.message)
}

#[test]
fn do_nothing_without_a_target_skips_any_uniqueness_conflict() {
    check(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, k TEXT UNIQUE, j INT, UNIQUE(j)); \
         INSERT INTO t VALUES (1, 'a', 10); \
         INSERT INTO t VALUES (2, 'a', 20) ON CONFLICT DO NOTHING; \
         INSERT INTO t VALUES (3, 'b', 10) ON CONFLICT DO NOTHING; \
         INSERT INTO t VALUES (1, 'c', 30) ON CONFLICT DO NOTHING; \
         INSERT INTO t VALUES (4, 'd', 40) ON CONFLICT DO NOTHING",
        "SELECT id, k, j FROM t ORDER BY id",
        &["1|a|10", "4|d|40"],
    );
}

#[test]
fn a_target_skips_only_its_own_constraint() {
    let script = "CREATE TABLE t(id INTEGER PRIMARY KEY, k TEXT UNIQUE, j INT UNIQUE); \
                  INSERT INTO t VALUES (1, 'a', 10); \
                  INSERT INTO t VALUES (2, 'a', 20) ON CONFLICT (k) DO NOTHING; \
                  INSERT INTO t VALUES (1, 'z', 30) ON CONFLICT (id) DO NOTHING; \
                  INSERT INTO t VALUES (1, 'y', 40) ON CONFLICT (rowid) DO NOTHING";
    check(script, "SELECT id, k, j FROM t ORDER BY id", &["1|a|10"]);
    // The conflict on `j` is not the target's: it aborts.
    let (_db, mut e) = engine("target-only");
    e.run_query(script).unwrap();
    let (kind, message) = err_of(
        &mut e,
        "INSERT INTO t VALUES (5, 'q', 10) ON CONFLICT (k) DO NOTHING",
    );
    assert_eq!(kind, ErrorKind::Execute);
    assert!(
        message.contains("UNIQUE constraint failed: t.j"),
        "{message}"
    );
    if let Some(theirs) = oracle_error(&format!(
        "{script}; INSERT INTO t VALUES (5, 'q', 10) ON CONFLICT (k) DO NOTHING"
    )) {
        assert!(theirs.contains("UNIQUE constraint failed: t.j"), "{theirs}");
    }
}

#[test]
fn several_rows_skip_individually() {
    check(
        "CREATE TABLE t(k TEXT PRIMARY KEY, n INT); \
         INSERT INTO t VALUES ('a', 1); \
         INSERT INTO t VALUES ('b', 2), ('a', 3), ('c', 4), ('b', 5) ON CONFLICT (k) DO NOTHING",
        "SELECT k, n FROM t ORDER BY k",
        &["a|1", "b|2", "c|4"],
    );
}

#[test]
fn a_select_source_skips_its_conflicting_rows() {
    check(
        "CREATE TABLE t(k INT PRIMARY KEY, n INT); \
         CREATE TABLE s(k INT, n INT); \
         INSERT INTO t VALUES (1, 100); \
         INSERT INTO s VALUES (1, 1), (2, 2), (2, 3), (3, 4); \
         INSERT INTO t SELECT k, n FROM s WHERE true ON CONFLICT DO NOTHING",
        "SELECT k, n FROM t ORDER BY k",
        &["1|100", "2|2", "3|4"],
    );
}

#[test]
fn composite_and_implicit_indexes_are_matched_by_their_columns() {
    check(
        "CREATE TABLE t(a INT, b INT, c INT, PRIMARY KEY (a, b)); \
         INSERT INTO t VALUES (1, 2, 0); \
         INSERT INTO t VALUES (1, 2, 1) ON CONFLICT (a, b) DO NOTHING; \
         INSERT INTO t VALUES (1, 2, 2) ON CONFLICT (b, a) DO NOTHING; \
         INSERT INTO t VALUES (1, 3, 3) ON CONFLICT (a, b) DO NOTHING",
        "SELECT a, b, c FROM t ORDER BY b",
        &["1|2|0", "1|3|3"],
    );
}

#[test]
fn not_null_and_check_violations_still_abort() {
    // `OR IGNORE` would skip these rows; `DO NOTHING` covers uniqueness only.
    let (_db, mut e) = engine("not-null");
    e.run_query(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, k TEXT UNIQUE, n INT NOT NULL, c INT CHECK (c > 0)); \
         INSERT INTO t VALUES (1, 'a', 1, 1)",
    )
    .unwrap();
    let (kind, message) = err_of(
        &mut e,
        "INSERT INTO t VALUES (2, 'b', NULL, 1) ON CONFLICT DO NOTHING",
    );
    assert_eq!(kind, ErrorKind::Execute);
    assert!(
        message.contains("NOT NULL constraint failed: t.n"),
        "{message}"
    );
    let (_, message) = err_of(
        &mut e,
        "INSERT INTO t VALUES (3, 'c', 1, 0) ON CONFLICT DO NOTHING",
    );
    assert!(message.contains("CHECK constraint failed"), "{message}");
}

#[test]
fn a_target_that_matches_no_constraint_is_refused_as_sqlite3_refuses_it() {
    let (_db, mut e) = engine("mismatch");
    e.run_query("CREATE TABLE t(id INTEGER PRIMARY KEY, a UNIQUE, b UNIQUE, c)")
        .unwrap();
    for sql in [
        "INSERT INTO t VALUES (1, 1, 1, 1) ON CONFLICT (c) DO NOTHING",
        "INSERT INTO t VALUES (1, 1, 1, 1) ON CONFLICT (a, b) DO NOTHING",
        "INSERT INTO t VALUES (1, 1, 1, 1) ON CONFLICT (a) WHERE a > 0 DO NOTHING",
    ] {
        let (kind, message) = err_of(&mut e, sql);
        assert_eq!(kind, ErrorKind::Compile, "{sql}");
        assert_eq!(
            message, "ON CONFLICT clause does not match any PRIMARY KEY or UNIQUE constraint",
            "{sql}"
        );
    }
    let (kind, message) = err_of(
        &mut e,
        "INSERT INTO t VALUES (1, 1, 1, 1) ON CONFLICT (nope) DO NOTHING",
    );
    assert_eq!(kind, ErrorKind::Compile);
    assert!(message.contains("no such column: nope"), "{message}");
}

#[test]
fn do_update_accumulates_into_the_existing_row() {
    check(
        "CREATE TABLE c(k TEXT PRIMARY KEY, n INT); \
         INSERT INTO c VALUES ('a', 1) ON CONFLICT (k) DO UPDATE SET n = n + excluded.n; \
         INSERT INTO c VALUES ('a', 10), ('b', 2), ('a', 100), ('b', 20) \
             ON CONFLICT (k) DO UPDATE SET n = n + excluded.n",
        "SELECT k, n FROM c ORDER BY k",
        &["a|111", "b|22"],
    );
}

#[test]
fn do_update_on_the_rowid() {
    check(
        "CREATE TABLE r(id INTEGER PRIMARY KEY, v TEXT, n INT); \
         INSERT INTO r VALUES (1, 'a', 1); \
         INSERT INTO r VALUES (1, 'b', 2), (2, 'c', 3), (1, 'd', 4) \
             ON CONFLICT (id) DO UPDATE SET v = excluded.v, n = n + excluded.n",
        "SELECT id, v, n FROM r ORDER BY id",
        &["1|d|7", "2|c|3"],
    );
    // The rowid keyword names the same constraint, and the key itself may
    // be what the update sets.
    check(
        "CREATE TABLE r(id INTEGER PRIMARY KEY, v TEXT); \
         INSERT INTO r VALUES (1, 'a'); \
         INSERT INTO r VALUES (1, 'x') ON CONFLICT (rowid) DO UPDATE SET id = excluded.id + 100, v = 'moved'",
        "SELECT id, v FROM r ORDER BY id",
        &["101|moved"],
    );
}

#[test]
fn excluded_carries_the_column_affinity_and_the_where_gates_the_update() {
    check(
        "CREATE TABLE a(k INT PRIMARY KEY, n INTEGER, s TEXT); \
         INSERT INTO a VALUES (1, 5, 'old'); \
         INSERT INTO a VALUES (1, '7', 'new') ON CONFLICT (k) DO UPDATE SET n = excluded.n, s = excluded.s",
        "SELECT k, n, typeof(n), s FROM a",
        &["1|7|integer|new"],
    );
    check(
        "CREATE TABLE g(k TEXT PRIMARY KEY, n INT); \
         INSERT INTO g VALUES ('a', 10); \
         INSERT INTO g VALUES ('a', 3), ('a', 30), ('a', 20) \
             ON CONFLICT (k) DO UPDATE SET n = excluded.n WHERE excluded.n > n",
        "SELECT k, n FROM g",
        &["a|30"],
    );
}

#[test]
fn clauses_chain_by_target_and_a_clause_without_one_answers_the_rest() {
    check(
        "CREATE TABLE m(a INT, b INT, c INT, PRIMARY KEY (a, b), UNIQUE (c)); \
         INSERT INTO m VALUES (1, 1, 10); \
         INSERT INTO m VALUES (1, 1, 20) ON CONFLICT (a, b) DO UPDATE SET c = excluded.c \
             ON CONFLICT (c) DO NOTHING; \
         INSERT INTO m VALUES (2, 2, 20) ON CONFLICT (a, b) DO UPDATE SET c = 99 \
             ON CONFLICT (c) DO NOTHING; \
         INSERT INTO m VALUES (3, 3, 30)",
        "SELECT a, b, c FROM m ORDER BY a",
        &["1|1|20", "3|3|30"],
    );
    check(
        "CREATE TABLE d(k TEXT UNIQUE, j INT UNIQUE, n INT); \
         INSERT INTO d VALUES ('a', 1, 0), ('b', 2, 0); \
         INSERT INTO d VALUES ('a', 9, 5) ON CONFLICT DO UPDATE SET n = n + 1; \
         INSERT INTO d VALUES ('z', 2, 5) ON CONFLICT DO UPDATE SET n = n + 10",
        "SELECT k, j, n FROM d ORDER BY k",
        &["a|1|1", "b|2|10"],
    );
}

#[test]
fn do_update_reads_a_select_source_row_by_row() {
    check(
        "CREATE TABLE t(k INT PRIMARY KEY, n INT); \
         CREATE TABLE s(k INT, n INT); \
         INSERT INTO t VALUES (1, 100); \
         INSERT INTO s VALUES (1, 1), (2, 2), (2, 3), (3, 4); \
         INSERT INTO t SELECT k, n FROM s WHERE true \
             ON CONFLICT (k) DO UPDATE SET n = n + excluded.n",
        "SELECT k, n FROM t ORDER BY k",
        &["1|101", "2|5", "3|4"],
    );
}

#[test]
fn a_bare_column_is_the_existing_row_and_the_table_name_qualifies_it() {
    check(
        "CREATE TABLE q(k TEXT PRIMARY KEY, n INT, m INT); \
         INSERT INTO q VALUES ('a', 1, 2); \
         INSERT INTO q VALUES ('a', 10, 20) ON CONFLICT (k) DO UPDATE SET n = q.n + excluded.n, m = m * excluded.m",
        "SELECT k, n, m FROM q",
        &["a|11|40"],
    );
}

#[test]
fn the_updated_row_meets_the_same_constraints_as_an_update() {
    let setup =
        "CREATE TABLE u(k TEXT PRIMARY KEY, j INT UNIQUE, n INT NOT NULL, c INT CHECK (c < 10)); \
                 INSERT INTO u VALUES ('a', 1, 1, 1), ('b', 2, 2, 2)";
    let (_db, mut e) = engine("update-constraints");
    e.run_query(setup).unwrap();
    for (sql, expect, oracle_text) in [
        (
            "INSERT INTO u VALUES ('a', 0, 0, 0) ON CONFLICT (k) DO UPDATE SET j = 2",
            "UNIQUE constraint failed: u.j",
            "UNIQUE constraint failed: u.j",
        ),
        (
            "INSERT INTO u VALUES ('a', 0, 0, 0) ON CONFLICT (k) DO UPDATE SET n = NULL",
            "NOT NULL constraint failed: u.n",
            "NOT NULL constraint failed: u.n",
        ),
        (
            "INSERT INTO u VALUES ('a', 0, 0, 0) ON CONFLICT (k) DO UPDATE SET c = 50",
            "CHECK constraint failed",
            "CHECK constraint failed",
        ),
    ] {
        let (kind, message) = err_of(&mut e, sql);
        assert_eq!(kind, ErrorKind::Execute, "{sql}");
        assert!(message.contains(expect), "{sql}: {message}");
        if let Some(theirs) = oracle_error(&format!("{setup}; {sql}")) {
            assert!(theirs.contains(oracle_text), "{sql}: {theirs}");
        }
    }
    // Setting the key to what it already is is not a conflict with itself.
    check(
        "CREATE TABLE u(k TEXT PRIMARY KEY, j INT UNIQUE, n INT); \
         INSERT INTO u VALUES ('a', 1, 1); \
         INSERT INTO u VALUES ('a', 1, 9) ON CONFLICT (k) DO UPDATE SET j = excluded.j, n = excluded.n",
        "SELECT k, j, n FROM u",
        &["a|1|9"],
    );
}

#[test]
fn a_new_rows_own_not_null_violation_still_aborts_before_it_conflicts() {
    let (_db, mut e) = engine("new-row-not-null");
    e.run_query(
        "CREATE TABLE n(k TEXT PRIMARY KEY, v INT NOT NULL); INSERT INTO n VALUES ('a', 1)",
    )
    .unwrap();
    let (_, message) = err_of(
        &mut e,
        "INSERT INTO n VALUES ('a', NULL) ON CONFLICT (k) DO UPDATE SET v = 5",
    );
    assert!(
        message.contains("NOT NULL constraint failed: n.v"),
        "{message}"
    );
}

#[test]
fn a_set_naming_an_unknown_column_is_refused() {
    let (_db, mut e) = engine("unknown-set-column");
    e.run_query("CREATE TABLE t(k TEXT PRIMARY KEY, n INT)")
        .unwrap();
    let (kind, message) = err_of(
        &mut e,
        "INSERT INTO t VALUES ('a', 1) ON CONFLICT (k) DO UPDATE SET nope = 1",
    );
    assert_eq!(kind, ErrorKind::Compile);
    assert!(message.contains("nope"), "{message}");
}

#[test]
fn the_file_stays_valid_for_sqlite3() {
    let (db, mut e) = engine("integrity");
    e.run_query(
        "CREATE TABLE t(k TEXT PRIMARY KEY, n INT, UNIQUE(n)); \
         INSERT INTO t VALUES ('a', 1), ('b', 2); \
         INSERT INTO t VALUES ('a', 9), ('c', 2), ('d', 4) ON CONFLICT DO NOTHING; \
         INSERT INTO t VALUES ('a', 0), ('e', 5) ON CONFLICT (k) DO UPDATE SET n = n + 100; \
         INSERT INTO t VALUES ('f', 4) ON CONFLICT (n) DO UPDATE SET k = excluded.k",
    )
    .unwrap();
    assert_eq!(
        e.run_query("SELECT k, n FROM t ORDER BY k")
            .unwrap()
            .rows
            .iter()
            .map(|row| row.iter().map(cell).collect::<Vec<_>>().join("|"))
            .collect::<Vec<_>>(),
        ["a|101", "b|2", "e|5", "f|4"]
    );
    drop(e);
    if let Ok(out) = Command::new("sqlite3")
        .arg(db.path())
        .arg("PRAGMA integrity_check")
        .output()
    {
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ok");
    }
}
