// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! #396: `GROUP BY`'s output epilogue (`AggFinal`/`MakeRecord`/
//! `OpenPseudo`/`ResultRow`) is compiled once and reached from both the
//! group-break site and the end-of-scan flush site via
//! `Opcode::Gosub`/`BeginSubrtn`/`Return`, not inlined twice.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test code fails fast (db-core#230)"
)]

use std::path::{Path, PathBuf};

use db_core::engine::row::RowEngine;
use db_core::engine::Engine;

const FIXTURE: &str = "tests/fixtures/btrees/table_single_page.db";

struct TempDb(PathBuf);

impl TempDb {
    fn new(label: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "db-core-group-by-subrtn-{label}-{}-{}.db",
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

fn opcodes(e: &RowEngine, sql: &str) -> Vec<(String, String)> {
    let sections = e
        .explain_opcodes(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err}"));
    sections
        .into_iter()
        .find(|s| s.label == "main")
        .expect("a main section")
        .rows
        .into_iter()
        .map(|r| (r.opcode, r.operands))
        .collect()
}

fn count(rows: &[(String, String)], opcode: &str) -> usize {
    rows.iter().filter(|(op, _)| op == opcode).count()
}

/// The issue's own repro: a sorted (temp-b-tree) `GROUP BY` currently
/// emits the group-output epilogue once at the boundary flush and once
/// more at the tail flush -- this asserts it now appears exactly once,
/// reached twice via `Gosub`.
#[test]
fn sorted_group_by_emits_the_output_epilogue_once() {
    let db = TempDb::new("sorted");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE gbst(a INTEGER, b INTEGER); \
         INSERT INTO gbst(a, b) VALUES (1, 1), (3, 2), (5, 2), (7, 3)",
    )
    .unwrap();

    let rows = opcodes(&e, "SELECT b, count(*) FROM gbst WHERE a > 2 GROUP BY b");

    assert_eq!(
        count(&rows, "AggFinal"),
        1,
        "AggFinal should appear once, in the shared subroutine: {rows:?}"
    );
    // db-core#409 added a second subroutine (accumulator reset), so
    // `BeginSubrtn`/`Gosub`/`Return` now count both subroutines'
    // entry/call/exit points, not just the output epilogue's --
    // `tests/unit/group_by_reset_subroutine_test.rs` asserts the reset
    // subroutine's own shape specifically.
    assert_eq!(
        count(&rows, "BeginSubrtn"),
        2,
        "both the output and reset subroutines should have exactly one entry point each: {rows:?}"
    );
    assert_eq!(
        count(&rows, "Gosub"),
        4,
        "boundary and tail flush call the output subroutine; pre-loop and boundary call the reset subroutine: {rows:?}"
    );
    assert_eq!(
        count(&rows, "Return"),
        2,
        "each subroutine should return exactly once: {rows:?}"
    );
}

/// The index-ordered `GROUP BY` fast path (`try_compile_index_ordered_group_by`)
/// has the identical boundary/tail duplication -- same assertion, an
/// indexed grouping column this time.
#[test]
fn index_ordered_group_by_emits_the_output_epilogue_once() {
    let db = TempDb::new("index-ordered");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE gbst(id INTEGER PRIMARY KEY, b INTEGER); \
         CREATE INDEX gbst_b ON gbst(b); \
         INSERT INTO gbst(id, b) VALUES (1, 1), (2, 2), (3, 2), (4, 3)",
    )
    .unwrap();

    let rows = opcodes(&e, "SELECT b, count(*) FROM gbst GROUP BY b");

    assert_eq!(count(&rows, "AggFinal"), 1, "{rows:?}");
    assert_eq!(count(&rows, "BeginSubrtn"), 2, "{rows:?}");
    assert_eq!(count(&rows, "Gosub"), 4, "{rows:?}");
    assert_eq!(count(&rows, "Return"), 2, "{rows:?}");
}

/// Behavior must be unchanged by the factoring -- same grouped counts,
/// same row values as before #396.
#[test]
fn grouped_query_results_are_unaffected_by_the_subroutine_factoring() {
    let db = TempDb::new("results");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE gbst(a INTEGER, b INTEGER); \
         INSERT INTO gbst(a, b) VALUES (1, 1), (3, 2), (5, 2), (7, 3)",
    )
    .unwrap();

    let result = e.run_query("SELECT b, count(*) FROM gbst WHERE a > 2 GROUP BY b");
    let rows = result.unwrap();
    let mut values: Vec<Vec<String>> = rows
        .rows
        .into_iter()
        .map(|row| row.into_iter().map(|v| v.to_string()).collect())
        .collect();
    values.sort();
    assert_eq!(values, vec![vec!["2", "2"], vec!["3", "1"]]);
}
