// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `UNION`/`UNION ALL` compound `SELECT`s with `ORDER BY`/`LIMIT`, end
//! to end through `engine::row::RowEngine` --
//! `codegen::row::select::entry::compile_select_compound`'s sorted
//! (`needs_sort`) path, its dedup-under-sort interaction, and its
//! error guards had no test anywhere in the suite (only an unordered,
//! unlimited compound was exercised elsewhere).
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
            "db-core-select-compound-{label}-{}-{}.db",
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
fn union_all_with_order_by_and_limit_offset_sorts_the_whole_compound() {
    let db = TempDb::new("union-all-sort");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE sc_a(a INTEGER); \
         CREATE TABLE sc_b(a INTEGER); \
         INSERT INTO sc_a VALUES (3), (1); \
         INSERT INTO sc_b VALUES (2), (1)",
    )
    .unwrap();
    let rows = e
        .run_query(
            "SELECT a FROM sc_a UNION ALL SELECT a FROM sc_b \
             ORDER BY a LIMIT 2 OFFSET 1",
        )
        .unwrap()
        .rows;
    // Full sorted sequence is 1,1,2,3; OFFSET 1 LIMIT 2 keeps the
    // middle two.
    assert_eq!(ints(&rows), vec![1, 2]);
}

#[test]
fn union_with_order_by_dedups_under_the_sorted_path() {
    let db = TempDb::new("union-sort-dedup");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE ud_a(a INTEGER); \
         CREATE TABLE ud_b(a INTEGER); \
         INSERT INTO ud_a VALUES (2), (1); \
         INSERT INTO ud_b VALUES (1), (3)",
    )
    .unwrap();
    let rows = e
        .run_query("SELECT a FROM ud_a UNION SELECT a FROM ud_b ORDER BY a")
        .unwrap()
        .rows;
    // Plain UNION dedups; the duplicate `1` across both arms collapses
    // to one row even though the sorted (not direct-emit) path is
    // taken because of the ORDER BY.
    assert_eq!(ints(&rows), vec![1, 2, 3]);
}

#[test]
fn order_by_ordinal_position_on_a_compound_sorts_by_that_output_column() {
    let db = TempDb::new("union-ordinal");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE uo_a(a INTEGER, b INTEGER); \
         CREATE TABLE uo_b(a INTEGER, b INTEGER); \
         INSERT INTO uo_a VALUES (1, 30); \
         INSERT INTO uo_b VALUES (2, 10)",
    )
    .unwrap();
    let rows = e
        .run_query("SELECT a, b FROM uo_a UNION ALL SELECT a, b FROM uo_b ORDER BY 2")
        .unwrap()
        .rows
        .into_iter()
        .map(|r| match &r[0] {
            Cell::Int(n) => *n,
            other => panic!("expected Int, got {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(rows, vec![2, 1]);
}

#[test]
fn compound_order_by_an_arbitrary_expression_is_unsupported() {
    let db = TempDb::new("union-order-expr");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE ue_a(a INTEGER); \
         CREATE TABLE ue_b(a INTEGER)",
    )
    .unwrap();
    let err = e
        .run_query("SELECT a FROM ue_a UNION SELECT a FROM ue_b ORDER BY a + 1")
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile);
    assert!(err.message.contains("ordinal"), "{}", err.message);
}

#[test]
fn compound_arm_column_count_mismatch_is_reported() {
    let db = TempDb::new("union-mismatch");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE mm_a(a INTEGER); \
         CREATE TABLE mm_b(a INTEGER, b INTEGER)",
    )
    .unwrap();
    let err = e
        .run_query("SELECT a FROM mm_a UNION SELECT a, b FROM mm_b")
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile);
}

#[test]
fn compound_arm_with_a_join_is_unsupported() {
    let db = TempDb::new("union-join-arm");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE uj_a(a INTEGER); \
         CREATE TABLE uj_b(a INTEGER, k INTEGER); \
         CREATE TABLE uj_c(k INTEGER)",
    )
    .unwrap();
    let err = e
        .run_query("SELECT a FROM uj_a UNION SELECT uj_b.a FROM uj_b JOIN uj_c ON uj_b.k = uj_c.k")
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Compile);
    assert!(err.message.contains("JOIN"), "{}", err.message);
}

#[test]
fn compound_arm_over_a_from_subquery_materializes_per_arm() {
    let db = TempDb::new("union-subquery-arm");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE sq_a(a INTEGER); \
         CREATE TABLE sq_b(a INTEGER); \
         INSERT INTO sq_a VALUES (1), (2); \
         INSERT INTO sq_b VALUES (3)",
    )
    .unwrap();
    let rows = e
        .run_query(
            "SELECT a FROM (SELECT a FROM sq_a WHERE a > 1) sub \
             UNION ALL SELECT a FROM sq_b ORDER BY a",
        )
        .unwrap()
        .rows;
    assert_eq!(ints(&rows), vec![2, 3]);
}
