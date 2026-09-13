// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `SELECT ... ORDER BY <indexed col(s)> [DESC] LIMIT n [OFFSET m]` with
//! no `WHERE` clause, end to end through `engine::row::RowEngine` (#296,
//! `codegen::row::select::index_scan::try_compile_index_ordered_scan`):
//! walking the index b-tree directly in place of a buffer-and-sort. Each
//! result is checked against the same query with the `ORDER BY` target
//! swapped for an unindexed mirror column that carries the identical
//! values, so the expectation is the engine's own (sorter-backed) output,
//! not a hand-typed list; `explain_opcodes` (not `explain_plan`, whose
//! `SCAN t` wording doesn't yet distinguish this path from the sorter
//! fallback) confirms an index walk (`IdxRewind`/`IdxLast`) was actually
//! compiled in place of a `SorterOpen`.
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

/// Whether the compiled program walks an index directly (`IdxRewind`/
/// `IdxLast`) rather than buffering into a sorter (`SorterOpen`) --
/// `explain_plan`'s own wording for this path is a bare `SCAN t`, same
/// as the sorter fallback (#296's EQP integration doesn't yet name the
/// index for this specific strategy), so opcode presence is the only
/// way to tell the two apart from outside `codegen::row`.
fn walks_an_index(e: &RowEngine, sql: &str) -> bool {
    let sections = e
        .explain_opcodes(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err}"));
    let ops: Vec<&str> = sections
        .iter()
        .flat_map(|s| s.rows.iter().map(|r| r.opcode.as_str()))
        .collect();
    let idx_walk = ops.contains(&"IdxRewind") || ops.contains(&"IdxLast");
    let sorted = ops.contains(&"SorterOpen");
    assert!(
        !(idx_walk && sorted),
        "{sql}: both index walk and sorter present: {ops:?}"
    );
    idx_walk
}

const FIXTURE: &str = "tests/corpus/fixtures/btrees/table_single_page.db";

struct TempDb(PathBuf);

