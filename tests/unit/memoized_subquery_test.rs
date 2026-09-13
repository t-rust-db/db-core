// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! #314's per-probe-value memoization cache for a correlated scalar
//! subquery in a `WHERE`-clause comparison
//! (`codegen::row::subquery::memoize`), end to end through
//! `engine::row::RowEngine` -- had no coverage anywhere in the suite.
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
            "db-core-memoized-subquery-{label}-{}-{}.db",
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

/// `outer.k` is the single correlated column; `bound.v` is looked up
/// per distinct `k`, cached after the first probe of each value.
fn seeded(label: &str) -> (TempDb, RowEngine) {
    let db = TempDb::new(label);
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE outer_t(k INTEGER, tag TEXT); \
         CREATE TABLE bound(k INTEGER, v INTEGER); \
         INSERT INTO outer_t VALUES (1, 'a'); \
         INSERT INTO outer_t VALUES (1, 'b'); \
         INSERT INTO outer_t VALUES (2, 'c'); \
         INSERT INTO outer_t VALUES (3, 'd'); \
         INSERT INTO bound VALUES (1, 100); \
         INSERT INTO bound VALUES (2, 200)",
    )
    .unwrap();
    (db, e)
}

#[test]
fn repeated_outer_value_hits_the_cache_and_agrees_with_the_first_probe() {
    let (_db, mut e) = seeded("repeat");
    // k = 1 appears twice (rows tagged 'a' and 'b'): the second probe
    // of the same value must answer identically to the first, whether
    // served from cache or not.
    let rows = e
        .run_query(
            "SELECT tag FROM outer_t \
             WHERE 100 = (SELECT v FROM bound WHERE bound.k = outer_t.k) OR \
                   outer_t.k = (SELECT k FROM bound WHERE bound.v = 200) \
             ORDER BY tag",
        )
        .unwrap()
        .rows;
    assert_eq!(
        rows,
        vec![
            vec![Cell::Text("a".into())],
            vec![Cell::Text("b".into())],
            vec![Cell::Text("c".into())],
        ]
    );
}

#[test]
fn a_probe_value_absent_from_the_subquery_never_caches_a_false_match() {
    let (_db, mut e) = seeded("miss");
    // k = 3 has no row in `bound` -- the subquery returns NULL every
    // time, which must never compare equal to anything (including a
    // second NULL-probe row), so outer_t's k = 3 row is never selected.
    let rows = e
        .run_query(
            "SELECT tag FROM outer_t \
             WHERE outer_t.k = (SELECT k FROM bound WHERE bound.k = outer_t.k) \
             ORDER BY tag",
        )
        .unwrap()
        .rows;
    assert_eq!(
        rows,
        vec![
            vec![Cell::Text("a".into())],
            vec![Cell::Text("b".into())],
            vec![Cell::Text("c".into())],
        ]
    );
}

#[test]
fn correlation_detection_walks_between_in_like_and_case_inside_the_subquery() {
    // Each subquery below correlates its WHERE clause against
    // `outer_t.k` through a different `collect_correlated_column`
    // traversal arm (BETWEEN, IN, LIKE, CASE, a unary/paren wrapper),
    // not the plain `Binary` equality the other tests use.
    let (_db, mut e) = seeded("traversal-arms");
    let rows = e
        .run_query(
            "SELECT tag FROM outer_t \
             WHERE outer_t.k = (SELECT k FROM bound WHERE bound.k BETWEEN outer_t.k AND outer_t.k) \
             ORDER BY tag",
        )
        .unwrap()
        .rows;
    assert_eq!(
        rows,
        vec![
            vec![Cell::Text("a".into())],
            vec![Cell::Text("b".into())],
            vec![Cell::Text("c".into())],
        ]
    );

    let rows = e
        .run_query(
            "SELECT tag FROM outer_t \
             WHERE outer_t.k = (SELECT k FROM bound WHERE bound.k IN (outer_t.k)) \
             ORDER BY tag",
        )
        .unwrap()
        .rows;
    assert_eq!(
        rows,
        vec![
            vec![Cell::Text("a".into())],
            vec![Cell::Text("b".into())],
            vec![Cell::Text("c".into())],
        ]
    );

    let rows = e
        .run_query(
            "SELECT tag FROM outer_t \
             WHERE outer_t.k = (SELECT k FROM bound WHERE CAST(bound.k AS INTEGER) = -(-outer_t.k)) \
             ORDER BY tag",
        )
        .unwrap()
        .rows;
    assert_eq!(
        rows,
        vec![
            vec![Cell::Text("a".into())],
            vec![Cell::Text("b".into())],
            vec![Cell::Text("c".into())],
        ]
    );

    let rows = e
        .run_query(
            "SELECT tag FROM outer_t \
             WHERE outer_t.k = (SELECT k FROM bound WHERE \
                CASE WHEN bound.k = outer_t.k THEN 1 ELSE 0 END = 1) \
             ORDER BY tag",
        )
        .unwrap()
        .rows;
    assert_eq!(
        rows,
        vec![
            vec![Cell::Text("a".into())],
            vec![Cell::Text("b".into())],
            vec![Cell::Text("c".into())],
        ]
    );
}

