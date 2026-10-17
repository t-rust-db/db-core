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
        ("SELECT x.abc FROM t AS x", &["Abc"]),
        ("SELECT rowid, oid FROM w", &["id", "id"]),
        ("SELECT rowid, _rowid_ FROM t", &["rowid", "rowid"]),
        ("SELECT x.abc, X.k FROM t x WHERE x.abc = 1", &["Abc", "k"]),
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
const KNOWN_GAPS: &[(&str, &[&str], &str)] = &[];

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

/// One row as sqlite3's `.mode quote` prints it.
fn quote(row: &[Cell]) -> String {
    row.iter()
        .map(|c| match c {
            Cell::Null => "NULL".to_string(),
            Cell::Int(i) => i.to_string(),
            Cell::Real(f) => format!("{f:?}"),
            Cell::Bool(b) => i64::from(*b).to_string(),
            Cell::Text(s) => format!("'{}'", s.replace('\'', "''")),
            Cell::Decimal { .. } => c.to_string(),
            Cell::Blob(b) => format!(
                "X'{}'",
                b.iter().map(|x| format!("{x:02X}")).collect::<String>()
            ),
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn grouped_engine(db: &TempDb) -> RowEngine {
    let mut e = RowEngine::open(db.path()).expect("open fixture copy");
    e.run_query(
        "CREATE TABLE g(k TEXT, n INT, s TEXT); \
         INSERT INTO g VALUES('a',5,'x'),('b',1,'y'),('b',2,'y'),('c',9,'X'),(NULL,4,'z'),('A',3,'x'),(NULL,7,NULL)",
    )
    .unwrap();
    e
}

/// #597: ORDER BY and DISTINCT over an aggregate, each result as sqlite3
/// 3.51 prints it (`.mode quote`, rows joined by `;`). Ties are broken
/// explicitly wherever sqlite3 leaves the order open.
#[test]
fn ordering_and_distinct_after_aggregation_match_sqlite3() {
    let db = TempDb::new("grouped");
    let mut e = grouped_engine(&db);
    let cases: &[(&str, &str)] = &[
        (
            "SELECT COUNT(*), k FROM g GROUP BY k ORDER BY k DESC",
            "1,'c';2,'b';1,'a';1,'A';2,NULL",
        ),
        (
            "SELECT k, COUNT(*) FROM g GROUP BY k ORDER BY k",
            "NULL,2;'A',1;'a',1;'b',2;'c',1",
        ),
        (
            "SELECT k, SUM(n) FROM g GROUP BY k ORDER BY SUM(n) DESC, k",
            "NULL,11;'c',9;'a',5;'A',3;'b',3",
        ),
        (
            "SELECT k FROM g GROUP BY k ORDER BY SUM(n), k",
            "'A';'b';'a';'c';NULL",
        ),
        (
            "SELECT k, COUNT(*) AS c FROM g GROUP BY k ORDER BY c DESC, k",
            "NULL,2;'b',2;'A',1;'a',1;'c',1",
        ),
        (
            "SELECT k, COUNT(*) FROM g GROUP BY k ORDER BY 2 DESC, 1 DESC",
            "'b',2;NULL,2;'c',1;'a',1;'A',1",
        ),
        (
            "SELECT k, MAX(n) FROM g GROUP BY k ORDER BY MAX(n) - MIN(n), k",
            "'A',3;'a',5;'c',9;'b',2;NULL,7",
        ),
        (
            "SELECT k FROM g GROUP BY k ORDER BY k NULLS LAST",
            "'A';'a';'b';'c';NULL",
        ),
        (
            "SELECT k FROM g GROUP BY k ORDER BY k DESC NULLS FIRST",
            "NULL;'c';'b';'a';'A'",
        ),
        (
            "SELECT k FROM g GROUP BY k ORDER BY k COLLATE NOCASE, k",
            "NULL;'A';'a';'b';'c'",
        ),
        (
            "SELECT k, COUNT(*) FROM g GROUP BY k HAVING COUNT(*) > 1 ORDER BY k DESC",
            "'b',2;NULL,2",
        ),
        (
            "SELECT k, COUNT(*) FROM g GROUP BY k ORDER BY k LIMIT 2",
            "NULL,2;'A',1",
        ),
        (
            "SELECT k, COUNT(*) FROM g GROUP BY k ORDER BY k LIMIT 2 OFFSET 2",
            "'a',1;'b',2",
        ),
        (
            "SELECT s, COUNT(*) FROM g GROUP BY s ORDER BY 2 DESC, s",
            "'x',2;'y',2;NULL,1;'X',1;'z',1",
        ),
        ("SELECT COUNT(*) FROM g ORDER BY 1", "7"),
        (
            "SELECT SUM(n), MAX(n) FROM g ORDER BY SUM(n) DESC LIMIT 1",
            "31,9",
        ),
        (
            "SELECT DISTINCT COUNT(*) FROM g GROUP BY k ORDER BY 1 DESC",
            "2;1",
        ),
        (
            "SELECT DISTINCT s FROM g GROUP BY k, s ORDER BY s",
            "NULL;'X';'x';'y';'z'",
        ),
        ("SELECT DISTINCT COUNT(*) FROM g", "7"),
        (
            "SELECT k, COUNT(*) FROM g GROUP BY k ORDER BY k LIMIT 0",
            "",
        ),
        (
            "SELECT k, COUNT(*) AS k2 FROM g GROUP BY k ORDER BY k2, k",
            "'A',1;'a',1;'c',1;NULL,2;'b',2",
        ),
        ("SELECT * FROM g WHERE n > 100 GROUP BY k ORDER BY k", ""),
        (
            "SELECT a.k, COUNT(*) FROM g a JOIN g b ON a.k = b.k GROUP BY a.k ORDER BY 2, 1",
            "'A',1;'a',1;'c',1;'b',4",
        ),
    ];
    let mut failures = Vec::new();
    for (sql, expected) in cases {
        match e.run_query(sql) {
            Ok(r) => {
                let got = r
                    .rows
                    .iter()
                    .map(|row| quote(row))
                    .collect::<Vec<_>>()
                    .join(";");
                if got != *expected {
                    failures.push(format!("{sql}\n    got     {got}\n    sqlite3 {expected}"));
                }
            }
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
    // Without ORDER BY the row order is open: compare as a set.
    let mut rows: Vec<String> = e
        .run_query("SELECT DISTINCT COUNT(*) FROM g GROUP BY k")
        .unwrap()
        .rows
        .iter()
        .map(|r| quote(r))
        .collect();
    rows.sort();
    assert_eq!(rows, ["1", "2"]);
}

/// The same step behind `INSERT ... SELECT` and inside a UNION arm, which
/// drive the aggregate scan with their own sinks.
#[test]
fn ordered_aggregates_feed_inserts_and_union_arms() {
    let db = TempDb::new("sinks");
    let mut e = grouped_engine(&db);
    e.run_query(
        "CREATE TABLE top2(k TEXT, c INT); \
         INSERT INTO top2 SELECT k, COUNT(*) FROM g GROUP BY k ORDER BY COUNT(*) DESC, k LIMIT 2",
    )
    .unwrap();
    let rows = e.run_query("SELECT * FROM top2").unwrap().rows;
    let got: Vec<String> = rows.iter().map(|r| quote(r)).collect();
    assert_eq!(got, ["NULL,2", "'b',2"], "sqlite3: NULL,2;'b',2");

    // Without ORDER BY too: every group after the first used to read the
    // previous group's flush record (a fixed cursor 4 shared with the
    // INSERT's scan), inserting three wrong rows instead of five.
    e.run_query(
        "CREATE TABLE every(k TEXT, c INT); \
         INSERT INTO every SELECT k, COUNT(*) FROM g GROUP BY k",
    )
    .unwrap();
    let rows = e.run_query("SELECT * FROM every").unwrap().rows;
    let mut got: Vec<String> = rows.iter().map(|r| quote(r)).collect();
    got.sort();
    assert_eq!(
        got,
        ["'A',1", "'a',1", "'b',2", "'c',1", "NULL,2"],
        "sqlite3: NULL,2;'A',1;'a',1;'b',2;'c',1"
    );

    let rows = e
        .run_query(
            "SELECT DISTINCT COUNT(*) FROM g GROUP BY k \
             UNION ALL SELECT MIN(n) FROM g WHERE n < 3 GROUP BY s",
        )
        .unwrap()
        .rows;
    let mut got: Vec<String> = rows.iter().map(|r| quote(r)).collect();
    got.sort();
    assert_eq!(got, ["1", "1", "2"], "sqlite3: 2;1;1 (arm order open)");
}

/// #597: EXPLAIN QUERY PLAN for ordered and distinct aggregates, as
/// sqlite3 3.51 reports it.
#[test]
fn aggregate_plans_match_sqlite3() {
    let db = TempDb::new("eqp");
    let e = grouped_engine(&db);
    let plan = |sql: &str| -> Vec<String> {
        e.explain_plan(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err}"))
            .into_iter()
            .map(|r| r.detail)
            .collect()
    };
    let cases: &[(&str, &[&str])] = &[
        (
            "SELECT k, SUM(n) FROM g GROUP BY k ORDER BY SUM(n)",
            &[
                "SCAN g",
                "USE TEMP B-TREE FOR GROUP BY",
                "USE TEMP B-TREE FOR ORDER BY",
            ],
        ),
        (
            "SELECT DISTINCT COUNT(*) FROM g GROUP BY k ORDER BY 1 DESC",
            &[
                "SCAN g",
                "USE TEMP B-TREE FOR GROUP BY",
                "USE TEMP B-TREE FOR DISTINCT",
                "USE TEMP B-TREE FOR ORDER BY",
            ],
        ),
        (
            "SELECT DISTINCT COUNT(*) FROM g GROUP BY k",
            &[
                "SCAN g",
                "USE TEMP B-TREE FOR GROUP BY",
                "USE TEMP B-TREE FOR DISTINCT",
            ],
        ),
        (
            "SELECT DISTINCT COUNT(*) FROM g",
            &["SCAN g", "USE TEMP B-TREE FOR DISTINCT"],
        ),
        ("SELECT COUNT(*) FROM g ORDER BY 1", &["SCAN g"]),
    ];
    for (sql, expected) in cases {
        assert_eq!(plan(sql), *expected, "{sql}");
    }
    // #607: ORDER BY on the GROUP BY key needs no second sort.
    assert_eq!(
        plan("SELECT k, COUNT(*) FROM g GROUP BY k ORDER BY k DESC"),
        ["SCAN g", "USE TEMP B-TREE FOR GROUP BY"]
    );
}

/// DISTINCT over an aggregate compares under each column's collation: a
/// NOCASE column dedupes `'a'` and `'A'`, an explicit `COLLATE BINARY`
/// keeps both (sqlite3: `'a';'b'` and `'A';'a';'b'`).
#[test]
fn distinct_after_aggregation_honours_collations() {
    let db = TempDb::new("collate");
    let mut e = RowEngine::open(db.path()).expect("open fixture copy");
    e.run_query(
        "CREATE TABLE c2(k TEXT COLLATE NOCASE, n INT); \
         INSERT INTO c2 VALUES('a',1),('A',2),('b',3)",
    )
    .unwrap();
    let rows = |e: &mut RowEngine, sql: &str| -> Vec<String> {
        e.run_query(sql)
            .unwrap()
            .rows
            .iter()
            .map(|r| quote(r))
            .collect()
    };
    let nocase = rows(&mut e, "SELECT DISTINCT k FROM c2 GROUP BY n");
    assert_eq!(nocase.len(), 2, "{nocase:?}");
    assert!(nocase.contains(&"'b'".to_string()), "{nocase:?}");
    assert_eq!(
        rows(
            &mut e,
            "SELECT DISTINCT k COLLATE BINARY FROM c2 GROUP BY n ORDER BY 1"
        ),
        ["'A'", "'a'", "'b'"]
    );
}

/// #604: a table alias qualifies columns in every clause of a
/// single-table query, including a correlated subquery, and the real
/// table name no longer does (sqlite3 3.51 results).
#[test]
fn table_aliases_qualify_columns_in_single_table_queries() {
    let db = TempDb::new("alias");
    let mut e = grouped_engine(&db);
    let cases: &[(&str, &str)] = &[
        ("SELECT x.k, x.n FROM g AS x WHERE x.n > 4 ORDER BY x.n", "'a',5;NULL,7;'c',9"),
        (
            "SELECT x.k, COUNT(*) FROM g x GROUP BY x.k HAVING COUNT(*) > 1 ORDER BY x.k",
            "NULL,2;'b',2",
        ),
        ("SELECT x.* FROM g x WHERE x.n = 9", "'c',9,'X'"),
        (
            "SELECT x.k FROM g x WHERE x.n = (SELECT MAX(y.n) FROM g y WHERE y.k = x.k) ORDER BY x.k",
            "'A';'a';'b';'c'",
        ),
        ("SELECT DISTINCT x.s FROM g x WHERE x.s IS NOT NULL ORDER BY 1", "'X';'x';'y';'z'"),
        ("SELECT x.n FROM g x ORDER BY x.n DESC LIMIT 2", "9;7"),
    ];
    let mut failures = Vec::new();
    for (sql, expected) in cases {
        match e.run_query(sql) {
            Ok(r) => {
                let got = r
                    .rows
                    .iter()
                    .map(|row| quote(row))
                    .collect::<Vec<_>>()
                    .join(";");
                if got != *expected {
                    failures.push(format!("{sql}\n    got     {got}\n    sqlite3 {expected}"));
                }
            }
            Err(err) => failures.push(format!("{sql}\n    error: {err}")),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    // sqlite3: "no such column: g.k".
    let err = e.run_query("SELECT g.k FROM g AS x").unwrap_err();
    assert!(err.to_string().contains("g.k"), "{err}");
}

fn rowid_engine(db: &TempDb) -> RowEngine {
    let mut e = RowEngine::open(db.path()).expect("open fixture copy");
    e.run_query(
        "CREATE TABLE p(a TEXT, b INT); INSERT INTO p VALUES('x',3),('y',1),('z',2); \
         CREATE TABLE k(id INTEGER PRIMARY KEY, v TEXT); INSERT INTO k VALUES(10,'p'),(20,'q'); \
         CREATE TABLE s(rowid TEXT, v INT); INSERT INTO s VALUES('r1',5); \
         CREATE TABLE s2(rowid INT, v TEXT); INSERT INTO s2 VALUES(5,'a'),(1,'b'); \
         CREATE TABLE w(a INT PRIMARY KEY, b) WITHOUT ROWID",
    )
    .unwrap();
    e
}

/// Runs each query and compares sqlite3's `.headers on` + `.mode quote`
/// output: the header row, then the rows, joined by `;`.
fn check_cases(e: &mut RowEngine, cases: &[(&str, &str)]) {
    let mut failures = Vec::new();
    for (sql, expected) in cases {
        match e.run_query(sql) {
            Ok(r) => {
                let header = r
                    .columns
                    .iter()
                    .map(|c| format!("'{}'", c.replace('\'', "''")))
                    .collect::<Vec<_>>()
                    .join(",");
                let got = std::iter::once(header)
                    .chain(r.rows.iter().map(|row| quote(row)))
                    .collect::<Vec<_>>()
                    .join(";");
                if got != *expected {
                    failures.push(format!("{sql}\n    got     {got}\n    sqlite3 {expected}"));
                }
            }
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

/// #605: rowid, oid and _rowid_ (sqlite3 3.51, `.headers on`).
#[test]
fn rowid_references_match_sqlite3() {
    let db = TempDb::new("rowid");
    let mut e = rowid_engine(&db);
    check_cases(
        &mut e,
        &[
            ("SELECT rowid, a FROM p", "'rowid','a';1,'x';2,'y';3,'z'"),
            (
                "SELECT oid, _rowid_, ROWID FROM p WHERE rowid > 1",
                "'rowid','rowid','rowid';2,2,2;3,3,3",
            ),
            ("SELECT a FROM p WHERE rowid = 2", "'a';'y'"),
            (
                "SELECT rowid + 1, typeof(rowid) FROM p WHERE oid < 3",
                "'rowid + 1','typeof(rowid)';2,'integer';3,'integer'",
            ),
            (
                "SELECT a FROM p WHERE rowid IN (1, 3) ORDER BY a DESC",
                "'a';'z';'x'",
            ),
            (
                "SELECT rowid, a FROM p ORDER BY b",
                "'rowid','a';2,'y';3,'z';1,'x'",
            ),
            (
                "SELECT rowid, a FROM p ORDER BY rowid DESC",
                "'rowid','a';3,'z';2,'y';1,'x'",
            ),
            (
                "SELECT rowid, id, v FROM k WHERE rowid >= 20",
                "'id','id','v';20,20,'q'",
            ),
            ("SELECT rowid, v FROM s", "'rowid','v';'r1',5"),
            ("SELECT v FROM s WHERE rowid = 'r1'", "'v';5"),
            (
                "SELECT x.rowid, x.a FROM p x WHERE x.rowid = 3",
                "'rowid','a';3,'z'",
            ),
            (
                "SELECT COUNT(*), MAX(rowid) FROM p",
                "'COUNT(*)','MAX(rowid)';3,3",
            ),
            (
                "SELECT b % 2 AS par, MAX(rowid) FROM p GROUP BY b % 2 ORDER BY 1",
                "'par','MAX(rowid)';0,3;1,2",
            ),
            // After a sort the rowid comes from the sort record, also inside
            // an expression; `id + 1` used to read 0 there (sqlite3 21, 11).
            (
                "SELECT id + 1, v FROM k ORDER BY v DESC",
                "'id + 1','v';21,'q';11,'p'",
            ),
            (
                "SELECT rowid * 10, a FROM p ORDER BY a DESC LIMIT 2",
                "'rowid * 10','a';30,'z';20,'y'",
            ),
            (
                "SELECT DISTINCT rowid % 2 FROM p ORDER BY 1",
                "'rowid % 2';0;1",
            ),
        ],
    );
    // sqlite3: "no such column: rowid".
    for sql in [
        "SELECT rowid FROM w",
        "SELECT rowid FROM (SELECT a FROM p) AS q",
        "SELECT rowid FROM (SELECT a FROM p)",
    ] {
        let err = e.run_query(sql).unwrap_err();
        assert!(err.to_string().contains("rowid"), "{sql}: {err}");
    }
}

/// #605: rowid in UPDATE and DELETE WHERE clauses (sqlite3:
/// 1,'x',3;2,'y',100).
#[test]
fn rowid_in_update_and_delete() {
    let db = TempDb::new("rowid-dml");
    let mut e = rowid_engine(&db);
    e.run_query("UPDATE p SET b = b * 100 WHERE rowid > 1; DELETE FROM p WHERE oid = 3")
        .unwrap();
    let rows: Vec<String> = e
        .run_query("SELECT rowid, a, b FROM p")
        .unwrap()
        .rows
        .iter()
        .map(|r| quote(r))
        .collect();
    assert_eq!(rows, ["1,'x',3", "2,'y',100"]);
}

/// #606: a FROM subquery without an alias (sqlite3 3.51, `.headers on`).
#[test]
fn unaliased_from_subqueries_match_sqlite3() {
    let db = TempDb::new("unaliased");
    let mut e = grouped_engine(&db);
    check_cases(
        &mut e,
        &[
            (
                "SELECT c, k FROM (SELECT COUNT(*) AS c, k FROM g GROUP BY k) ORDER BY k DESC",
                "'c','k';1,'c';2,'b';1,'a';1,'A';2,NULL",
            ),
            (
                "SELECT * FROM (SELECT k FROM g WHERE n > 4) ORDER BY 1",
                "'k';NULL;'a';'c'",
            ),
        ],
    );
    // Nothing can qualify an unaliased subquery's columns.
    let err = e
        .run_query("SELECT a.k FROM (SELECT k FROM g) JOIN (SELECT k AS k2 FROM g) ON k = k2")
        .unwrap_err();
    assert!(err.to_string().contains("a.k"), "{err}");
}

/// #607: ORDER BY listing the GROUP BY keys orders the groups as they are
/// formed, with or without an index on the key (sqlite3 3.51). A
/// descending order walks the index backward; NULLS FIRST on a
/// descending key still needs the second sort.
#[test]
fn ordering_by_the_group_keys_sorts_once() {
    let db = TempDb::new("group-order");
    let mut e = grouped_engine(&db);
    let cases: &[(&str, &str, &[&str])] = &[
        (
            "SELECT k, COUNT(*) FROM g GROUP BY k ORDER BY k DESC",
            "'c',1;'b',2;'a',1;'A',1;NULL,2",
            &["SCAN g", "USE TEMP B-TREE FOR GROUP BY"],
        ),
        (
            "SELECT k, COUNT(*) FROM g GROUP BY k ORDER BY 1",
            "NULL,2;'A',1;'a',1;'b',2;'c',1",
            &["SCAN g", "USE TEMP B-TREE FOR GROUP BY"],
        ),
        (
            "SELECT k AS key, SUM(n) FROM g GROUP BY k ORDER BY key DESC LIMIT 3",
            "'c',9;'b',3;'a',5",
            &["SCAN g", "USE TEMP B-TREE FOR GROUP BY"],
        ),
        (
            "SELECT k, s, COUNT(*) FROM g GROUP BY k, s ORDER BY k DESC, s DESC",
            "'c','X',1;'b','y',2;'a','x',1;'A','x',1;NULL,'z',1;NULL,NULL,1",
            &["SCAN g", "USE TEMP B-TREE FOR GROUP BY"],
        ),
        (
            "SELECT k, COUNT(*) FROM g GROUP BY k ORDER BY k DESC NULLS FIRST",
            "NULL,2;'c',1;'b',2;'a',1;'A',1",
            &[
                "SCAN g",
                "USE TEMP B-TREE FOR GROUP BY",
                "USE TEMP B-TREE FOR ORDER BY",
            ],
        ),
    ];
    let rows = |e: &mut RowEngine, sql: &str| -> String {
        e.run_query(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err}"))
            .rows
            .iter()
            .map(|r| quote(r))
            .collect::<Vec<_>>()
            .join(";")
    };
    let plan = |e: &RowEngine, sql: &str| -> Vec<String> {
        e.explain_plan(sql)
            .unwrap()
            .into_iter()
            .map(|r| r.detail)
            .collect()
    };
    for (sql, expected, expected_plan) in cases {
        assert_eq!(rows(&mut e, sql), *expected, "{sql}");
        assert_eq!(plan(&e, sql), *expected_plan, "{sql}");
    }
    // With an index on the key: the same rows, and the groups come off the
    // index (backward for DESC) with no temp b-tree at all.
    e.run_query("CREATE INDEX gk ON g(k)").unwrap();
    for (sql, expected, _) in cases {
        assert_eq!(rows(&mut e, sql), *expected, "{sql} (indexed)");
    }
    let indexed = plan(&e, "SELECT k, COUNT(*) FROM g GROUP BY k ORDER BY k DESC");
    assert!(
        indexed.iter().any(|r| r.contains("INDEX gk")),
        "{indexed:?}"
    );
    assert!(
        !indexed.iter().any(|r| r.contains("TEMP B-TREE")),
        "{indexed:?}"
    );
}

/// #608: DISTINCT over a joined aggregate (sqlite3 3.51, `.headers on`):
/// a duplicate group row is dropped before ORDER BY and LIMIT see it.
#[test]
fn distinct_over_a_joined_aggregate_matches_sqlite3() {
    let db = TempDb::new("joined-distinct");
    let mut e = grouped_engine(&db);
    check_cases(
        &mut e,
        &[
            (
                "SELECT DISTINCT a.s FROM g a JOIN g b ON a.k = b.k GROUP BY a.k, a.s ORDER BY 1",
                "'s';'X';'x';'y'",
            ),
            (
                "SELECT DISTINCT COUNT(*) FROM g a JOIN g b ON a.k = b.k GROUP BY a.k ORDER BY 1 DESC",
                "'COUNT(*)';4;1",
            ),
            (
                "SELECT DISTINCT COUNT(*) FROM g a JOIN g b ON a.k = b.k GROUP BY a.k ORDER BY 1 DESC LIMIT 1",
                "'COUNT(*)';4",
            ),
        ],
    );
    // The plan: GROUP BY, DISTINCT and ORDER BY temp b-trees after the
    // join, as sqlite3 lists them.
    let plan: Vec<String> = e
        .explain_plan(
            "SELECT DISTINCT COUNT(*) FROM g a JOIN g b ON a.k = b.k GROUP BY a.k ORDER BY 1 DESC",
        )
        .unwrap()
        .into_iter()
        .map(|r| r.detail)
        .collect();
    let temp: Vec<&String> = plan.iter().filter(|r| r.contains("TEMP B-TREE")).collect();
    assert_eq!(
        temp,
        [
            "USE TEMP B-TREE FOR GROUP BY",
            "USE TEMP B-TREE FOR DISTINCT",
            "USE TEMP B-TREE FOR ORDER BY"
        ],
        "{plan:?}"
    );
}

/// #613: DISTINCT over a plain join compares each column under its
/// collation, sorted or not, across INNER, LEFT and FULL joins and
/// through `*`/`t.*` (sqlite3 3.53, `.headers on`).
#[test]
fn distinct_over_a_join_honours_collations() {
    let db = TempDb::new("join-distinct-collate");
    let mut e = grouped_engine(&db);
    e.run_query(
        "CREATE TABLE h(k INT, s TEXT COLLATE NOCASE); \
         INSERT INTO h VALUES(1,'a'),(1,'A'),(2,'b')",
    )
    .unwrap();
    check_cases(
        &mut e,
        &[
            (
                "SELECT DISTINCT h1.s FROM h h1 JOIN h h2 ON h1.k = h2.k",
                "'s';'a';'b'",
            ),
            (
                "SELECT DISTINCT h1.s FROM h h1 LEFT JOIN h h2 ON h1.k = h2.k",
                "'s';'a';'b'",
            ),
            (
                "SELECT DISTINCT h1.s FROM h h1 FULL JOIN h h2 ON h1.k = h2.k",
                "'s';'a';'b'",
            ),
            (
                "SELECT DISTINCT h1.s FROM h h1 JOIN h h2 ON h1.k = h2.k ORDER BY 1 DESC",
                "'s';'b';'a'",
            ),
            (
                "SELECT DISTINCT h1.s FROM h h1 LEFT JOIN h h2 ON h1.k = h2.k ORDER BY 1",
                "'s';'a';'b'",
            ),
            (
                "SELECT DISTINCT h1.s FROM h h1 FULL JOIN h h2 ON h1.k = h2.k ORDER BY 1",
                "'s';'a';'b'",
            ),
            (
                "SELECT DISTINCT h1.* FROM h h1 JOIN h h2 ON h1.k = h2.k",
                "'k','s';1,'a';2,'b'",
            ),
            (
                "SELECT DISTINCT h1.s COLLATE BINARY FROM h h1 JOIN h h2 ON h1.k = h2.k ORDER BY 1",
                "'h1.s COLLATE BINARY';'A';'a';'b'",
            ),
            (
                "SELECT DISTINCT a.s COLLATE NOCASE FROM g a JOIN g b ON a.k = b.k ORDER BY 1",
                "'a.s COLLATE NOCASE';'x';'y'",
            ),
        ],
    );
    // `*` skips the USING column and keeps both NOCASE `s` columns: one
    // row per `k` (which spelling survives follows the join order).
    let r = e
        .run_query("SELECT DISTINCT * FROM h h1 JOIN h h2 USING (k)")
        .unwrap();
    assert_eq!(r.columns, ["k", "s", "s"]);
    assert_eq!(r.rows.len(), 2, "{:?}", r.rows);
}

/// ORDER BY a position over a join sorts on that result column, under
/// its collation (sqlite3 3.53, `.headers on`). It used to sort on the
/// constant itself, leaving the rows in join order.
#[test]
fn ordering_a_join_by_position_matches_sqlite3() {
    let db = TempDb::new("join-order-position");
    let mut e = RowEngine::open(db.path()).expect("open fixture copy");
    e.run_query(
        "CREATE TABLE h(k INT, s TEXT COLLATE NOCASE); \
         INSERT INTO h VALUES(1,'a'),(1,'B'),(2,'c')",
    )
    .unwrap();
    check_cases(
        &mut e,
        &[
            (
                "SELECT h1.k, h2.s FROM h h1 JOIN h h2 ON h1.k = h2.k ORDER BY 2 DESC, 1",
                "'k','s';2,'c';1,'B';1,'B';1,'a';1,'a'",
            ),
            (
                "SELECT h1.s AS z FROM h h1 JOIN h h2 ON h1.k = h2.k ORDER BY z DESC",
                "'z';'c';'B';'B';'a';'a'",
            ),
            (
                "SELECT * FROM h h1 JOIN h h2 USING (k) ORDER BY 3 DESC, 2",
                "'k','s','s';2,'c','c';1,'a','B';1,'B','B';1,'a','a';1,'B','a'",
            ),
            (
                "SELECT h1.* FROM h h1 LEFT JOIN h h2 ON h1.k = h2.k + 1 ORDER BY 2 DESC",
                "'k','s';2,'c';2,'c';1,'B';1,'a'",
            ),
        ],
    );
    let err = e
        .run_query("SELECT h1.k FROM h h1 JOIN h h2 ON h1.k = h2.k ORDER BY 2")
        .unwrap_err();
    assert!(err.to_string().contains("out of range"), "{err}");
}

/// #611: a FROM subquery without a FROM of its own, and `VALUES` in
/// FROM, materialized as constant rows (sqlite3 3.53, `.headers on`).
#[test]
fn from_less_subqueries_and_values_match_sqlite3() {
    let db = TempDb::new("from-less");
    let mut e = engine(&db);
    check_cases(
        &mut e,
        &[
            ("SELECT x FROM (SELECT 1 AS x) WHERE x = 1", "'x';1"),
            (
                "SELECT * FROM (SELECT 1 AS x, 'a' AS y) AS s",
                "'x','y';1,'a'",
            ),
            (
                "SELECT * FROM (SELECT 1 AS x) s JOIN t ON s.x = t.Abc",
                "'x','Abc','x y','k';1,1,'p','a'",
            ),
            (
                "SELECT * FROM (SELECT 1 AS x UNION ALL SELECT 2)",
                "'x';1;2",
            ),
            ("WITH c AS (SELECT 1 AS x) SELECT * FROM c", "'x';1"),
            ("SELECT * FROM (SELECT 1+1)", "'1+1';2"),
            (
                "SELECT * FROM (VALUES(1,2),(3,4))",
                "'column1','column2';1,2;3,4",
            ),
            (
                "SELECT * FROM (VALUES(1,2),(3,4)) AS v WHERE column1 > 1",
                "'column1','column2';3,4",
            ),
            (
                "SELECT v.column2 FROM (VALUES(1,'a'),(2,'b')) v \
                 JOIN (VALUES(2)) w ON v.column1 = w.column1",
                "'column2';'b'",
            ),
            (
                "SELECT count(*), sum(column1) FROM (VALUES(1),(2),(NULL))",
                "'count(*)','sum(column1)';3,3",
            ),
        ],
    );
    let err = e.run_query("SELECT * FROM (VALUES(1,2),(3))").unwrap_err();
    assert!(
        err.to_string()
            .contains("all VALUES must have the same number of terms"),
        "{err}"
    );
}

/// #612: a joined GROUP BY/aggregate's result columns, aggregate
/// arguments, HAVING and ORDER BY terms can be any expression over the
/// group's columns and aggregates, under the columns' collations
/// (sqlite3 3.53, `.headers on`).
#[test]
fn joined_aggregate_expressions_match_sqlite3() {
    let db = TempDb::new("joined-agg-expr");
    let mut e = RowEngine::open(db.path()).expect("open fixture copy");
    e.run_query(
        "CREATE TABLE g(k INT, s TEXT, n INT); \
         INSERT INTO g VALUES(1,'x',2),(1,'X',3),(2,'y',4),(3,'z',NULL); \
         CREATE TABLE h(k INT, s TEXT COLLATE NOCASE); \
         INSERT INTO h VALUES(1,'a'),(1,'A'),(2,'b')",
    )
    .unwrap();
    check_cases(
        &mut e,
        &[
            (
                "SELECT DISTINCT a.s COLLATE NOCASE FROM g a JOIN g b ON a.k = b.k GROUP BY a.s",
                "'a.s COLLATE NOCASE';'X';'y';'z'",
            ),
            (
                "SELECT upper(a.s), COUNT(*) + 1, a.n * 2 FROM g a JOIN g b ON a.k = b.k \
                 GROUP BY a.s ORDER BY 1, 3",
                "'upper(a.s)','COUNT(*) + 1','a.n * 2';'X',3,4;'X',3,6;'Y',2,8;'Z',2,NULL",
            ),
            (
                "SELECT a.k, CASE WHEN count(*) > 1 THEN 'many' ELSE 'one' END AS c \
                 FROM g a JOIN g b ON a.k = b.k GROUP BY a.k ORDER BY a.k",
                "'k','c';1,'many';2,'one';3,'one'",
            ),
            (
                "SELECT a.k, sum(a.n * b.n), max(a.n + b.n) - min(b.n) FROM g a JOIN g b \
                 ON a.k = b.k GROUP BY a.k ORDER BY 2 DESC",
                "'k','sum(a.n * b.n)','max(a.n + b.n) - min(b.n)';1,25,4;2,16,4;3,NULL,NULL",
            ),
            (
                "SELECT a.k, count(*) AS c FROM g a JOIN g b ON a.k = b.k GROUP BY a.k \
                 HAVING max(a.s) = 'x' ORDER BY a.k",
                "'k','c';1,4",
            ),
            (
                "SELECT a.k, b.s FROM g a JOIN g b ON a.k = b.k GROUP BY a.k, b.s \
                 HAVING b.s = 'X' ORDER BY 1",
                "'k','s';1,'X'",
            ),
            (
                "SELECT a.k, count(*) FROM g a JOIN g b ON a.k = b.k GROUP BY a.k \
                 ORDER BY count(*) * -1, a.k",
                "'k','count(*)';1,4;2,1;3,1",
            ),
            (
                "SELECT lower(a.s), count(*) FROM h a JOIN h b ON a.k = b.k GROUP BY a.s ORDER BY 1",
                "'lower(a.s)','count(*)';'a',4;'b',1",
            ),
            (
                "SELECT lower(a.s) || '!', count(*) FROM h a LEFT JOIN h b ON a.k = b.k + 1 \
                 GROUP BY a.s ORDER BY 1 DESC",
                "'lower(a.s) || ''!''','count(*)';'b!',2;'a!',2",
            ),
            (
                "SELECT DISTINCT lower(a.s) FROM h a JOIN h b ON a.k = b.k GROUP BY a.s, b.s \
                 ORDER BY 1",
                "'lower(a.s)';'a';'b'",
            ),
            (
                "SELECT coalesce(max(a.n), 0) FROM g a JOIN g b ON a.k = b.k WHERE a.k > 5",
                "'coalesce(max(a.n), 0)';0",
            ),
        ],
    );
}

/// #610's computed GROUP BY keys under a JOIN (sqlite3 3.53).
#[test]
fn joined_aggregate_groups_by_expressions() {
    let db = TempDb::new("joined-agg-keys");
    let mut e = RowEngine::open(db.path()).expect("open fixture copy");
    e.run_query(
        "CREATE TABLE g(k INT, s TEXT, n INT); \
         INSERT INTO g VALUES(1,'x',2),(1,'X',3),(2,'y',4),(3,'z',NULL)",
    )
    .unwrap();
    check_cases(
        &mut e,
        &[
            (
                "SELECT a.k % 2 AS par, count(*) FROM g a JOIN g b ON a.k = b.k \
                 GROUP BY a.k % 2 ORDER BY par",
                "'par','count(*)';0,1;1,5",
            ),
            (
                "SELECT upper(a.s) AS u, count(*) FROM g a JOIN g b ON a.k = b.k \
                 GROUP BY upper(a.s) ORDER BY u DESC",
                "'u','count(*)';'Z',1;'Y',1;'X',4",
            ),
        ],
    );
}

/// An aggregate nested in CASE, a function argument or BETWEEN is
/// accumulated like any other (found with #612; sqlite3 3.53).
#[test]
fn nested_aggregates_match_sqlite3() {
    let db = TempDb::new("nested-agg");
    let mut e = RowEngine::open(db.path()).expect("open fixture copy");
    e.run_query(
        "CREATE TABLE g(k INT, s TEXT, n INT); \
         INSERT INTO g VALUES(1,'x',2),(1,'X',3),(2,'y',4)",
    )
    .unwrap();
    check_cases(
        &mut e,
        &[
            (
                "SELECT k, CASE WHEN count(*) > 1 THEN 'many' ELSE 'one' END FROM g GROUP BY k",
                "'k','CASE WHEN count(*) > 1 THEN ''many'' ELSE ''one'' END';1,'many';2,'one'",
            ),
            (
                "SELECT k, coalesce(max(n), 0), abs(-sum(n)) FROM g GROUP BY k",
                "'k','coalesce(max(n), 0)','abs(-sum(n))';1,3,5;2,4,4",
            ),
            (
                "SELECT k FROM g GROUP BY k HAVING max(n) BETWEEN 3 AND 5",
                "'k';1;2",
            ),
            (
                "SELECT CASE WHEN count(*) > 2 THEN 'big' END FROM g",
                "'CASE WHEN count(*) > 2 THEN ''big'' END';'big'",
            ),
        ],
    );
}

/// #610: a GROUP BY position or result-column alias, and HAVING on an
/// alias, as sqlite3 3.53 resolves them: a FROM column wins over an
/// alias of the same name.
#[test]
fn group_by_positions_and_aliases_match_sqlite3() {
    let db = TempDb::new("group-alias");
    let mut e = rowid_engine(&db);
    check_cases(
        &mut e,
        &[
            (
                "SELECT b % 2 AS par, COUNT(*) FROM p GROUP BY par",
                "'par','COUNT(*)';0,1;1,2",
            ),
            (
                "SELECT b % 2 AS par, COUNT(*) FROM p GROUP BY 1",
                "'par','COUNT(*)';0,1;1,2",
            ),
            (
                "SELECT b % 2 AS a, COUNT(*) FROM p GROUP BY a ORDER BY 1, 2",
                "'a','COUNT(*)';0,1;1,1;1,1",
            ),
            (
                "SELECT b % 2 AS par, COUNT(*) FROM p GROUP BY par HAVING par = 1",
                "'par','COUNT(*)';1,2",
            ),
            (
                "SELECT b % 2 AS par, COUNT(*) AS n FROM p GROUP BY par HAVING n > 1",
                "'par','n';1,2",
            ),
            (
                "SELECT b % 2 AS par, COUNT(*) FROM p GROUP BY 1, par ORDER BY par DESC",
                "'par','COUNT(*)';1,2;0,1",
            ),
            (
                "SELECT *, count(*) FROM p GROUP BY 2 ORDER BY 2",
                "'a','b','count(*)';'y',1,1;'z',2,1;'x',3,1",
            ),
            ("SELECT count(*) AS c FROM p HAVING c > 2", "'c';3"),
            (
                "SELECT x.b % 2 AS par, count(*) FROM p x JOIN p y ON x.b = y.b GROUP BY par",
                "'par','count(*)';0,1;1,2",
            ),
            (
                "SELECT x.b % 2 AS par, count(*) FROM p x JOIN p y ON x.b = y.b \
                 GROUP BY 1 HAVING par = 0",
                "'par','count(*)';0,1",
            ),
        ],
    );
    for (sql, error) in [
        (
            "SELECT b % 2 AS par, COUNT(*) FROM p GROUP BY 3",
            "1st GROUP BY term out of range - should be between 1 and 2",
        ),
        (
            "SELECT b % 2 AS par, COUNT(*) FROM p GROUP BY 2",
            "aggregate functions are not allowed in the GROUP BY clause",
        ),
        (
            "SELECT b % 2 AS par, COUNT(*) AS n FROM p GROUP BY n",
            "aggregate functions are not allowed in the GROUP BY clause",
        ),
        (
            "SELECT x.a AS b, count(*) FROM p x JOIN p y ON x.b = y.b GROUP BY b",
            "ambiguous",
        ),
    ] {
        let err = e.run_query(sql).unwrap_err().to_string();
        assert!(err.contains(error), "{sql}: {err}");
    }
}
