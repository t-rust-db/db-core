// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! #409: `GROUP BY`'s accumulator reset is factored the same way #396
//! factored the output epilogue -- a shared `AggReset`-per-slot body
//! reached via `Opcode::Gosub`/`BeginSubrtn`/`Return` from both the
//! pre-loop initialization site and the group-boundary site, not
//! inlined (or fused into `AggStep`'s own reset flag) at each site,
//! mirroring the two separate `Gosub`s the real `sqlite3` oracle emits
//! for its own accumulator reset (`select.c`, confirmed against
//! `sqlite3` 3.51.0 in #409's issue).
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
            "db-core-group-by-reset-subrtn-{label}-{}-{}.db",
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

/// A sorted (temp-b-tree) `GROUP BY` resets its accumulator once, in a
/// shared subroutine reached from both the pre-loop site and every
/// group-boundary site via `Gosub` -- not once per boundary, and not
/// fused into `AggStep` itself.
#[test]
fn sorted_group_by_resets_the_accumulator_via_one_shared_subroutine() {
    let db = TempDb::new("sorted");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE gbrst(a INTEGER, b INTEGER); \
         INSERT INTO gbrst(a, b) VALUES (1, 1), (3, 2), (5, 2), (7, 3)",
    )
    .unwrap();

    let rows = opcodes(&e, "SELECT b, count(*) FROM gbrst WHERE a > 2 GROUP BY b");

    assert_eq!(
        count(&rows, "AggReset"),
        1,
        "AggReset should appear once, in the shared reset subroutine: {rows:?}"
    );
}

/// The index-ordered `GROUP BY` fast path has the identical shape.
#[test]
fn index_ordered_group_by_resets_the_accumulator_via_one_shared_subroutine() {
    let db = TempDb::new("index-ordered");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE gbrst(id INTEGER PRIMARY KEY, b INTEGER); \
         CREATE INDEX gbrst_b ON gbrst(b); \
         INSERT INTO gbrst(id, b) VALUES (1, 1), (2, 2), (3, 2), (4, 3)",
    )
    .unwrap();

    let rows = opcodes(&e, "SELECT b, count(*) FROM gbrst GROUP BY b");

    assert_eq!(count(&rows, "AggReset"), 1, "{rows:?}");
}

/// Behavior must be unchanged by splitting reset out of `AggStep`'s
/// fused reset-and-fold into a separate `Gosub` + plain `AggStep` --
/// same grouped counts as before #409.
#[test]
fn grouped_query_results_are_unaffected_by_the_reset_subroutine_factoring() {
    let db = TempDb::new("results");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE gbrst(a INTEGER, b INTEGER); \
         INSERT INTO gbrst(a, b) VALUES (1, 1), (3, 2), (5, 2), (7, 3)",
    )
    .unwrap();

    let result = e.run_query("SELECT b, count(*) FROM gbrst WHERE a > 2 GROUP BY b");
    let rows = result.unwrap();
    let mut values: Vec<Vec<String>> = rows
        .rows
        .into_iter()
        .map(|row| row.into_iter().map(|v| v.to_string()).collect())
        .collect();
    values.sort();
    assert_eq!(values, vec![vec!["2", "2"], vec!["3", "1"]]);
}

/// The implicit whole-table group (no explicit `GROUP BY`, an aggregate
/// in the result list) over a zero-row table still finalizes to the
/// empty-group values (`count(*) = 0`) -- the pre-loop reset call must
/// not disturb that path.
#[test]
fn implicit_group_over_zero_rows_still_finalizes_empty() {
    let db = TempDb::new("implicit-empty");
    let mut e = open(&db);
    e.run_query("CREATE TABLE gbrst(a INTEGER)").unwrap();

    let result = e.run_query("SELECT count(*) FROM gbrst");
    let rows = result.unwrap();
    let values: Vec<Vec<String>> = rows
        .rows
        .into_iter()
        .map(|row| row.into_iter().map(|v| v.to_string()).collect())
        .collect();
    assert_eq!(values, vec![vec!["0"]]);
}
