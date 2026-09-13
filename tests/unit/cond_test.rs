// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `codegen::row::expr::cond`'s three-valued jump-mode condition
//! compilation, end to end through `engine::row::RowEngine` -- `IS`/
//! `IS NOT` with a NULL operand, `NOT BETWEEN`, `IN ()`'s always-false
//! empty-list case, `NOT IN` with a NULL in the list (unknown, not
//! true), and a cost-based `AND`/`OR` operand reorder -- none of which
//! had a test anywhere in the suite.
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
            "db-core-cond-{label}-{}-{}.db",
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

fn seeded(label: &str) -> (TempDb, RowEngine) {
    let db = TempDb::new(label);
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE ct(a INTEGER); \
         INSERT INTO ct VALUES (1), (2), (3), (NULL)",
    )
    .unwrap();
    (db, e)
}

#[test]
fn is_and_is_not_never_propagate_null_to_unknown() {
    let (_db, mut e) = seeded("is-not");
    // `a IS NULL` (not `= NULL`) matches the NULL row; `a IS NOT NULL`
    // matches every non-NULL row -- unlike `=`/`<>`, always definite.
    let rows = e
        .run_query("SELECT a FROM ct WHERE a IS NULL")
        .unwrap()
        .rows;
    assert_eq!(rows, vec![vec![Cell::Null]]);

    let rows = e
        .run_query("SELECT a FROM ct WHERE a IS NOT 2 ORDER BY a")
        .unwrap()
        .rows;
    assert_eq!(
        rows,
        vec![vec![Cell::Null], vec![Cell::Int(1)], vec![Cell::Int(3)]]
    );
}

#[test]
fn not_between_excludes_the_range_and_still_treats_null_as_unknown() {
    let (_db, mut e) = seeded("not-between");
    let rows = e
        .run_query("SELECT a FROM ct WHERE a NOT BETWEEN 1 AND 2 ORDER BY a")
        .unwrap()
        .rows;
    // The NULL row is excluded too -- NOT BETWEEN on a NULL operand is
    // unknown, not true, so WHERE drops it same as a false row would.
    assert_eq!(ints(&rows), vec![3]);
}

#[test]
fn in_with_an_empty_list_is_always_false_even_for_a_null_operand() {
    let (_db, mut e) = seeded("in-empty");
    let rows = e.run_query("SELECT a FROM ct WHERE a IN ()").unwrap().rows;
    assert!(rows.is_empty(), "{rows:?}");
}

#[test]
fn not_in_with_a_null_list_item_is_unknown_not_true() {
    let (_db, mut e) = seeded("not-in-null-item");
    // `3 NOT IN (1, NULL)`: no match found, but a NULL was seen along
    // the way, so the honest answer is unknown -- WHERE drops it, the
    // same as it would a definite false.
    let rows = e
        .run_query("SELECT a FROM ct WHERE a NOT IN (1, NULL) ORDER BY a")
        .unwrap()
        .rows;
    assert!(rows.is_empty(), "{rows:?}");
}

#[test]
fn in_with_a_null_list_item_can_still_definitely_match() {
    // A definite match is found before the list's NULL item is even
    // reached (`1 IN (1, NULL)`), so this is still true -- only a
    // *miss* through a NULL item is unknown.
    let (_db, mut e) = seeded("in-null-item-hit");
    let rows = e
        .run_query("SELECT a FROM ct WHERE a IN (1, NULL) ORDER BY a")
        .unwrap()
        .rows;
    assert_eq!(ints(&rows), vec![1]);
}

#[test]
fn bare_column_and_subquery_truthiness_in_boolean_context() {
    // A bare (non-comparison) column and a scalar subquery used
    // directly as a boolean condition both go through `finish_truthy`
    // rather than a dedicated comparison opcode.
    let db = TempDb::new("truthy");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE tr(a INTEGER); \
         CREATE TABLE flag(v INTEGER); \
         INSERT INTO tr VALUES (0), (1), (2); \
         INSERT INTO flag VALUES (1)",
    )
    .unwrap();
    let rows = e
        .run_query("SELECT a FROM tr WHERE a ORDER BY a")
        .unwrap()
        .rows;
    assert_eq!(ints(&rows), vec![1, 2]);

    let rows = e
        .run_query("SELECT a FROM tr WHERE (SELECT v FROM flag) ORDER BY a")
        .unwrap()
        .rows;
    assert_eq!(ints(&rows), vec![0, 1, 2]);
}

#[test]
fn or_with_a_bare_truthy_operand_routes_null_through_the_other_side() {
    // The first OR operand is a bare column (truthy, not a comparison),
    // compiled with `NullTarget::True` from the OR's own on_null
    // threading -- exercises finish_truthy's NullTarget::True arm,
    // not just its NullTarget::False one (the WHERE-clause default).
    let db = TempDb::new("or-truthy-null");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE ot(a INTEGER, b INTEGER); \
         INSERT INTO ot VALUES (NULL, 1); \
         INSERT INTO ot VALUES (NULL, 0); \
         INSERT INTO ot VALUES (1, 0)",
    )
    .unwrap();
    let rows = e
        .run_query("SELECT b FROM ot WHERE a OR b = 1 ORDER BY b")
        .unwrap()
        .rows;
    // Row (NULL, 0): unknown OR false = unknown, excluded. Row (NULL, 1):
    // unknown OR true = true, included. Row (1, 0): true OR false = true,
    // included.
    assert_eq!(ints(&rows), vec![0, 1]);
}

#[test]
fn and_or_reorder_the_cheaper_operand_first_but_answer_the_same() {
    // #581: `abs(a) > 0` (a function call, cost_class 2) is pricier
    // than the bare-column comparisons on the other side of AND/OR, so
    // the compiler reorders the cheap side first -- purely an
    // evaluation-order optimization, the answer must be identical to
    // the same predicate written the other way around.
    let (_db, mut e) = seeded("and-or-reorder");
    let rows = e
        .run_query("SELECT a FROM ct WHERE abs(a) > 0 AND a = 2")
        .unwrap()
        .rows;
    assert_eq!(ints(&rows), vec![2]);

    let rows = e
        .run_query("SELECT a FROM ct WHERE abs(a) > 100 OR a = 3")
        .unwrap()
        .rows;
    assert_eq!(ints(&rows), vec![3]);
}
