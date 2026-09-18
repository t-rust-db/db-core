// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `EXPLAIN QUERY PLAN` (`codegen::row::select::eqp`), end to end
//! through `engine::row::RowEngine` -- joins (plain, aliased,
//! multi-table), a `UNION` compound, a scalar subquery, a `GROUP BY`
//! (both index-ordered and temp-b-tree), `SELECT DISTINCT`, and the
//! `#545` automatic-covering-index join wording, none of which had a
//! test anywhere in the suite before.
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
use db_core::engine::Engine;

const FIXTURE: &str = "tests/fixtures/btrees/table_single_page.db";

struct TempDb(PathBuf);

impl TempDb {
    fn new(label: &str) -> Self {
        Self::from_fixture(FIXTURE, label)
    }

    fn from_fixture(fixture: &str, label: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "db-core-eqp-{label}-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::copy(fixture, &path).expect("copy fixture");
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

fn plan(e: &RowEngine, sql: &str) -> Vec<String> {
    e.explain_plan(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err}"))
        .into_iter()
        .map(|r| r.detail)
        .collect()
}

fn seeded(label: &str) -> (TempDb, RowEngine) {
    let db = TempDb::new(label);
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE eq_a(id INTEGER PRIMARY KEY, k INTEGER, v INTEGER); \
         CREATE INDEX eq_a_k ON eq_a(k); \
         CREATE TABLE eq_b(id INTEGER PRIMARY KEY, k INTEGER, w INTEGER); \
         CREATE UNIQUE INDEX eq_b_k ON eq_b(k); \
         CREATE TABLE eq_c(id INTEGER PRIMARY KEY, k INTEGER); \
         INSERT INTO eq_a(k, v) VALUES (1, 10); \
         INSERT INTO eq_b(k, w) VALUES (1, 100)",
    )
    .unwrap();
    (db, e)
}

#[test]
fn join_reports_the_right_tables_unique_index_seek() {
    let (_db, e) = seeded("join-seek");
    assert_eq!(
        plan(&e, "SELECT * FROM eq_a JOIN eq_b ON eq_a.k = eq_b.k"),
        vec!["SCAN eq_a", "SEARCH eq_b USING INDEX eq_b_k (k=?)"]
    );
}

#[test]
fn join_with_a_table_alias_reports_the_alias() {
    let (_db, e) = seeded("join-alias");
    assert_eq!(
        plan(&e, "SELECT * FROM eq_a a1 JOIN eq_b b1 ON a1.k = b1.k"),
        vec![
            "SCAN eq_a AS a1",
            "SEARCH eq_b AS b1 USING INDEX eq_b_k (k=?)"
        ]
    );
}

#[test]
fn three_table_join_reports_one_row_per_table() {
    let (_db, e) = seeded("three-table");
    assert_eq!(
        plan(
            &e,
            "SELECT * FROM eq_a, eq_b, eq_c \
             WHERE eq_a.k = eq_b.k AND eq_b.k = eq_c.k"
        ),
        vec!["SCAN eq_a", "SCAN eq_b", "SCAN eq_c"]
    );
}

#[test]
fn left_right_and_full_join_report_from_clause_order() {
    // #250 note in the module doc: EQP reports FROM-clause order, not
    // the internal RIGHT-JOIN execution reordering.
    let (_db, e) = seeded("outer-joins");
    for op in ["LEFT", "RIGHT", "FULL"] {
        let sql = format!("SELECT * FROM eq_a {op} JOIN eq_b ON eq_a.k = eq_b.k");
        assert_eq!(
            plan(&e, &sql),
            vec!["SCAN eq_a", "SEARCH eq_b USING INDEX eq_b_k (k=?)"],
            "{op}"
        );
    }
}

#[test]
fn union_compound_reports_a_nested_plan_per_arm() {
    let (_db, e) = seeded("union-plan");
    assert_eq!(
        plan(&e, "SELECT * FROM eq_a UNION SELECT * FROM eq_a"),
        vec![
            "COMPOUND QUERY",
            "LEFT-MOST SUBQUERY",
            "SCAN eq_a",
            "UNION USING TEMP B-TREE",
            "SCAN eq_a",
        ]
    );
}

#[test]
fn scalar_subquery_in_where_reports_a_nested_scalar_subquery_plan() {
    let (_db, e) = seeded("scalar-subquery-plan");
    assert_eq!(
        plan(&e, "SELECT * FROM eq_a WHERE k = (SELECT k FROM eq_b)"),
        vec![
            "SEARCH eq_a USING INDEX eq_a_k (k=?)",
            "SCALAR SUBQUERY 1",
            "SCAN eq_b",
        ]
    );
}

#[test]
fn group_by_on_an_indexed_column_walks_the_index_instead_of_a_temp_b_tree() {
    let (_db, e) = seeded("group-by-indexed");
    assert_eq!(
        plan(&e, "SELECT k, count(*) FROM eq_a GROUP BY k"),
        vec!["SCAN eq_a USING INDEX eq_a_k"]
    );
}

#[test]
fn group_by_on_an_unindexed_column_uses_a_temp_b_tree() {
    let (_db, e) = seeded("group-by-temp-btree");
    assert_eq!(
        plan(&e, "SELECT v, count(*) FROM eq_a GROUP BY v"),
        vec!["SCAN eq_a", "USE TEMP B-TREE FOR GROUP BY"]
    );
}

#[test]
fn select_distinct_reports_a_plain_scan() {
    let (_db, e) = seeded("distinct-plan");
    assert_eq!(plan(&e, "SELECT DISTINCT k FROM eq_a"), vec!["SCAN eq_a"]);
}

#[test]
fn join_on_a_large_unindexed_table_reports_the_automatic_covering_index() {
    let db = TempDb::new("auto-index-plan");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE ai_a(id INTEGER PRIMARY KEY, k INTEGER); \
         CREATE TABLE ai_b(id INTEGER PRIMARY KEY, k INTEGER, v INTEGER); \
         INSERT INTO ai_a(k) VALUES (7)",
    )
    .unwrap();
    for i in 0..30 {
        e.run_query(&format!("INSERT INTO ai_b(k, v) VALUES ({i}, {})", i * 10))
            .unwrap();
    }
    e.run_query("ANALYZE").unwrap();
    assert_eq!(
        plan(&e, "SELECT ai_b.v FROM ai_a JOIN ai_b ON ai_b.k = ai_a.k"),
        vec![
            "SCAN ai_a",
            "SEARCH ai_b USING AUTOMATIC COVERING INDEX (k=?)"
        ]
    );
}

