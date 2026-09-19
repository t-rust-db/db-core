// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Regression tests for #525: `try_compile_direct_agg_scan`'s implicit
//! whole-table-group `AggStep` fold no longer forces a `reset` on the
//! first matching row (the has-seen-a-row `have_group_reg` branch is only
//! emitted at all when the select list needs a plain-column snapshot).
//! These pin down the zero-row/non-zero-row aggregate semantics that
//! change relies on, specifically over an `IN (SELECT ...)` filter (the
//! ephemeral-index probe shape the issue is about) since that is the
//! shape most likely to regress silently if the fold logic is wrong.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test code fails fast (db-core#230)"
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
            "db-core-in-subquery-agg-{label}-{}-{}.db",
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

fn ints(rows: &[Vec<Cell>]) -> Vec<Vec<Option<i64>>> {
    rows.iter()
        .map(|r| {
            r.iter()
                .map(|c| match c {
                    Cell::Int(i) => Some(*i),
                    Cell::Null => None,
                    other => panic!("expected integer/null cell, got {other:?}"),
                })
                .collect()
        })
        .collect()
}

fn setup(engine: &mut RowEngine) {
    engine
        .run_query(
            "CREATE TABLE bench_data(bucket INTEGER); \
             CREATE TABLE bench_lookup(code INTEGER); \
             INSERT INTO bench_data VALUES (1), (2), (3), (4), (5); \
             INSERT INTO bench_lookup VALUES (2), (4)",
        )
        .unwrap();
}

#[test]
fn count_star_over_in_subquery_counts_only_matching_rows() {
    let db = TempDb::new("count-match");
    let mut engine = open(&db);
    setup(&mut engine);
    let r = engine
        .run_query(
            "SELECT count(*) FROM bench_data WHERE bucket IN (SELECT code FROM bench_lookup)",
        )
        .unwrap();
    assert_eq!(ints(&r.rows), vec![vec![Some(2)]]);
}

#[test]
fn count_star_over_in_subquery_with_no_matches_is_zero_not_null() {
    let db = TempDb::new("count-zero");
    let mut engine = open(&db);
    setup(&mut engine);
    let r = engine
        .run_query(
            "SELECT count(*) FROM bench_data \
             WHERE bucket IN (SELECT code FROM bench_lookup WHERE code > 100)",
        )
        .unwrap();
    assert_eq!(ints(&r.rows), vec![vec![Some(0)]]);
}

#[test]
fn sum_over_in_subquery_with_no_matches_finalizes_null() {
    let db = TempDb::new("sum-zero");
    let mut engine = open(&db);
    setup(&mut engine);
    let r = engine
        .run_query(
            "SELECT sum(bucket) FROM bench_data \
             WHERE bucket IN (SELECT code FROM bench_lookup WHERE code > 100)",
        )
        .unwrap();
    assert_eq!(ints(&r.rows), vec![vec![None]]);
}

#[test]
fn sum_over_in_subquery_matches_folds_every_matching_row() {
    let db = TempDb::new("sum-match");
    let mut engine = open(&db);
    setup(&mut engine);
    let r = engine
        .run_query(
            "SELECT sum(bucket) FROM bench_data WHERE bucket IN (SELECT code FROM bench_lookup)",
        )
        .unwrap();
    // bucket 2 + bucket 4 = 6
    assert_eq!(ints(&r.rows), vec![vec![Some(6)]]);
}

#[test]
fn min_and_count_over_in_subquery_snapshot_a_plain_column_from_the_first_match() {
    let db = TempDb::new("min-plain-col");
    let mut engine = open(&db);
    setup(&mut engine);
    // `min(bucket)` alongside `count(*)` still needs the aggregate-only
    // path (no plain, non-aggregate column is read), so this exercises
    // the same `needed.is_empty()` fold as count(*) alone but with two
    // aggregate slots folding on every matching row.
    let r = engine
        .run_query(
            "SELECT count(*), min(bucket), max(bucket) FROM bench_data \
             WHERE bucket IN (SELECT code FROM bench_lookup)",
        )
        .unwrap();
    assert_eq!(ints(&r.rows), vec![vec![Some(2), Some(2), Some(4)]]);
}

#[test]
fn count_over_in_subquery_matching_every_row_folds_across_the_whole_table() {
    let db = TempDb::new("count-all");
    let mut engine = open(&db);
    setup(&mut engine);
    let r = engine
        .run_query(
            "SELECT count(*) FROM bench_data \
             WHERE bucket IN (SELECT bucket FROM bench_data)",
        )
        .unwrap();
    assert_eq!(ints(&r.rows), vec![vec![Some(5)]]);
}
