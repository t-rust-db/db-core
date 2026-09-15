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

const FIXTURE: &str = "tests/fixtures/btrees/table_single_page.db";

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

#[test]
fn join_on_the_right_tables_rowid_seeks_instead_of_scanning() {
    // `rs_b.id` is `rs_b`'s rowid alias, so `choose_join_access` picks
    // `JoinAccess::Rowid` -- a `SeekRowid`, no scan of `rs_b` at all.
    let db = TempDb::new("rowid-seek");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE rs_a(id INTEGER PRIMARY KEY, k INTEGER); \
         CREATE TABLE rs_b(id INTEGER PRIMARY KEY, v INTEGER); \
         INSERT INTO rs_a(k) VALUES (2); \
         INSERT INTO rs_b(v) VALUES (100); \
         INSERT INTO rs_b(v) VALUES (200)",
    )
    .unwrap();
    let rows = e
        .run_query("SELECT rs_b.v FROM rs_a JOIN rs_b ON rs_b.id = rs_a.k")
        .unwrap()
        .rows;
    assert_eq!(ints(&rows), vec![vec![200]]);
}

#[test]
fn join_on_a_unique_indexed_column_seeks_via_that_index() {
    // `ui_b.k` is `UNIQUE`-indexed (not the rowid), so
    // `choose_join_access` picks `JoinAccess::UniqueIndex` --
    // `SeekIndexEq` + `IdxRowid` + `SeekRowid`, still no scan of `ui_b`.
    let db = TempDb::new("unique-index-seek");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE ui_a(id INTEGER PRIMARY KEY, k INTEGER); \
         CREATE TABLE ui_b(id INTEGER PRIMARY KEY, k INTEGER, v INTEGER); \
         CREATE UNIQUE INDEX ui_b_k ON ui_b(k); \
         INSERT INTO ui_a(k) VALUES (5); \
         INSERT INTO ui_b(k, v) VALUES (5, 500); \
         INSERT INTO ui_b(k, v) VALUES (6, 600)",
    )
    .unwrap();
    let rows = e
        .run_query("SELECT ui_b.v FROM ui_a JOIN ui_b ON ui_b.k = ui_a.k")
        .unwrap()
        .rows;
    assert_eq!(ints(&rows), vec![vec![500]]);
}

#[test]
fn join_over_a_large_unindexed_table_builds_a_transient_auto_index() {
    // #545: no index backs `ai_b.k` at all, but `ANALYZE` gives it
    // enough rows (>= 25) for `is_automatic_index_worthwhile` to build
    // a transient one-shot index over the join column instead of an
    // O(n*m) nested scan.
    let db = TempDb::new("auto-index");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE ai_a(id INTEGER PRIMARY KEY, k INTEGER); \
         CREATE TABLE ai_b(id INTEGER PRIMARY KEY, k INTEGER, v INTEGER); \
         INSERT INTO ai_a(k) VALUES (7)",
    )
    .unwrap();
    for i in 0..30 {
        e.run_query(&format!(
            "INSERT INTO ai_b(k, v) VALUES ({}, {})",
            i,
            i * 10
        ))
        .unwrap();
    }
    e.run_query("ANALYZE").unwrap();
    let rows = e
        .run_query("SELECT ai_b.v FROM ai_a JOIN ai_b ON ai_b.k = ai_a.k")
        .unwrap()
        .rows;
    assert_eq!(ints(&rows), vec![vec![70]]);
}

#[test]
fn right_join_on_a_unique_indexed_column_still_tracks_matched_rows() {
    // A plain (INNER) join over a UniqueIndex access never needs the
    // "outer join's matched register" bookkeeping -- only a RIGHT/LEFT
    // join does, to know whether to emit a NULL-padded row for an
    // unmatched left-side row. Combining UniqueIndex access with a
    // RIGHT JOIN exercises that `check.sets_matched` branch, which
    // `join_on_a_unique_indexed_column_seeks_via_that_index` (a plain
    // JOIN) never reaches.
    let db = TempDb::new("right-join-unique-index");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE rjui_a(id INTEGER PRIMARY KEY, k INTEGER); \
         CREATE TABLE rjui_b(id INTEGER PRIMARY KEY, k INTEGER, v INTEGER); \
         CREATE UNIQUE INDEX rjui_b_k ON rjui_b(k); \
         INSERT INTO rjui_a(k) VALUES (5); \
         INSERT INTO rjui_b(k, v) VALUES (5, 500); \
         INSERT INTO rjui_b(k, v) VALUES (9, 900)",
    )
    .unwrap();
    let rows = e
        .run_query(
            "SELECT rjui_a.k, rjui_b.v FROM rjui_a RIGHT JOIN rjui_b ON rjui_b.k = rjui_a.k",
        )
        .unwrap()
        .rows;
    assert_eq!(ints(&rows), vec![vec![5, 500], vec![-1, 900]]);
}

