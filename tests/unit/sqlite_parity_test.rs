// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Row engine parity with sqlite3 (#596, #597): result column names and
//! query results, each expectation taken from sqlite3 3.51 (`.headers
//! on`) over the same schema and rows, and pinned here so CI needs no
//! sqlite3.
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
        let path = std::env::temp_dir().join(format!(
            "db-core-sqlite-parity-{label}-{}-{}.db",
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

/// The schema the sqlite3 expectations were taken over.
fn engine(db: &TempDb) -> RowEngine {
    let mut e = RowEngine::open(db.path()).expect("open fixture copy");
    for stmt in [
        "DROP TABLE IF EXISTS t",
        r#"CREATE TABLE t(Abc INT, "x y" TEXT, k TEXT)"#,
        "INSERT INTO t VALUES(1, 'p', 'a')",
        "CREATE TABLE u(k TEXT, v INT)",
        "INSERT INTO u VALUES('a', 2)",
        "CREATE TABLE w(id INTEGER PRIMARY KEY, a)",
        "INSERT INTO w VALUES(1, 2)",
        "CREATE VIEW vv AS SELECT a, id+1 FROM w",
        "CREATE VIEW v2(p, q) AS SELECT a, id FROM w",
    ] {
        e.run_query(stmt)
            .unwrap_or_else(|err| panic!("{stmt}: {err}"));
    }
    e
}

/// #596: each query's header row as sqlite3 3.51 prints it.
#[test]
fn result_column_names_match_sqlite3() {
    let db = TempDb::new("names");
    let mut e = engine(&db);
    let cases: &[(&str, &[&str])] = &[
        ("SELECT abc FROM t", &["Abc"]),
        ("SELECT ABC, t.abc FROM t", &["Abc", "Abc"]),
        (r#"SELECT "x y", [x y] FROM t"#, &["x y", "x y"]),
        (
            "SELECT abc+1, abc + 1, ABC  +  1 FROM t",
            &["abc+1", "abc + 1", "ABC  +  1"],
        ),
        (
            "SELECT count(*), COUNT( * ), sum(abc) FROM t",
            &["count(*)", "COUNT( * )", "sum(abc)"],
        ),
        (
            r#"SELECT -abc, (abc), "abc", 'lit', 1.50, NULL FROM t"#,
            &["-abc", "Abc", "Abc", "'lit'", "1.50", "NULL"],
        ),
        (
            "SELECT CAST(abc AS TEXT), CASE WHEN abc THEN 1 END FROM t",
            &["CAST(abc AS TEXT)", "CASE WHEN abc THEN 1 END"],
        ),
        ("SELECT (SELECT 1), abc AS A2 FROM t", &["(SELECT 1)", "A2"]),
        ("SELECT abc /*c*/ + 1 FROM t", &["abc /*c*/ + 1"]),
        ("SELECT * FROM t", &["Abc", "x y", "k"]),
        (
            "SELECT t.*, u.* FROM t JOIN u ON t.k = u.k",
            &["Abc", "x y", "k", "k", "v"],
        ),
        ("SELECT * FROM t JOIN u USING(k)", &["Abc", "x y", "k", "v"]),
        ("SELECT * FROM t NATURAL JOIN u", &["Abc", "x y", "k", "v"]),
        ("SELECT t.k, u.k FROM t JOIN u ON t.k = u.k", &["k", "k"]),
        ("SELECT abc FROM t UNION SELECT v FROM u", &["Abc"]),
        (
            "SELECT 1 AS one, abc FROM t UNION SELECT v, k FROM u",
            &["one", "Abc"],
        ),
        ("SELECT x FROM (SELECT abc AS x FROM t) AS s", &["x"]),
        ("SELECT * FROM (SELECT abc FROM t) AS s", &["abc"]),
        ("SELECT * FROM (SELECT ABC FROM t) AS s", &["ABC"]),
        ("SELECT * FROM (SELECT t.ABC FROM t) AS s", &["ABC"]),
        ("SELECT * FROM (SELECT abc+1 FROM t) AS s", &["abc+1"]),
        ("SELECT ABC FROM (SELECT abc FROM t) AS s", &["abc"]),
        (
            "SELECT * FROM (SELECT abc, abc FROM t) AS s",
            &["abc", "abc:1"],
        ),
        ("SELECT s.abc FROM (SELECT abc+1 AS abc FROM t) s", &["abc"]),
        ("WITH c AS (SELECT abc FROM t) SELECT * FROM c", &["abc"]),
        ("WITH c(z) AS (SELECT abc FROM t) SELECT * FROM c", &["z"]),
        ("SELECT k, upper(k) FROM t GROUP BY k", &["k", "upper(k)"]),
        ("SELECT * FROM vv", &["a", "id+1"]),
        ("SELECT * FROM v2", &["p", "q"]),
        ("SELECT * FROM w x JOIN w y USING(a)", &["id", "a", "id"]),
        ("SELECT x.*, y.id FROM w x, w y", &["id", "a", "id"]),
        ("SELECT COUNT(*), k FROM t GROUP BY k", &["COUNT(*)", "k"]),
        ("SELECT 1, 'a' || 'b'", &["1", "'a' || 'b'"]),
    ];
    let mut failures = Vec::new();
    for (sql, expected) in cases {
        match e.run_query(sql) {
            Ok(r) if r.columns == *expected => {}
            Ok(r) => failures.push(format!(
                "{sql}\n    got {:?}, sqlite3 {expected:?}",
                r.columns
            )),
            Err(err) => failures.push(format!("{sql}\n    error: {err}")),
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} differ:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

/// Queries sqlite3 answers and the row engine still refuses, with the
/// headers sqlite3 gives. Each is asserted to still fail: once one runs,
/// move it into [`result_column_names_match_sqlite3`] and close its issue.
const KNOWN_GAPS: &[(&str, &[&str], &str)] = &[
    ("SELECT x.abc FROM t AS x", &["Abc"], "#604"),
    ("SELECT rowid, oid FROM w", &["id", "id"], "#605"),
    ("SELECT rowid, _rowid_ FROM t", &["rowid", "rowid"], "#605"),
];

#[test]
fn known_gaps_still_fail() {
    let db = TempDb::new("gaps");
    let mut e = engine(&db);
    for (sql, headers, issue) in KNOWN_GAPS {
        assert!(
            e.run_query(sql).is_err(),
            "{sql} now runs: check its headers are {headers:?}, move it to the parity cases \
             and close {issue}"
        );
    }
}

fn text(s: &str) -> Cell {
    Cell::Text(s.to_string())
}

/// The values under those headers are unchanged: names only.
#[test]
fn renamed_columns_keep_their_values() {
    let db = TempDb::new("values");
    let mut e = engine(&db);
    let r = e.run_query("SELECT abc+1, upper(k) FROM t").unwrap();
    assert_eq!(r.columns, ["abc+1", "upper(k)"]);
    assert_eq!(r.rows, vec![vec![Cell::Int(2), text("A")]]);
}
