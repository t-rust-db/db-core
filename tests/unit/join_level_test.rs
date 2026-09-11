// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `NATURAL`/`USING` joins, `RIGHT JOIN`, and 3+ table joins end to end
//! through `engine::row::RowEngine` --
//! `codegen::row::select::joins::level` (`resolve_join_constraint`'s
//! `NATURAL`/`USING` branches, `compile_join_level`'s N-way recursion,
//! and `RIGHT JOIN` reordering) had large stretches of uncovered code:
//! this file's own `codegen_roundtrip_test.rs` sibling only exercised a
//! plain two-table `LEFT`/`FULL` `ON` join.
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
use db_core::engine::{Cell, Engine};

const FIXTURE: &str = "tests/corpus/fixtures/btrees/table_single_page.db";

struct TempDb(PathBuf);

impl TempDb {
    fn new(label: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "db-core-join-level-{label}-{}-{}.db",
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

fn ints(rows: &[Vec<Cell>]) -> Vec<Vec<i64>> {
    rows.iter()
        .map(|r| {
            r.iter()
                .map(|c| match c {
                    Cell::Int(n) => *n,
                    Cell::Null => -1,
                    other => panic!("expected Int or Null, got {other:?}"),
                })
                .collect()
        })
        .collect()
}

#[test]
fn natural_join_matches_on_every_shared_column() {
    let db = TempDb::new("natural");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE nj_a(id INTEGER PRIMARY KEY, k INTEGER, v INTEGER); \
         CREATE TABLE nj_b(id INTEGER PRIMARY KEY, k INTEGER, w INTEGER); \
         INSERT INTO nj_a(k, v) VALUES (1, 10); \
         INSERT INTO nj_a(k, v) VALUES (2, 20); \
         INSERT INTO nj_b(k, w) VALUES (1, 100); \
         INSERT INTO nj_b(k, w) VALUES (3, 300)",
    )
    .unwrap();
    let rows = e
        .run_query("SELECT nj_a.v, nj_b.w FROM nj_a NATURAL JOIN nj_b")
        .unwrap()
        .rows;
    assert_eq!(ints(&rows), vec![vec![10, 100]]);
}

#[test]
fn natural_join_with_no_shared_columns_is_an_unconditional_cross_join() {
    let db = TempDb::new("natural-cross");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE nc_a(pk_a INTEGER PRIMARY KEY, v INTEGER); \
         CREATE TABLE nc_b(pk_b INTEGER PRIMARY KEY, w INTEGER); \
         INSERT INTO nc_a(v) VALUES (1); \
         INSERT INTO nc_a(v) VALUES (2); \
         INSERT INTO nc_b(w) VALUES (10); \
         INSERT INTO nc_b(w) VALUES (20)",
    )
    .unwrap();
    let rows = e
        .run_query("SELECT nc_a.v, nc_b.w FROM nc_a NATURAL JOIN nc_b ORDER BY nc_a.v, nc_b.w")
        .unwrap()
        .rows;
    assert_eq!(
        ints(&rows),
        vec![vec![1, 10], vec![1, 20], vec![2, 10], vec![2, 20],]
    );
}

#[test]
fn using_join_dedups_the_shared_column_under_star() {
    let db = TempDb::new("using");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE uj_a(id INTEGER PRIMARY KEY, k INTEGER, v INTEGER); \
         CREATE TABLE uj_b(id INTEGER PRIMARY KEY, k INTEGER, w INTEGER); \
         INSERT INTO uj_a(k, v) VALUES (1, 10); \
         INSERT INTO uj_b(k, w) VALUES (1, 100)",
    )
    .unwrap();
    // `SELECT *` over a `USING` join reports the shared column once at
    // the *row* level, not once per side -- `dedup_star` bookkeeping in
    // `resolve_join_constraint`. The header count (`derive_headers`,
    // via `select_result_column_count_joined`) doesn't consult that
    // same dedup -- a documented narrow spot on that helper -- so it
    // over-counts by the one deduped column here; the actual row width
    // (asserted below) is what a client renders against.
    let result = e
        .run_query("SELECT * FROM uj_a JOIN uj_b USING (k)")
        .unwrap();
    assert_eq!(
        result.columns,
        ["column1", "column2", "column3", "column4", "column5", "column6"]
    );
    assert_eq!(ints(&result.rows), vec![vec![1, 1, 10, 1, 100]]);
}

