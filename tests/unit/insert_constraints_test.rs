// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `INSERT` constraint enforcement and `INSERT ... SELECT`, end to end
//! through `engine::row::RowEngine` -- `codegen::row::stmt::insert` was
//! 59.53% line coverage with none of its `NOT NULL`/`CHECK`/`DEFAULT`/
//! `UNIQUE`/`ON CONFLICT`/`AUTOINCREMENT`/`INSERT ... SELECT` machinery
//! exercised anywhere in the suite.
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

use db_core::engine::row::RowEngine;
use db_core::engine::{Cell, Engine, ErrorKind};

const FIXTURE: &str = "tests/corpus/fixtures/btrees/table_single_page.db";

struct TempDb(PathBuf);

impl TempDb {
    fn new(label: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "db-core-insert-constraints-{label}-{}-{}.db",
            std::process::id(),
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

fn open(db: &TempDb) -> RowEngine {
    RowEngine::open(db.path()).expect("open fixture copy")
}

fn ints(rows: &[Vec<Cell>]) -> Vec<i64> {
    rows.iter()
        .map(|r| match &r[0] {
            Cell::Int(n) => *n,
            other => panic!("expected Int, got {other:?}"),
        })
        .collect()
}

#[test]
fn not_null_violation_is_a_compile_time_reported_constraint_error() {
    let db = TempDb::new("notnull");
    let mut e = open(&db);
    e.run_query("CREATE TABLE nn(a INTEGER NOT NULL, b INTEGER)")
        .unwrap();
    let err = e.run_query("INSERT INTO nn(b) VALUES (1)").unwrap_err();
    assert!(err.message.contains("NOT NULL"), "{}", err.message);
    // The row was rejected -- the table stays empty.
    let rows = e.run_query("SELECT count(*) FROM nn").unwrap().rows;
    assert_eq!(ints(&rows), vec![0]);
}

#[test]
fn check_constraint_violation_is_reported() {
    let db = TempDb::new("check");
    let mut e = open(&db);
    e.run_query("CREATE TABLE ck(a INTEGER CHECK (a > 0))")
        .unwrap();
    let err = e.run_query("INSERT INTO ck(a) VALUES (-1)").unwrap_err();
    assert!(err.message.contains("CHECK"), "{}", err.message);
    e.run_query("INSERT INTO ck(a) VALUES (1)").unwrap();
    let rows = e.run_query("SELECT a FROM ck").unwrap().rows;
    assert_eq!(ints(&rows), vec![1]);
}

#[test]
fn table_level_check_constraint_sees_every_column() {
    let db = TempDb::new("table-check");
    let mut e = open(&db);
    e.run_query("CREATE TABLE tck(a INTEGER, b INTEGER, CHECK (a < b))")
        .unwrap();
    let err = e
        .run_query("INSERT INTO tck(a, b) VALUES (5, 1)")
        .unwrap_err();
    assert!(err.message.contains("CHECK"), "{}", err.message);
    e.run_query("INSERT INTO tck(a, b) VALUES (1, 5)").unwrap();
}

#[test]
fn default_value_fills_an_omitted_column() {
    let db = TempDb::new("default");
    let mut e = open(&db);
    e.run_query("CREATE TABLE df(a INTEGER, b INTEGER DEFAULT 42)")
        .unwrap();
    e.run_query("INSERT INTO df(a) VALUES (1)").unwrap();
    let rows = e.run_query("SELECT b FROM df").unwrap().rows;
    assert_eq!(ints(&rows), vec![42]);
}

#[test]
fn or_replace_substitutes_default_for_an_explicit_null_literal() {
    let db = TempDb::new("replace-default");
    let mut e = open(&db);
    e.run_query("CREATE TABLE rd(a INTEGER, b INTEGER DEFAULT 9)")
        .unwrap();
    e.run_query("INSERT OR REPLACE INTO rd(a, b) VALUES (1, NULL)")
        .unwrap();
    let rows = e.run_query("SELECT b FROM rd").unwrap().rows;
    assert_eq!(ints(&rows), vec![9]);
}

#[test]
fn on_conflict_ignore_skips_the_conflicting_row_and_keeps_going() {
    let db = TempDb::new("ignore");
    let mut e = open(&db);
    e.run_query("CREATE TABLE ig(id INTEGER PRIMARY KEY, v INTEGER)")
        .unwrap();
    e.run_query("INSERT INTO ig VALUES (1, 100)").unwrap();
    // #1 conflicts on the rowid PK and is silently skipped; #2 still
    // gets inserted -- proof `INSERT OR IGNORE` doesn't abort the whole
    // statement the way plain `INSERT` (`ABORT`) would.
    e.run_query("INSERT OR IGNORE INTO ig VALUES (1, 999), (2, 200)")
        .unwrap();
    let rows = e.run_query("SELECT v FROM ig ORDER BY id").unwrap().rows;
    assert_eq!(ints(&rows), vec![100, 200]);
}

#[test]
fn on_conflict_replace_overwrites_the_conflicting_row() {
    let db = TempDb::new("replace");
    let mut e = open(&db);
    e.run_query("CREATE TABLE rp(id INTEGER PRIMARY KEY, v INTEGER)")
        .unwrap();
    e.run_query("INSERT INTO rp VALUES (1, 100)").unwrap();
    e.run_query("INSERT OR REPLACE INTO rp VALUES (1, 999)")
        .unwrap();
    let rows = e.run_query("SELECT v FROM rp WHERE id = 1").unwrap().rows;
    assert_eq!(ints(&rows), vec![999]);
}

#[test]
fn default_insert_conflict_action_reports_the_error_but_keeps_prior_rows_in_the_statement() {
    // Real SQLite's default (`ABORT`) conflict action rolls back the
    // *whole statement*, undoing (2, 200) too. db-core's own doc
    // comment on this module says why that isn't true here yet: "no
    // per-transaction/per-statement partial-rollback machinery at the
    // VDBE layer" -- `ROLLBACK`/`FAIL`/`ABORT` all compile to the same
    // single `Halt`, which stops the scan but doesn't undo writes
    // already made earlier in the same statement. This test pins the
    // current (documented-gap) behavior rather than the ideal one.
    let db = TempDb::new("abort");
    let mut e = open(&db);
    e.run_query("CREATE TABLE ab(id INTEGER PRIMARY KEY, v INTEGER)")
        .unwrap();
    e.run_query("INSERT INTO ab VALUES (1, 100)").unwrap();
    let err = e
        .run_query("INSERT INTO ab VALUES (2, 200), (1, 999)")
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Execute);
    let rows = e.run_query("SELECT v FROM ab ORDER BY id").unwrap().rows;
    assert_eq!(ints(&rows), vec![100, 200]);
}

#[test]
fn unique_index_rejects_a_duplicate_non_pk_value() {
    // An inline column `UNIQUE` constraint alone has no backing index
    // (db-core doesn't auto-create a `sqlite_autoindex_*` entry the way
    // stock SQLite does -- a documented `CREATE TABLE`-side gap, not an
    // INSERT-codegen one), so enforcement here goes through an explicit
    // `CREATE UNIQUE INDEX` instead.
    let db = TempDb::new("unique");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE uq(id INTEGER PRIMARY KEY, k INTEGER); \
         CREATE UNIQUE INDEX uq_k ON uq(k); \
         INSERT INTO uq(k) VALUES (1)",
    )
    .unwrap();
    let err = e.run_query("INSERT INTO uq(k) VALUES (1)").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Execute);
    let rows = e.run_query("SELECT count(*) FROM uq").unwrap().rows;
    assert_eq!(ints(&rows), vec![1]);
}

