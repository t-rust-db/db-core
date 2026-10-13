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
const KNOWN_GAPS: &[(&str, &[&str], &str)] = &[
    ("SELECT x FROM (SELECT 1 AS x) WHERE x = 1", &["x"], "#611"),
    (
        "SELECT DISTINCT t.k FROM t JOIN u ON t.k = u.k GROUP BY t.k",
        &["k"],
        "#608",
    ),
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

/// One row as sqlite3's `.mode quote` prints it.
fn quote(row: &[Cell]) -> String {
    row.iter()
        .map(|c| match c {
            Cell::Null => "NULL".to_string(),
            Cell::Int(i) => i.to_string(),
            Cell::Real(f) => format!("{f:?}"),
            Cell::Bool(b) => i64::from(*b).to_string(),
            Cell::Text(s) => format!("'{}'", s.replace('\'', "''")),
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
/// sqlite3 3.51 reports it. ORDER BY on the GROUP BY keys themselves is
/// the one difference (an extra sort here, #607), pinned so a fix shows.
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
    // #607: sqlite3 reports only SCAN g / USE TEMP B-TREE FOR GROUP BY.
    assert_eq!(
        plan("SELECT k, COUNT(*) FROM g GROUP BY k ORDER BY k DESC"),
        [
            "SCAN g",
            "USE TEMP B-TREE FOR GROUP BY",
            "USE TEMP B-TREE FOR ORDER BY"
        ],
        "if this now matches sqlite3, move it into the cases above and close #607"
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