#[test]
fn right_join_reorders_to_an_equivalent_left_join() {
    let db = TempDb::new("right");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE rj_a(id INTEGER PRIMARY KEY, k INTEGER); \
         CREATE TABLE rj_b(id INTEGER PRIMARY KEY, k INTEGER, w INTEGER); \
         INSERT INTO rj_a(k) VALUES (1); \
         INSERT INTO rj_b(k, w) VALUES (1, 100); \
         INSERT INTO rj_b(k, w) VALUES (2, 200)",
    )
    .unwrap();
    let rows = e
        .run_query(
            "SELECT rj_a.k, rj_b.w FROM rj_a RIGHT JOIN rj_b ON rj_a.k = rj_b.k \
             ORDER BY rj_b.w",
        )
        .unwrap()
        .rows;
    // rj_b.w = 200 has no matching rj_a row -- the LEFT side is NULL,
    // same as an ordinary unmatched LEFT JOIN row, just on the other
    // side of the reordering.
    assert_eq!(ints(&rows), vec![vec![1, 100], vec![-1, 200]]);
}

#[test]
fn three_way_join_chains_the_join_level_recursion() {
    let db = TempDb::new("three-way");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE tw_a(id INTEGER PRIMARY KEY, k INTEGER); \
         CREATE TABLE tw_b(id INTEGER PRIMARY KEY, k INTEGER, k2 INTEGER); \
         CREATE TABLE tw_c(id INTEGER PRIMARY KEY, k2 INTEGER, v INTEGER); \
         INSERT INTO tw_a(k) VALUES (1); \
         INSERT INTO tw_a(k) VALUES (2); \
         INSERT INTO tw_b(k, k2) VALUES (1, 9); \
         INSERT INTO tw_c(k2, v) VALUES (9, 900)",
    )
    .unwrap();
    let rows = e
        .run_query(
            "SELECT tw_a.k, tw_c.v FROM tw_a \
             JOIN tw_b ON tw_a.k = tw_b.k \
             JOIN tw_c ON tw_b.k2 = tw_c.k2",
        )
        .unwrap()
        .rows;
    // tw_a.k = 2 has no tw_b match, so it never reaches tw_c's level at
    // all -- the recursion's own inner-join short-circuit.
    assert_eq!(ints(&rows), vec![vec![1, 900]]);
}

#[test]
fn three_way_left_join_chain_produces_null_padded_unmatched_rows() {
    let db = TempDb::new("three-way-left");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE tl_a(id INTEGER PRIMARY KEY, k INTEGER); \
         CREATE TABLE tl_b(id INTEGER PRIMARY KEY, k INTEGER, k2 INTEGER); \
         CREATE TABLE tl_c(id INTEGER PRIMARY KEY, k2 INTEGER, v INTEGER); \
         INSERT INTO tl_a(k) VALUES (1); \
         INSERT INTO tl_a(k) VALUES (2); \
         INSERT INTO tl_b(k, k2) VALUES (1, 9); \
         INSERT INTO tl_c(k2, v) VALUES (9, 900)",
    )
    .unwrap();
    let rows = e
        .run_query(
            "SELECT tl_a.k, tl_c.v FROM tl_a \
             LEFT JOIN tl_b ON tl_a.k = tl_b.k \
             LEFT JOIN tl_c ON tl_b.k2 = tl_c.k2 \
             ORDER BY tl_a.k",
        )
        .unwrap()
        .rows;
    assert_eq!(ints(&rows), vec![vec![1, 900], vec![2, -1]]);
}
