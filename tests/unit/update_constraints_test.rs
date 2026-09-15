// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `UPDATE`'s rowid-seek fast path and constraint re-validation, end to
//! end through `engine::row::RowEngine` --
//! `codegen::row::stmt::update` was 76.53% line coverage with none of
//! its `WHERE rowid = <literal>` single-row seek, `WITHOUT ROWID`
//! rejection, or NOT NULL/CHECK re-validation on the new row values
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

const FIXTURE: &str = "tests/fixtures/btrees/table_single_page.db";

struct TempDb(PathBuf);

impl TempDb {
    fn new(label: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "db-core-update-constraints-{label}-{}-{}.db",
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
fn update_with_a_literal_rowid_equality_takes_the_single_row_seek() {
    let db = TempDb::new("rowid-seek-literal");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE us(id INTEGER PRIMARY KEY, v INTEGER); \
         INSERT INTO us VALUES (1, 10); \
         INSERT INTO us VALUES (2, 20)",
    )
    .unwrap();
    e.run_query("UPDATE us SET v = 999 WHERE id = 1").unwrap();
    let rows = e.run_query("SELECT v FROM us ORDER BY id").unwrap().rows;
    assert_eq!(ints(&rows), vec![999, 20]);
}

#[test]
fn update_with_a_literal_rowid_equality_that_matches_nothing_is_a_no_op() {
    let db = TempDb::new("rowid-seek-miss");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE usm(id INTEGER PRIMARY KEY, v INTEGER); \
         INSERT INTO usm VALUES (1, 10)",
    )
    .unwrap();
    e.run_query("UPDATE usm SET v = 999 WHERE id = 42").unwrap();
    let rows = e.run_query("SELECT v FROM usm").unwrap().rows;
    assert_eq!(ints(&rows), vec![10]);
}

#[test]
fn without_rowid_table_is_rejected_at_update_compile_time() {
    let db = TempDb::new("without-rowid-update");
    let mut e = open(&db);
    e.run_query("CREATE TABLE wru(a INTEGER PRIMARY KEY) WITHOUT ROWID")
        .unwrap();
    let err = e.run_query("UPDATE wru SET a = 1").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile);
    assert!(err.message.contains("WITHOUT ROWID"), "{}", err.message);
}

#[test]
fn update_revalidates_not_null_against_the_new_value() {
    let db = TempDb::new("update-notnull");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE unn(a INTEGER NOT NULL); \
         INSERT INTO unn VALUES (1)",
    )
    .unwrap();
    let err = e.run_query("UPDATE unn SET a = NULL").unwrap_err();
    assert!(err.message.contains("NOT NULL"), "{}", err.message);
    let rows = e.run_query("SELECT a FROM unn").unwrap().rows;
    assert_eq!(ints(&rows), vec![1]);
}

#[test]
fn update_revalidates_a_column_level_check_against_the_new_value() {
    let db = TempDb::new("update-check");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE uck(a INTEGER CHECK (a > 0)); \
         INSERT INTO uck VALUES (1)",
    )
    .unwrap();
    let err = e.run_query("UPDATE uck SET a = -1").unwrap_err();
    assert!(err.message.contains("CHECK"), "{}", err.message);
    e.run_query("UPDATE uck SET a = 5").unwrap();
    let rows = e.run_query("SELECT a FROM uck").unwrap().rows;
    assert_eq!(ints(&rows), vec![5]);
}

#[test]
fn update_revalidates_a_table_level_check_across_assigned_columns() {
    let db = TempDb::new("update-table-check");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE utk(a INTEGER, b INTEGER, CHECK (a < b)); \
         INSERT INTO utk VALUES (1, 5)",
    )
    .unwrap();
    let err = e.run_query("UPDATE utk SET a = 10").unwrap_err();
    assert!(err.message.contains("CHECK"), "{}", err.message);
}

#[test]
fn update_or_ignore_skips_a_row_that_would_violate_a_check() {
    let db = TempDb::new("update-or-ignore");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE uoi(id INTEGER PRIMARY KEY, a INTEGER CHECK (a > 0)); \
         INSERT INTO uoi VALUES (1, 5); \
         INSERT INTO uoi VALUES (2, 5)",
    )
    .unwrap();
    e.run_query("UPDATE OR IGNORE uoi SET a = -1 WHERE id = 1")
        .unwrap();
    let rows = e.run_query("SELECT a FROM uoi ORDER BY id").unwrap().rows;
    // The would-be-violating row is left untouched; a plain UPDATE
    // (ABORT) would have errored instead.
    assert_eq!(ints(&rows), vec![5, 5]);
}
