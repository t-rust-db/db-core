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