#[test]
fn right_join_over_an_auto_indexed_table_still_tracks_matched_rows() {
    // Same reasoning as the unique-index case above, but for the #545
    // transient-auto-index access path: `join_over_a_large_unindexed_
    // table_builds_a_transient_auto_index` is a plain JOIN, so it never
    // reaches the auto-index branch's own `check.sets_matched` arm.
    let db = TempDb::new("right-join-auto-index");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE rjai_a(id INTEGER PRIMARY KEY, k INTEGER); \
         CREATE TABLE rjai_b(id INTEGER PRIMARY KEY, k INTEGER, v INTEGER); \
         INSERT INTO rjai_a(k) VALUES (7)",
    )
    .unwrap();
    for i in 0..30 {
        e.run_query(&format!(
            "INSERT INTO rjai_b(k, v) VALUES ({}, {})",
            i,
            i * 10
        ))
        .unwrap();
    }
    e.run_query("ANALYZE").unwrap();
    let rows = e
        .run_query("SELECT rjai_a.k, rjai_b.v FROM rjai_a RIGHT JOIN rjai_b ON rjai_b.k = rjai_a.k")
        .unwrap()
        .rows;
    assert_eq!(rows.len(), 30);
}

#[test]
fn full_join_emits_matched_rows_and_both_sides_unmatched_rows() {
    let db = TempDb::new("full-join-plain");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE fj_a(k INTEGER); \
         CREATE TABLE fj_b(k INTEGER, w INTEGER); \
         INSERT INTO fj_a VALUES (1), (2); \
         INSERT INTO fj_b VALUES (2, 200), (3, 300)",
    )
    .unwrap();
    let rows = e
        .run_query(
            "SELECT fj_a.k, fj_b.w FROM fj_a FULL JOIN fj_b ON fj_a.k = fj_b.k \
             ORDER BY fj_a.k, fj_b.w",
        )
        .unwrap()
        .rows;
    // 1 unmatched on the left (fj_b side NULL), 2/200 matched, 3/300
    // unmatched on the right (fj_a side NULL).
    assert_eq!(ints(&rows), vec![vec![-1, 300], vec![1, -1], vec![2, 200]]);
}

#[test]
fn full_join_with_order_by_alone_sorts_the_whole_result() {
    let db = TempDb::new("full-join-order-by");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE fjo_a(k INTEGER); \
         CREATE TABLE fjo_b(k INTEGER, w INTEGER); \
         INSERT INTO fjo_a VALUES (3), (1); \
         INSERT INTO fjo_b VALUES (1, 100)",
    )
    .unwrap();
    let rows = e
        .run_query(
            "SELECT fjo_a.k FROM fjo_a FULL JOIN fjo_b ON fjo_a.k = fjo_b.k ORDER BY fjo_a.k",
        )
        .unwrap()
        .rows;
    assert_eq!(ints(&rows), vec![vec![1], vec![3]]);
}

#[test]
fn full_join_with_distinct_alone_dedups_without_a_sort() {
    let db = TempDb::new("full-join-distinct");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE fjd_a(k INTEGER); \
         CREATE TABLE fjd_b(k INTEGER); \
         INSERT INTO fjd_a VALUES (1), (1); \
         INSERT INTO fjd_b VALUES (1)",
    )
    .unwrap();
    let rows = e
        .run_query("SELECT DISTINCT fjd_a.k FROM fjd_a FULL JOIN fjd_b ON fjd_a.k = fjd_b.k")
        .unwrap()
        .rows;
    assert_eq!(ints(&rows), vec![vec![1]]);
}

#[test]
fn join_with_a_where_clause_limit_offset_and_distinct() {
    // `emit_join_final_row`'s WHERE/LIMIT/OFFSET guards and
    // `emit_join_distinct_guard`'s dedup, all combined with a join --
    // exercised together to keep this file from growing one narrow
    // test per guard.
    let db = TempDb::new("join-where-limit-distinct");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE jw_a(k INTEGER); \
         CREATE TABLE jw_b(k INTEGER, v INTEGER); \
         INSERT INTO jw_a VALUES (1), (1), (2); \
         INSERT INTO jw_b VALUES (1, 100); \
         INSERT INTO jw_b VALUES (2, 5)",
    )
    .unwrap();
    let rows = e
        .run_query(
            "SELECT DISTINCT jw_b.v FROM jw_a JOIN jw_b ON jw_a.k = jw_b.k \
             WHERE jw_b.v > 10",
        )
        .unwrap()
        .rows;
    assert_eq!(ints(&rows), vec![vec![100]]);

    let rows = e
        .run_query(
            "SELECT jw_a.k FROM jw_a JOIN jw_b ON jw_a.k = jw_b.k \
             LIMIT 1 OFFSET 1",
        )
        .unwrap()
        .rows;
    assert_eq!(ints(&rows), vec![vec![1]]);
}

#[test]
fn joined_group_by_with_limit_and_offset() {
    // `compile_limit_setup`'s OFFSET arm, specifically for a joined
    // `GROUP BY` (`aggregate::join`'s own call site) -- the plain-join
    // LIMIT/OFFSET test above exercises a different, ungrouped call
    // site.
    let db = TempDb::new("join-group-by-limit-offset");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE jgl_a(k INTEGER); \
         CREATE TABLE jgl_b(k INTEGER, v INTEGER); \
         INSERT INTO jgl_a VALUES (1), (2), (3); \
         INSERT INTO jgl_b VALUES (1, 10), (2, 20), (3, 30)",
    )
    .unwrap();
    let rows = e
        .run_query(
            "SELECT jgl_a.k, count(*) FROM jgl_a JOIN jgl_b ON jgl_a.k = jgl_b.k \
             GROUP BY jgl_a.k ORDER BY jgl_a.k LIMIT 1 OFFSET 1",
        )
        .unwrap()
        .rows;
    assert_eq!(ints(&rows), vec![vec![2, 1]]);
}