#[test]
fn autoincrement_never_reuses_a_rowid_after_deletion() {
    let db = TempDb::new("autoinc");
    let mut e = open(&db);
    e.run_query("CREATE TABLE ai(id INTEGER PRIMARY KEY AUTOINCREMENT, v INTEGER)")
        .unwrap();
    e.run_query("INSERT INTO ai(v) VALUES (1); DELETE FROM ai WHERE id = 1")
        .unwrap();
    e.run_query("INSERT INTO ai(v) VALUES (2)").unwrap();
    let rows = e.run_query("SELECT id FROM ai").unwrap().rows;
    // A plain (non-autoincrement) rowid alias would reuse id 1 here;
    // AUTOINCREMENT must not.
    assert_eq!(ints(&rows), vec![2]);
}

#[test]
fn insert_select_from_a_single_table_source() {
    let db = TempDb::new("insert-select");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE src(a INTEGER); \
         CREATE TABLE dst(a INTEGER); \
         INSERT INTO src VALUES (1), (2), (3)",
    )
    .unwrap();
    e.run_query("INSERT INTO dst SELECT a FROM src WHERE a > 1")
        .unwrap();
    let rows = e.run_query("SELECT a FROM dst ORDER BY a").unwrap().rows;
    assert_eq!(ints(&rows), vec![2, 3]);
}

#[test]
fn insert_select_from_a_joined_source() {
    let db = TempDb::new("insert-select-join");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE isj_a(id INTEGER PRIMARY KEY, k INTEGER); \
         CREATE TABLE isj_b(id INTEGER PRIMARY KEY, k INTEGER, w INTEGER); \
         CREATE TABLE isj_dst(k INTEGER, w INTEGER); \
         INSERT INTO isj_a(k) VALUES (1); \
         INSERT INTO isj_b(k, w) VALUES (1, 100)",
    )
    .unwrap();
    e.run_query(
        "INSERT INTO isj_dst SELECT isj_a.k, isj_b.w FROM isj_a JOIN isj_b ON isj_a.k = isj_b.k",
    )
    .unwrap();
    let rows = e
        .run_query("SELECT k, w FROM isj_dst")
        .unwrap()
        .rows
        .into_iter()
        .map(|r| match (&r[0], &r[1]) {
            (Cell::Int(k), Cell::Int(w)) => (*k, *w),
            other => panic!("expected two Ints, got {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(rows, vec![(1, 100)]);
}

#[test]
fn without_rowid_table_is_rejected_at_insert_compile_time() {
    let db = TempDb::new("without-rowid");
    let mut e = open(&db);
    e.run_query("CREATE TABLE wr(a INTEGER PRIMARY KEY) WITHOUT ROWID")
        .unwrap();
    let err = e.run_query("INSERT INTO wr VALUES (1)").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile);
    assert!(err.message.contains("WITHOUT ROWID"), "{}", err.message);
}
