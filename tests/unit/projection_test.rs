// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `codegen::row::select::projection`'s pseudo-cursor rowid-alias
//! re-read (both the `*`/`table.*` expansion and a bare-column result
//! expression), `SELECT DISTINCT`'s dedup guard, and a non-contiguous
//! `compile_row_values` result -- exercised through
//! `engine::row::RowEngine` (`ORDER BY`/`DISTINCT` force the
//! post-sort/dedup pseudo-cursor re-read path this file's other tests
//! don't reach on a table with a rowid alias).
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

const FIXTURE: &str = "tests/fixtures/btrees/table_single_page.db";

struct TempDb(PathBuf);

impl TempDb {
    fn new(label: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "db-core-projection-{label}-{}-{}.db",
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
fn select_star_with_order_by_reads_the_rowid_alias_back_through_the_pseudo_cursor() {
    // `pt.id` is the rowid alias, stored as a NULL placeholder in the
    // record; `ORDER BY pt.v` forces the sorted path, so `SELECT *`'s
    // `id` column is re-read from the post-sort pseudo cursor rather
    // than the live table cursor `compile_row_values`'s non-sorted
    // branch would use.
    let db = TempDb::new("star-sorted-rowid");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE pt(id INTEGER PRIMARY KEY, v INTEGER); \
         INSERT INTO pt(v) VALUES (30); \
         INSERT INTO pt(v) VALUES (10); \
         INSERT INTO pt(v) VALUES (20)",
    )
    .unwrap();
    let rows = e.run_query("SELECT * FROM pt ORDER BY v").unwrap().rows;
    let pairs: Vec<(i64, i64)> = rows
        .iter()
        .map(|r| match (&r[0], &r[1]) {
            (Cell::Int(id), Cell::Int(v)) => (*id, *v),
            other => panic!("expected two Ints, got {other:?}"),
        })
        .collect();
    assert_eq!(pairs, vec![(2, 10), (3, 20), (1, 30)]);
}

#[test]
fn bare_rowid_alias_column_with_order_by_also_reads_through_the_pseudo_cursor() {
    // Same rowid-alias re-read, but through the `ResultColumnPlan::Expr`
    // bare-column arm (`SELECT id`) rather than `*`'s `Column` arm.
    let db = TempDb::new("bare-sorted-rowid");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE bt(id INTEGER PRIMARY KEY, v INTEGER); \
         INSERT INTO bt(v) VALUES (30); \
         INSERT INTO bt(v) VALUES (10)",
    )
    .unwrap();
    let rows = e.run_query("SELECT id FROM bt ORDER BY v").unwrap().rows;
    assert_eq!(ints(&rows), vec![2, 1]);
}

#[test]
fn select_distinct_on_a_single_table_dedups_matching_rows() {
    let db = TempDb::new("distinct-single");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE ds(a INTEGER); \
         INSERT INTO ds VALUES (1), (1), (2)",
    )
    .unwrap();
    let rows = e
        .run_query("SELECT DISTINCT a FROM ds ORDER BY a")
        .unwrap()
        .rows;
    assert_eq!(ints(&rows), vec![1, 2]);
}

#[test]
fn select_list_mixing_a_computed_expression_with_a_bare_column_is_not_naturally_contiguous() {
    // `coalesce(a, -1)` allocates temporaries before its own result
    // register lands, so its register and the following bare `a`
    // column's register aren't naturally adjacent -- exercises
    // `compile_row_values`'s copy-into-a-fresh-contiguous-run fallback.
    let db = TempDb::new("noncontig-projection");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE nc(a INTEGER); \
         INSERT INTO nc VALUES (5)",
    )
    .unwrap();
    let rows = e
        .run_query("SELECT coalesce(a, -1), a FROM nc")
        .unwrap()
        .rows;
    assert_eq!(rows, vec![vec![Cell::Int(5), Cell::Int(5)]]);
}