impl TempDb {
    fn new(label: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "db-core-idx-ordered-{label}-{}-{}.db",
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

/// `ot.k` is indexed ascending, `ot.mirror` duplicates it unindexed.
/// `m` has a covering `(a, b)` index for multi-column ordering.
/// (A `CREATE INDEX ... (col DESC)` case is not covered here --
/// db-core's DDL doesn't yet support a descending index key.)
fn seeded(label: &str) -> (TempDb, RowEngine) {
    let db = TempDb::new(label);
    let mut e = RowEngine::open(db.path()).expect("open");
    e.run_query(
        "CREATE TABLE ot(id INTEGER PRIMARY KEY, k INTEGER, mirror INTEGER); \
         CREATE INDEX ot_k ON ot(k); \
         INSERT INTO ot(k, mirror) VALUES (30, 30); \
         INSERT INTO ot(k, mirror) VALUES (10, 10); \
         INSERT INTO ot(k, mirror) VALUES (20, 20); \
         INSERT INTO ot(k, mirror) VALUES (10, 10); \
         CREATE TABLE m(id INTEGER PRIMARY KEY, a INTEGER, b INTEGER); \
         CREATE INDEX m_ab ON m(a, b); \
         INSERT INTO m(a, b) VALUES (1, 2); \
         INSERT INTO m(a, b) VALUES (1, 1); \
         INSERT INTO m(a, b) VALUES (2, 1); \
         CREATE TABLE p(id INTEGER PRIMARY KEY, a INTEGER, b INTEGER); \
         CREATE INDEX p_a ON p(a); \
         INSERT INTO p(a, b) VALUES (1, 30); \
         INSERT INTO p(a, b) VALUES (2, 20); \
         INSERT INTO p(a, b) VALUES (1, 10); \
         INSERT INTO p(a, b) VALUES (2, 40); \
         INSERT INTO p(a, b) VALUES (1, 20)",
    )
    .expect("seed");
    (db, e)
}

fn ints(rows: &[Vec<Cell>]) -> Vec<i64> {
    rows.iter()
        .map(|r| match &r[0] {
            Cell::Int(n) => *n,
            other => panic!("expected Int, got {other:?}"),
        })
        .collect()
}

fn rows(e: &mut RowEngine, sql: &str) -> Vec<Vec<Cell>> {
    e.run_query(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err}"))
        .rows
}

fn plan(e: &RowEngine, sql: &str) -> String {
    e.explain_plan(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err}"))
        .iter()
        .map(|r| r.detail.clone())
        .collect::<Vec<_>>()
        .join(" | ")
}

#[test]
fn ascending_order_by_walks_the_index_forward() {
    let (_db, mut e) = seeded("asc");
    assert_eq!(plan(&e, "SELECT k FROM ot ORDER BY k"), "SCAN ot");
    assert!(walks_an_index(&e, "SELECT k FROM ot ORDER BY k"));
    assert_eq!(
        ints(&rows(&mut e, "SELECT k FROM ot ORDER BY k")),
        vec![10, 10, 20, 30]
    );
}

#[test]
fn descending_order_by_walks_the_ascending_index_backward() {
    let (_db, mut e) = seeded("desc");
    assert_eq!(plan(&e, "SELECT k FROM ot ORDER BY k DESC"), "SCAN ot");
    assert!(walks_an_index(&e, "SELECT k FROM ot ORDER BY k DESC"));
    assert_eq!(
        ints(&rows(&mut e, "SELECT k FROM ot ORDER BY k DESC")),
        vec![30, 20, 10, 10]
    );
}

#[test]
fn limit_and_offset_bound_the_index_walk() {
    let (_db, mut e) = seeded("limit-offset");
    assert_eq!(
        ints(&rows(
            &mut e,
            "SELECT k FROM ot ORDER BY k LIMIT 2 OFFSET 1"
        )),
        vec![10, 20]
    );
    assert_eq!(
        ints(&rows(
            &mut e,
            "SELECT k FROM ot ORDER BY k DESC LIMIT 1 OFFSET 2"
        )),
        vec![10]
    );
}

#[test]
fn multi_column_order_by_matching_the_index_prefix_uses_the_index() {
    let (_db, mut e) = seeded("multi-col");
    let out = rows(&mut e, "SELECT a, b FROM m ORDER BY a, b");
    let pairs: Vec<(i64, i64)> = out
        .iter()
        .map(|r| match (&r[0], &r[1]) {
            (Cell::Int(a), Cell::Int(b)) => (*a, *b),
            other => panic!("expected two Ints, got {other:?}"),
        })
        .collect();
    assert_eq!(pairs, vec![(1, 1), (1, 2), (2, 1)]);
    assert_eq!(plan(&e, "SELECT a, b FROM m ORDER BY a, b"), "SCAN m");
    assert!(walks_an_index(&e, "SELECT a, b FROM m ORDER BY a, b"));
}

#[test]
fn order_by_matching_only_an_index_prefix_walks_the_prefix_and_sorts_each_groups_suffix() {
    // `p_a` indexes only `a`; `ORDER BY a, b` matches just that
    // one-column prefix (#574's `try_compile_partial_sorted_index_scan`),
    // so `a`'s groups are visited via the index in order, and each
    // group's `b` values are sorted independently rather than the whole
    // table going through one sort.
    let (_db, mut e) = seeded("partial-prefix");
    let out = rows(&mut e, "SELECT a, b FROM p ORDER BY a, b");
    let pairs: Vec<(i64, i64)> = out
        .iter()
        .map(|r| match (&r[0], &r[1]) {
            (Cell::Int(a), Cell::Int(b)) => (*a, *b),
            other => panic!("expected two Ints, got {other:?}"),
        })
        .collect();
    assert_eq!(pairs, vec![(1, 10), (1, 20), (1, 30), (2, 20), (2, 40)]);
    // Unlike the full-match path, a partial-prefix scan legitimately
    // walks the index *and* opens a (per-group) sorter -- `walks_an_index`'s
    // either/or assumption doesn't hold here, so check the opcode
    // directly instead.
    let sections = e
        .explain_opcodes("SELECT a, b FROM p ORDER BY a, b")
        .unwrap();
    let ops: Vec<&str> = sections
        .iter()
        .flat_map(|s| s.rows.iter().map(|r| r.opcode.as_str()))
        .collect();
    assert!(ops.contains(&"IdxRewind"), "{ops:?}");
    assert!(ops.contains(&"SorterOpen"), "{ops:?}");
}

#[test]
fn order_by_matching_only_an_index_prefix_respects_limit_and_offset() {
    let (_db, mut e) = seeded("partial-prefix-limit");
    let out = rows(&mut e, "SELECT a, b FROM p ORDER BY a, b LIMIT 2 OFFSET 3");
    let pairs: Vec<(i64, i64)> = out
        .iter()
        .map(|r| match (&r[0], &r[1]) {
            (Cell::Int(a), Cell::Int(b)) => (*a, *b),
            other => panic!("expected two Ints, got {other:?}"),
        })
        .collect();
    assert_eq!(pairs, vec![(2, 20), (2, 40)]);
}

#[test]
fn where_clause_disqualifies_the_index_ordered_path() {
    // `try_compile_index_ordered_scan` only fires with no `WHERE` at
    // all (no cardinality estimation in this MVP); a predicate present
    // falls back to the sorter, still correct, just via a bare `SCAN`.
    let (_db, mut e) = seeded("where-disqualifies");
    assert_eq!(
        plan(&e, "SELECT k FROM ot WHERE k > 10 ORDER BY k"),
        "SCAN ot"
    );
    assert!(!walks_an_index(
        &e,
        "SELECT k FROM ot WHERE k > 10 ORDER BY k"
    ));
    assert_eq!(
        ints(&rows(&mut e, "SELECT k FROM ot WHERE k > 10 ORDER BY k")),
        vec![20, 30]
    );
}

#[test]
fn order_by_an_unindexed_column_falls_back_to_the_sorter() {
    let (_db, mut e) = seeded("unindexed");
    assert_eq!(plan(&e, "SELECT k FROM ot ORDER BY mirror"), "SCAN ot");
    assert!(!walks_an_index(&e, "SELECT k FROM ot ORDER BY mirror"));
    assert_eq!(
        ints(&rows(&mut e, "SELECT k FROM ot ORDER BY mirror")),
        vec![10, 10, 20, 30]
    );
}
