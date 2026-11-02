// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! #937: a subquery expression in a JOIN query may read a table that is
//! not one of the joined tables. Before the fix the join path resolved
//! the subquery's own FROM against the joined tables only ("isn't
//! visible to this compiler's catalog"). Expected rows are sqlite3
//! 3.54.1's on the same data.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test code fails fast (db-core#230)"
)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use db_core::engine::row::{CreateOptions, RowEngine};
use db_core::engine::{Cell, Engine};

static NEXT: AtomicU32 = AtomicU32::new(0);

const SCHEMA: &str = "CREATE TABLE a (id INTEGER PRIMARY KEY, k TEXT NOT NULL); \
                      CREATE TABLE b (k TEXT NOT NULL, t TEXT NOT NULL); \
                      CREATE TABLE c (id INTEGER NOT NULL, t TEXT NOT NULL)";

struct TempDb(PathBuf);

impl Drop for TempDb {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
        std::fs::remove_file(format!("{}-journal", self.0.display())).ok();
    }
}

fn engine(rows: &[&str]) -> (TempDb, RowEngine) {
    let path = std::env::temp_dir().join(format!(
        "db-core-join-subquery-{}-{}.db",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut e = RowEngine::create(&path, CreateOptions::default(), SCHEMA).unwrap();
    for sql in rows {
        e.run_query(sql).unwrap();
    }
    (TempDb(path), e)
}

/// a(1,x) a(2,y) a(3,z); b(x,icf) b(x,mcqf) b(y,icf) b(w,orphan);
/// c(1,icf) c(2,other).
fn fixture() -> (TempDb, RowEngine) {
    engine(&[
        "INSERT INTO a VALUES (1,'x'),(2,'y'),(3,'z')",
        "INSERT INTO b VALUES ('x','icf'),('x','mcqf'),('y','icf'),('w','orphan')",
        "INSERT INTO c VALUES (1,'icf'),(2,'other')",
    ])
}

fn cell(c: &Cell) -> String {
    match c {
        Cell::Null => "NULL".to_string(),
        Cell::Text(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The rows as `a|b` strings, sorted unless `ordered`.
fn rows(e: &mut RowEngine, sql: &str, ordered: bool) -> Vec<String> {
    let result = e
        .run_query(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err}"));
    let mut out: Vec<String> = result
        .rows
        .iter()
        .map(|r| r.iter().map(cell).collect::<Vec<_>>().join("|"))
        .collect();
    if !ordered {
        out.sort();
    }
    out
}

#[test]
fn the_issue_query_returns_sqlites_row() {
    let (db, e) = engine(&[
        "INSERT INTO a VALUES (1,'x')",
        "INSERT INTO b VALUES ('x','icf'),('x','mcqf')",
        "INSERT INTO c VALUES (1,'icf')",
    ]);
    let sql = "SELECT a.id, b.t FROM a JOIN b ON b.k = a.k \
               WHERE NOT EXISTS (SELECT 1 FROM c WHERE c.id = a.id AND c.t = b.t)";
    drop(e);
    // The issue saw the failure after a reopen too.
    let mut e = RowEngine::open(&db.0).unwrap();
    assert_eq!(rows(&mut e, sql, false), ["1|mcqf"]);
}

#[test]
fn not_exists_over_a_table_outside_the_join() {
    let (_db, mut e) = fixture();
    let sql = "SELECT a.id, b.t FROM a JOIN b ON b.k = a.k \
               WHERE NOT EXISTS (SELECT 1 FROM c WHERE c.id = a.id AND c.t = b.t)";
    assert_eq!(rows(&mut e, sql, false), ["1|mcqf", "2|icf"]);
}

#[test]
fn exists_over_a_table_outside_the_join() {
    let (_db, mut e) = fixture();
    let sql = "SELECT a.id, b.t FROM a JOIN b ON b.k = a.k \
               WHERE EXISTS (SELECT 1 FROM c WHERE c.id = a.id AND c.t = b.t)";
    assert_eq!(rows(&mut e, sql, false), ["1|icf"]);
}

#[test]
fn scalar_subquery_in_the_select_list_over_a_table_outside_the_join() {
    let (_db, mut e) = fixture();
    let sql = "SELECT a.id, b.t, (SELECT count(*) FROM c WHERE c.id = a.id) \
               FROM a JOIN b ON b.k = a.k";
    assert_eq!(rows(&mut e, sql, false), ["1|icf|1", "1|mcqf|1", "2|icf|1"]);
}

#[test]
fn in_and_not_in_over_a_table_outside_the_join() {
    let (_db, mut e) = fixture();
    let correlated = "SELECT a.id, b.t FROM a JOIN b ON b.k = a.k \
                      WHERE b.t IN (SELECT t FROM c WHERE c.id = a.id)";
    assert_eq!(rows(&mut e, correlated, false), ["1|icf"]);
    let uncorrelated = "SELECT a.id, b.t FROM a JOIN b ON b.k = a.k \
                        WHERE b.t NOT IN (SELECT t FROM c)";
    assert_eq!(rows(&mut e, uncorrelated, false), ["1|mcqf"]);
}

#[test]
fn grouped_join_with_a_subquery_outside_the_join() {
    let (_db, mut e) = fixture();
    let sql = "SELECT b.t, count(*) FROM a JOIN b ON b.k = a.k \
               WHERE NOT EXISTS (SELECT 1 FROM c WHERE c.t = b.t) GROUP BY b.t";
    assert_eq!(rows(&mut e, sql, false), ["mcqf|1"]);
}

#[test]
fn left_join_with_a_subquery_outside_the_join() {
    let (_db, mut e) = fixture();
    let sql = "SELECT a.id, b.t FROM a LEFT JOIN b ON b.k = a.k \
               WHERE NOT EXISTS (SELECT 1 FROM c WHERE c.id = a.id AND c.t = b.t) \
               ORDER BY 1, 2";
    assert_eq!(rows(&mut e, sql, true), ["1|mcqf", "2|icf", "3|NULL"]);
}

#[test]
fn full_join_with_a_subquery_outside_the_join() {
    let (_db, mut e) = fixture();
    let sql = "SELECT a.id, b.t FROM a FULL JOIN b ON b.k = a.k \
               WHERE NOT EXISTS (SELECT 1 FROM c WHERE c.t = b.t) ORDER BY 1, 2";
    assert_eq!(rows(&mut e, sql, true), ["NULL|orphan", "1|mcqf", "3|NULL"]);
}

#[test]
fn aliased_join_tables_correlate_by_alias() {
    let (_db, mut e) = fixture();
    let sql = "SELECT x.id, y.t FROM a AS x JOIN b AS y ON y.k = x.k \
               WHERE NOT EXISTS (SELECT 1 FROM c WHERE c.id = x.id AND c.t = y.t)";
    assert_eq!(rows(&mut e, sql, false), ["1|mcqf", "2|icf"]);
}

#[test]
fn a_subquery_over_a_joined_table_still_shadows_by_its_own_alias() {
    let (_db, mut e) = fixture();
    let sql = "SELECT a.id, b.t FROM a JOIN b ON b.k = a.k \
               WHERE NOT EXISTS (SELECT 1 FROM a AS a2 WHERE a2.id = a.id - 1) ORDER BY 1, 2";
    assert_eq!(rows(&mut e, sql, true), ["1|icf", "1|mcqf"]);
}