// ---------------------------------------------------------------------
// #498: `sqlite_stat4` histograms decide seek versus scan for a range
// predicate exactly as sqlite3 3.53.4 does. `stat4_range.db` is
// `t(id INTEGER PRIMARY KEY, a INTEGER, b INTEGER, s TEXT)` with `ia(a)`,
// 4096 rows (`a = id`), analyzed by sqlite3 itself (24 stat4 samples).
// Every expectation below is the oracle's own EXPLAIN QUERY PLAN line.
// ---------------------------------------------------------------------

const STAT4_FIXTURE: &str = "tests/fixtures/btrees/stat4_range.db";

fn stat4_engine(label: &str) -> (TempDb, RowEngine) {
    let db = TempDb::from_fixture(STAT4_FIXTURE, label);
    let e = open(&db);
    (db, e)
}

#[test]
fn stat4_wide_range_over_a_non_covering_index_scans_like_sqlite3() {
    let (_db, e) = stat4_engine("stat4-wide");
    assert_eq!(
        plan(&e, "SELECT sum(b) FROM t WHERE a BETWEEN 10 AND 3900"),
        ["SCAN t"]
    );
    assert_eq!(plan(&e, "SELECT sum(b) FROM t WHERE a > 100"), ["SCAN t"]);
    assert_eq!(
        plan(&e, "SELECT b FROM t WHERE a BETWEEN 10 AND 3900"),
        ["SCAN t"]
    );
}

#[test]
fn stat4_narrow_range_still_seeks_like_sqlite3() {
    let (_db, e) = stat4_engine("stat4-narrow");
    assert_eq!(
        plan(&e, "SELECT sum(b) FROM t WHERE a BETWEEN 10 AND 20"),
        ["SEARCH t USING INDEX ia (a>? AND a<?)"]
    );
    assert_eq!(
        plan(&e, "SELECT sum(b) FROM t WHERE a > 4000"),
        ["SEARCH t USING INDEX ia (a>?)"]
    );
    // 1000..3000 keeps about half the rows and still seeks: the oracle's
    // cost model puts it at 132 against a scan at 134.
    assert_eq!(
        plan(&e, "SELECT sum(b) FROM t WHERE a BETWEEN 1000 AND 3000"),
        ["SEARCH t USING INDEX ia (a>? AND a<?)"]
    );
    assert_eq!(
        plan(&e, "SELECT b FROM t WHERE a > 4000"),
        ["SEARCH t USING INDEX ia (a>?)"]
    );
}

#[test]
fn stat4_cannot_see_through_a_subquery_bound_so_the_seek_stays() {
    let (_db, e) = stat4_engine("stat4-subquery");
    let d = plan(
        &e,
        "SELECT sum(b) FROM t WHERE a BETWEEN 10 AND (SELECT max(a) FROM t)",
    );
    assert_eq!(d[0], "SEARCH t USING INDEX ia (a>? AND a<?)", "{d:?}");
}

#[test]
fn stat4_covering_walk_is_never_demoted() {
    let (_db, e) = stat4_engine("stat4-covering");
    assert_eq!(
        plan(&e, "SELECT count(*) FROM t WHERE a BETWEEN 10 AND 3900"),
        ["SEARCH t USING COVERING INDEX ia (a>? AND a<?)"]
    );
}

#[test]
fn stat4_removed_restores_the_stats_free_seek() {
    let (_db, mut e) = stat4_engine("stat4-removed");
    e.run_query("DELETE FROM sqlite_stat4").unwrap();
    // `stat1` alone never demotes a range seek (sqlite3's fixed 1/64
    // default for a closed range), so this is the pre-#498 plan again.
    assert_eq!(
        plan(&e, "SELECT sum(b) FROM t WHERE a BETWEEN 10 AND 3900"),
        ["SEARCH t USING INDEX ia (a>? AND a<?)"]
    );
}