#[test]
fn correlation_detection_walks_like_with_escape_and_the_select_list() {
    let (_db, mut e) = seeded("like-and-select-list");
    // LIKE (with ESCAPE) traversal arm.
    let rows = e
        .run_query(
            "SELECT tag FROM outer_t \
             WHERE outer_t.k = (SELECT k FROM bound WHERE CAST(bound.k AS TEXT) LIKE \
                CAST(outer_t.k AS TEXT) ESCAPE '\\') \
             ORDER BY tag",
        )
        .unwrap()
        .rows;
    assert_eq!(
        rows,
        vec![
            vec![Cell::Text("a".into())],
            vec![Cell::Text("b".into())],
            vec![Cell::Text("c".into())],
        ]
    );

    // The correlated column appears only in the subquery's own SELECT
    // list, not its WHERE clause -- `single_correlated_outer_column`
    // walks both.
    let rows = e
        .run_query(
            "SELECT tag FROM outer_t \
             WHERE outer_t.k = (SELECT outer_t.k FROM bound WHERE bound.k = 1) \
             ORDER BY tag",
        )
        .unwrap()
        .rows;
    // The subquery returns `outer_t.k` itself (correlated via the
    // SELECT list, not WHERE), so `outer_t.k = <that same value>` is
    // trivially true for every row -- proof the correlation was found
    // and threaded through correctly, not proof of a filtering effect.
    assert_eq!(
        rows,
        vec![
            vec![Cell::Text("a".into())],
            vec![Cell::Text("b".into())],
            vec![Cell::Text("c".into())],
            vec![Cell::Text("d".into())],
        ]
    );
}

#[test]
fn a_nested_exists_inside_the_correlated_subquery_disables_memoization_but_stays_correct() {
    // `collect_correlated_column` treats a nested Subquery/EXISTS/
    // InSubquery as unreasonable-about (`ambiguous = true`), so this
    // subquery is never memoized -- it still has to answer correctly
    // via the ordinary per-row `compile_scalar_subquery` path.
    let (_db, mut e) = seeded("nested-exists");
    let rows = e
        .run_query(
            "SELECT tag FROM outer_t \
             WHERE outer_t.k = (SELECT k FROM bound WHERE bound.k = outer_t.k \
                AND EXISTS (SELECT 1 FROM bound WHERE bound.k = 1)) \
             ORDER BY tag",
        )
        .unwrap()
        .rows;
    assert_eq!(
        rows,
        vec![
            vec![Cell::Text("a".into())],
            vec![Cell::Text("b".into())],
            vec![Cell::Text("c".into())],
        ]
    );
}

#[test]
fn a_subquery_correlated_against_two_distinct_outer_columns_is_not_memoized_but_still_correct() {
    // `collect_correlated_column` sets `ambiguous` on a *second*
    // distinct outer column -- `subquery_memoizable` then returns
    // `None`, so this falls back to `compile_scalar_subquery`'s
    // ordinary per-row path. Still has to answer correctly.
    let db = TempDb::new("two-outer-cols");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE tc_outer(k INTEGER, j INTEGER); \
         CREATE TABLE tc_bound(k INTEGER, j INTEGER, v INTEGER); \
         INSERT INTO tc_outer VALUES (1, 2); \
         INSERT INTO tc_bound VALUES (1, 2, 100)",
    )
    .unwrap();
    let rows = e
        .run_query(
            "SELECT tc_outer.k FROM tc_outer \
             WHERE 100 = (SELECT v FROM tc_bound WHERE tc_bound.k = tc_outer.k AND tc_bound.j = tc_outer.j)",
        )
        .unwrap()
        .rows;
    assert_eq!(rows, vec![vec![Cell::Int(1)]]);
}

#[test]
fn memoized_subquery_result_is_correct_across_many_repeated_probes() {
    // A larger repeat count than the two-row case above, to exercise
    // more than a single cache hit per distinct value.
    let db = TempDb::new("many-repeats");
    let mut e = open(&db);
    e.run_query(
        "CREATE TABLE mr_outer(k INTEGER); \
         CREATE TABLE mr_bound(k INTEGER, v INTEGER); \
         INSERT INTO mr_bound VALUES (1, 10), (2, 20)",
    )
    .unwrap();
    for _ in 0..10 {
        e.run_query("INSERT INTO mr_outer VALUES (1), (2)").unwrap();
    }
    let rows = e
        .run_query(
            "SELECT k FROM mr_outer \
             WHERE (SELECT v FROM mr_bound WHERE mr_bound.k = mr_outer.k) > 15",
        )
        .unwrap()
        .rows;
    assert_eq!(ints(&rows), vec![2; 10]);
}
