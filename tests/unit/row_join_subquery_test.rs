// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! #937: a subquery expression in a JOIN query may read a table that is
//! not one of the joined tables. Before the fix the join path resolved
//! the subquery's own FROM against the joined tables only ("isn't
//! visible to this compiler's catalog"). A JOIN with ORDER BY also
//! refused any computed result column; it is now evaluated before the
//! sort and carried in the sorter record. `INSERT ... SELECT` resolves
//! its subqueries against the whole catalog too, and a SELECT that reads
//! the target table runs to completion before the first row is inserted.
//! Expected rows are sqlite3 3.54.1's on the same data.
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

// ORDER BY on a JOIN with computed result columns: the rows and their
// order are sqlite3's.

#[test]
fn ordered_join_with_a_scalar_subquery_in_the_select_list() {
    let (_db, mut e) = fixture();
    let sql = "SELECT a.id, b.t, (SELECT count(*) FROM c WHERE c.id = a.id) \
               FROM a JOIN b ON b.k = a.k ORDER BY b.t, a.id";
    assert_eq!(rows(&mut e, sql, true), ["1|icf|1", "2|icf|1", "1|mcqf|1"]);
}

#[test]
fn ordered_join_sorting_on_a_correlated_subquery_alias() {
    let (_db, mut e) = fixture();
    let sql = "SELECT a.id, (SELECT count(*) FROM c WHERE c.t = b.t) AS n, b.t \
               FROM a JOIN b ON b.k = a.k ORDER BY n DESC, b.t, a.id";
    assert_eq!(rows(&mut e, sql, true), ["1|1|icf", "2|1|icf", "1|0|mcqf"]);
}

#[test]
fn ordered_join_by_position_over_computed_items() {
    let (_db, mut e) = fixture();
    let sql = "SELECT a.id + 10, upper(b.t) FROM a JOIN b ON b.k = a.k ORDER BY 2, 1";
    assert_eq!(rows(&mut e, sql, true), ["11|ICF", "12|ICF", "11|MCQF"]);
}

#[test]
fn ordered_join_by_an_alias_and_by_the_expression_itself() {
    let (_db, mut e) = fixture();
    let by_alias = "SELECT a.id * 2 AS d, b.t FROM a JOIN b ON b.k = a.k ORDER BY d DESC, b.t";
    assert_eq!(rows(&mut e, by_alias, true), ["4|icf", "2|icf", "2|mcqf"]);
    let by_expr = "SELECT a.id, b.t || '!' FROM a JOIN b ON b.k = a.k \
                   ORDER BY b.t || '!' DESC, a.id";
    assert_eq!(rows(&mut e, by_expr, true), ["1|mcqf!", "1|icf!", "2|icf!"]);
}

#[test]
fn ordered_join_aliases_shadowing_column_names() {
    let (_db, mut e) = fixture();
    let sql = "SELECT a.id AS t, b.t || '' AS k FROM a JOIN b ON b.k = a.k ORDER BY k, t";
    assert_eq!(rows(&mut e, sql, true), ["1|icf", "2|icf", "1|mcqf"]);
}

#[test]
fn ordered_outer_joins_with_computed_items() {
    let (_db, mut e) = fixture();
    let left = "SELECT a.id, b.t || '!' FROM a LEFT JOIN b ON b.k = a.k ORDER BY 2, 1";
    assert_eq!(
        rows(&mut e, left, true),
        ["3|NULL", "1|icf!", "2|icf!", "1|mcqf!"]
    );
    let right = "SELECT b.t || '#', a.id * 3 FROM a RIGHT JOIN b ON b.k = a.k ORDER BY 2, 1";
    assert_eq!(
        rows(&mut e, right, true),
        ["orphan#|NULL", "icf#|3", "mcqf#|3", "icf#|6"]
    );
    let full = "SELECT a.id, coalesce(b.t, '-') FROM a FULL JOIN b ON b.k = a.k ORDER BY 2, 1";
    assert_eq!(
        rows(&mut e, full, true),
        ["3|-", "1|icf", "2|icf", "1|mcqf", "NULL|orphan"]
    );
    let nulls_last = "SELECT a.id, nullif(b.t, 'icf') AS v FROM a LEFT JOIN b ON b.k = a.k \
                      ORDER BY v DESC, a.id";
    assert_eq!(
        rows(&mut e, nulls_last, true),
        ["1|mcqf", "1|NULL", "2|NULL", "3|NULL"]
    );
}

#[test]
fn ordered_three_table_join_with_a_computed_item() {
    let (_db, mut e) = fixture();
    let sql = "SELECT a.id, b.t, c.t || '/' || b.t FROM a JOIN b ON b.k = a.k \
               JOIN c ON c.id = a.id ORDER BY 3, 1";
    assert_eq!(
        rows(&mut e, sql, true),
        ["1|icf|icf/icf", "1|mcqf|icf/mcqf", "2|icf|other/icf"]
    );
}

#[test]
fn ordered_join_mixing_star_and_computed_items() {
    let (_db, mut e) = fixture();
    let sql = "SELECT a.id - 1, b.*, a.id + 1 FROM a JOIN b ON b.k = a.k ORDER BY b.t, a.id";
    assert_eq!(
        rows(&mut e, sql, true),
        ["0|x|icf|2", "1|y|icf|3", "0|x|mcqf|2"]
    );
}

#[test]
fn ordered_join_with_distinct_and_limit_over_computed_items() {
    let (_db, mut e) = fixture();
    let distinct = "SELECT DISTINCT b.t || '?' FROM a JOIN b ON b.k = a.k ORDER BY 1";
    assert_eq!(rows(&mut e, distinct, true), ["icf?", "mcqf?"]);
    let top =
        "SELECT DISTINCT a.k || '-' || b.k FROM a JOIN b ON b.k = a.k ORDER BY 1 DESC LIMIT 1";
    assert_eq!(rows(&mut e, top, true), ["y-y"]);
    let page =
        "SELECT a.id, b.t || '!' FROM a JOIN b ON b.k = a.k ORDER BY a.k, b.t LIMIT 2 OFFSET 1";
    assert_eq!(rows(&mut e, page, true), ["1|mcqf!", "2|icf!"]);
}

// INSERT ... SELECT: subqueries over any table, and a source that reads
// the target table sees it as it was before the statement.

#[test]
fn insert_select_with_subqueries_over_tables_outside_the_source() {
    let (_db, mut e) = fixture();
    for sql in [
        "CREATE TABLE d (id INTEGER, t TEXT)",
        "INSERT INTO d SELECT a.id, b.t FROM a JOIN b ON b.k = a.k \
         WHERE NOT EXISTS (SELECT 1 FROM c WHERE c.id = a.id AND c.t = b.t)",
        "INSERT INTO d SELECT a.id + 100, b.t FROM a JOIN b ON b.k = a.k \
         WHERE b.t IN (SELECT t FROM c)",
        "INSERT INTO d SELECT a.id + 200, (SELECT max(t) FROM c WHERE c.id = a.id) \
         FROM a JOIN b ON b.k = a.k",
        "INSERT INTO d SELECT a.id + 300, k FROM a WHERE EXISTS (SELECT 1 FROM c WHERE c.id = a.id)",
        "INSERT INTO d SELECT a.id + 400, (SELECT count(*) FROM b WHERE b.k = a.k) FROM a",
        "INSERT INTO d SELECT a.id + 500, b.t || '!' FROM a JOIN b ON b.k = a.k \
         ORDER BY 2 DESC LIMIT 1",
    ] {
        e.run_query(sql).unwrap_or_else(|err| panic!("{sql}: {err}"));
    }
    assert_eq!(
        rows(&mut e, "SELECT * FROM d ORDER BY 1, 2", true),
        [
            "1|mcqf",
            "2|icf",
            "101|icf",
            "102|icf",
            "201|icf",
            "201|icf",
            "202|other",
            "301|x",
            "302|y",
            "401|2",
            "402|1",
            "403|0",
            "501|mcqf!"
        ]
    );
}

#[test]
fn insert_select_whose_subquery_reads_the_target_sees_it_before_the_statement() {
    let (_db, mut e) = fixture();
    for sql in [
        "CREATE TABLE e (id INTEGER, t TEXT)",
        "INSERT INTO e VALUES (1, 'icf')",
        "INSERT INTO e SELECT a.id, b.t FROM a JOIN b ON b.k = a.k \
         WHERE NOT EXISTS (SELECT 1 FROM e WHERE e.id = a.id AND e.t = b.t)",
        // sqlite3: 3 on every row, the count before the statement.
        "INSERT INTO e SELECT id + 10, (SELECT count(*) FROM e) FROM a \
         WHERE NOT EXISTS (SELECT 1 FROM e AS x WHERE x.id = a.id + 10)",
    ] {
        e.run_query(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err}"));
    }
    assert_eq!(
        rows(&mut e, "SELECT * FROM e ORDER BY 1, 2", true),
        ["1|icf", "1|mcqf", "2|icf", "11|3", "12|3", "13|3"]
    );
}

#[test]
fn insert_select_from_the_target_across_page_splits_copies_each_row_once() {
    // On main this lost rows: the scan read pages the insert was splitting.
    let (_db, mut e) = fixture();
    e.run_query("CREATE TABLE g (id INTEGER, pad TEXT)")
        .unwrap();
    e.run_query("INSERT INTO g SELECT a.id, b.t FROM a, b")
        .unwrap();
    for step in 1..=9 {
        let sql = format!("INSERT INTO g SELECT id + {}, pad FROM g", 10_i64.pow(step));
        e.run_query(&sql)
            .unwrap_or_else(|err| panic!("{sql}: {err}"));
    }
    assert_eq!(
        rows(&mut e, "SELECT count(*), sum(id) FROM g", true),
        ["6144|3413333342208"]
    );
}

#[test]
fn insert_select_from_the_target_with_an_index_upsert_and_limit() {
    let (_db, mut e) = fixture();
    for sql in [
        "CREATE TABLE h (id INTEGER PRIMARY KEY, v TEXT UNIQUE, n INTEGER)",
        "CREATE INDEX h_n ON h(n)",
        "INSERT INTO h(v, n) SELECT b.t || a.id, a.id FROM a JOIN b ON b.k = a.k",
        "INSERT INTO h(v, n) SELECT v || '+', n + (SELECT max(n) FROM h) FROM h \
         ORDER BY v DESC LIMIT 2",
    ] {
        e.run_query(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err}"));
    }
    // Two new rows would both be `icf#`: the whole statement fails.
    let err = e
        .run_query(
            "INSERT INTO h(v, n) SELECT b.t || '#', a.id FROM a JOIN b \
             ON b.k = a.k AND a.id IN (SELECT n FROM h)",
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("UNIQUE constraint failed: h.v"),
        "{err}"
    );
    for sql in [
        "INSERT INTO h(v, n) SELECT v, 99 FROM h WHERE n = 1 \
         ON CONFLICT(v) DO UPDATE SET n = excluded.n + h.n",
        "INSERT OR IGNORE INTO h(v, n) SELECT v, -1 FROM h",
    ] {
        e.run_query(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err}"));
    }
    assert_eq!(
        rows(&mut e, "SELECT * FROM h ORDER BY id", true),
        [
            "1|icf1|100",
            "2|mcqf1|100",
            "3|icf2|2",
            "4|mcqf1+|3",
            "5|icf2+|4"
        ]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT n, count(*) FROM h WHERE n > 0 GROUP BY n ORDER BY n",
            true
        ),
        ["2|1", "3|1", "4|1", "100|2"]
    );
}

#[test]
fn insert_select_from_the_target_with_autoincrement() {
    let (_db, mut e) = fixture();
    for sql in [
        "CREATE TABLE z (id INTEGER PRIMARY KEY AUTOINCREMENT, v INTEGER)",
        "INSERT INTO z(v) VALUES (1), (2)",
        "INSERT INTO z(v) SELECT v + (SELECT count(*) FROM z) FROM z",
    ] {
        e.run_query(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err}"));
    }
    assert_eq!(
        rows(&mut e, "SELECT * FROM z ORDER BY id", true),
        ["1|1", "2|2", "3|3", "4|4"]
    );
}

// #939: INSERT ... SELECT from sources sqlite3 runs beyond a plain table or
// join. Each test starts from the fixture plus a view and a seeded target;
// the rows are sqlite3 3.54.1's for the same statements.

/// The fixture plus `vb` (a view over `b`) and `d` holding two seed rows.
fn insert_fixture() -> (TempDb, RowEngine) {
    let (db, mut e) = fixture();
    for sql in [
        "CREATE VIEW vb AS SELECT k, t || '~' AS t FROM b",
        "CREATE TABLE d (id INTEGER, t TEXT)",
        "INSERT INTO d VALUES (1, 'seed'), (2, 'seed2')",
    ] {
        e.run_query(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err}"));
    }
    (db, e)
}

/// Runs `statements` against [`insert_fixture`], then returns `d` in order.
fn insert_rows(statements: &[&str]) -> Vec<String> {
    let (_db, mut e) = insert_fixture();
    for sql in statements {
        e.run_query(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err}"));
    }
    rows(&mut e, "SELECT * FROM d ORDER BY 1, 2", true)
}

#[test]
fn insert_select_from_a_from_subquery() {
    assert_eq!(
        insert_rows(&[
            "INSERT INTO d SELECT x.id, x.k FROM (SELECT id, k FROM a WHERE id > 1) AS x"
        ]),
        ["1|seed", "2|seed2", "2|y", "3|z"]
    );
}

#[test]
fn insert_select_from_a_view() {
    assert_eq!(
        insert_rows(&["INSERT INTO d SELECT 10, t FROM vb WHERE k = 'x'"]),
        ["1|seed", "2|seed2", "10|icf~", "10|mcqf~"]
    );
}

#[test]
fn insert_select_from_a_cte_inside_the_insert() {
    assert_eq!(
        insert_rows(&[
            "INSERT INTO d WITH w AS (SELECT id + 1000 AS id, t FROM c) SELECT id, t FROM w"
        ]),
        ["1|seed", "2|seed2", "1001|icf", "1002|other"]
    );
}

#[test]
fn insert_select_joining_a_subquery_and_a_view() {
    assert_eq!(
        insert_rows(&[
            "INSERT INTO d SELECT a.id + 2000, s.t FROM a \
             JOIN (SELECT k, max(t) AS t FROM b GROUP BY k) AS s ON s.k = a.k",
            "INSERT INTO d SELECT a.id + 3000, v.t FROM a JOIN vb AS v ON v.k = a.k",
        ]),
        [
            "1|seed",
            "2|seed2",
            "2001|mcqf",
            "2002|icf",
            "3001|icf~",
            "3001|mcqf~",
            "3002|icf~"
        ]
    );
}

#[test]
fn insert_select_from_a_subquery_over_the_target_sees_it_before_the_statement() {
    assert_eq!(
        insert_rows(&[
            "INSERT INTO d SELECT x.id + 7000, x.t FROM (SELECT id, t FROM d WHERE id < 10) AS x",
            "INSERT INTO d SELECT count(*) + 8000, 'self' FROM (SELECT * FROM d)",
        ]),
        ["1|seed", "2|seed2", "7001|seed", "7002|seed2", "8004|self"]
    );
}

#[test]
fn insert_select_from_nested_and_limited_subqueries() {
    assert_eq!(
        insert_rows(&[
            "INSERT INTO d SELECT * FROM (SELECT 9500, 'nested' FROM (SELECT 1))",
            "INSERT INTO d SELECT x.id + 100, x.t FROM \
             (SELECT id, t FROM d ORDER BY id DESC LIMIT 1) AS x",
        ]),
        ["1|seed", "2|seed2", "9500|nested", "9600|nested"]
    );
}

#[test]
fn insert_select_without_from() {
    let (_db, mut e) = fixture();
    for sql in [
        "CREATE TABLE p (id INTEGER PRIMARY KEY, v TEXT UNIQUE, n REAL)",
        "INSERT INTO p SELECT 1, 'a', 2",
        "INSERT INTO p(v) SELECT 'b'",
        // Reads the target: sees it before the statement.
        "INSERT INTO p(v, n) SELECT (SELECT max(v) FROM p) || '+', (SELECT count(*) FROM p)",
        "INSERT INTO p(v, n) SELECT NULL, '3.5'",
        "INSERT OR IGNORE INTO p(v, n) SELECT 'a', 99",
        "INSERT INTO p(v, n) SELECT 'a', 7 ON CONFLICT(v) DO UPDATE SET n = excluded.n + p.n",
    ] {
        e.run_query(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err}"));
    }
    let err = e
        .run_query("INSERT INTO p(v, n) SELECT 'b', 1")
        .unwrap_err();
    assert!(
        err.to_string().contains("UNIQUE constraint failed: p.v"),
        "{err}"
    );
    assert!(e.run_query("INSERT INTO p(v) SELECT 'x', 'y'").is_err());
    e.run_query(
        "INSERT INTO p(v, n) SELECT (SELECT max(k) || min(k) FROM a), (SELECT sum(id) FROM a)",
    )
    .unwrap();
    assert_eq!(
        rows(
            &mut e,
            "SELECT id, v, n, typeof(n) FROM p ORDER BY id",
            true
        ),
        [
            "1|a|9.0|real",
            "2|b|NULL|null",
            "3|b+|2.0|real",
            "4|NULL|3.5|real",
            "5|zx|6.0|real"
        ]
    );
}

#[test]
fn insert_select_without_from_with_where_inserts_only_when_it_holds() {
    let (_db, mut e) = fixture();
    for sql in [
        "CREATE TABLE p (id INTEGER PRIMARY KEY, v TEXT UNIQUE, n REAL)",
        "INSERT INTO p(v) VALUES ('a'), ('b'), ('c'), ('d')",
        "INSERT INTO p(v) SELECT 'w' WHERE 1",
        "INSERT INTO p(v) SELECT 'never' WHERE 0",
        "INSERT INTO p(v) SELECT 'nul' WHERE NULL",
        // sqlite3's insert-if-missing idiom: the second one inserts nothing.
        "INSERT INTO p(v, n) SELECT 'new', 1 WHERE NOT EXISTS (SELECT 1 FROM p WHERE v = 'new')",
        "INSERT INTO p(v, n) SELECT 'new', 2 WHERE NOT EXISTS (SELECT 1 FROM p WHERE v = 'new')",
        "INSERT INTO p(v, n) SELECT 'cnt', (SELECT count(*) FROM p) WHERE (SELECT count(*) FROM p) > 3",
    ] {
        e.run_query(sql).unwrap_or_else(|err| panic!("{sql}: {err}"));
    }
    assert_eq!(
        rows(
            &mut e,
            "SELECT id, v, n FROM p WHERE id > 4 ORDER BY id",
            true
        ),
        ["5|w|NULL", "6|new|1.0", "7|cnt|6.0"]
    );
}

#[test]
fn select_without_from_reads_tables_in_its_subqueries() {
    let (_db, mut e) = fixture();
    assert_eq!(
        rows(
            &mut e,
            "SELECT (SELECT count(*) FROM c), (SELECT max(t) FROM b)",
            true
        ),
        ["2|orphan"]
    );
}

#[test]
fn insert_select_from_union_all_and_union() {
    assert_eq!(
        insert_rows(&["INSERT INTO d SELECT id, k FROM a UNION ALL SELECT id, t FROM c"]),
        ["1|icf", "1|seed", "1|x", "2|other", "2|seed2", "2|y", "3|z"]
    );
    // UNION dedups across arms: `x` and `y` come from both.
    assert_eq!(
        insert_rows(&["INSERT INTO d SELECT 50, k FROM b UNION SELECT 50, k FROM a"]),
        ["1|seed", "2|seed2", "50|w", "50|x", "50|y", "50|z"]
    );
    assert_eq!(
        insert_rows(&["INSERT INTO d SELECT 600, 'v1' UNION ALL SELECT 601, 'v2'"]),
        ["1|seed", "2|seed2", "600|v1", "601|v2"]
    );
}

#[test]
fn insert_select_from_a_union_with_order_by_and_limit() {
    assert_eq!(
        insert_rows(&[
            "INSERT INTO d SELECT id + 100, k FROM a UNION ALL SELECT id + 100, t FROM c \
             ORDER BY 1 DESC, 2 LIMIT 3 OFFSET 1"
        ]),
        ["1|seed", "2|seed2", "101|icf", "102|other", "102|y"]
    );
}

#[test]
fn insert_select_from_a_union_over_a_subquery_and_a_view() {
    assert_eq!(
        insert_rows(&[
            "INSERT INTO d SELECT x.id + 300, x.k FROM (SELECT id, k FROM a WHERE id < 3) AS x \
             UNION ALL SELECT 300, t FROM vb WHERE k = 'y'"
        ]),
        ["1|seed", "2|seed2", "300|icf~", "301|x", "302|y"]
    );
}

#[test]
fn insert_select_from_a_union_over_the_target_sees_it_before_the_statement() {
    assert_eq!(
        insert_rows(&["INSERT INTO d SELECT id + 400, t FROM d \
             UNION ALL SELECT (SELECT count(*) FROM d) + 400, 'n' FROM a WHERE id = 1"]),
        ["1|seed", "2|seed2", "401|seed", "402|n", "402|seed2"]
    );
    assert_eq!(
        insert_rows(&["INSERT INTO d SELECT id, t FROM d UNION SELECT id, t FROM d"]),
        ["1|seed", "1|seed", "2|seed2", "2|seed2"]
    );
}

#[test]
fn insert_select_union_dedup_decides_a_unique_constraint() {
    let (_db, mut e) = fixture();
    e.run_query("CREATE TABLE u (t TEXT UNIQUE)").unwrap();
    // `icf` is in both b and c: UNION keeps one, so no conflict.
    e.run_query("INSERT INTO u SELECT t FROM b UNION SELECT t FROM c")
        .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT * FROM u ORDER BY 1", true),
        ["icf", "mcqf", "orphan", "other"]
    );
    let err = e
        .run_query("INSERT INTO u SELECT t FROM b UNION ALL SELECT 'zz' FROM a")
        .unwrap_err();
    assert!(
        err.to_string().contains("UNIQUE constraint failed: u.t"),
        "{err}"
    );
    assert_eq!(rows(&mut e, "SELECT count(*) FROM u", true), ["4"]);
}

#[test]
fn union_order_by_names_output_columns_by_position_alias_or_first_arm_column() {
    let (_db, mut e) = fixture();
    for (sql, expected) in [
        (
            "SELECT id + 100, k FROM a UNION ALL SELECT id, t FROM c ORDER BY 1 DESC",
            &["103|z", "102|y", "101|x", "2|other", "1|icf"][..],
        ),
        (
            "SELECT k AS z FROM a UNION SELECT t FROM c ORDER BY z DESC",
            &["z", "y", "x", "other", "icf"][..],
        ),
        (
            "SELECT id * 2 AS d, k FROM a UNION ALL SELECT id, t FROM c ORDER BY d, 2",
            &["1|icf", "2|other", "2|x", "4|y", "6|z"][..],
        ),
        (
            "SELECT upper(k) FROM a UNION SELECT t FROM c ORDER BY 1",
            &["X", "Y", "Z", "icf", "other"][..],
        ),
        (
            "SELECT id AS n, k FROM a UNION ALL SELECT id, t FROM c ORDER BY n DESC LIMIT 2",
            &["3|z", "2|y"][..],
        ),
    ] {
        assert_eq!(rows(&mut e, sql, true), expected, "{sql}");
    }
}

#[test]
fn with_before_insert_feeds_its_select() {
    assert_eq!(
        insert_rows(&[
            "WITH w AS (SELECT id * 100 AS id, k FROM a) INSERT INTO d SELECT id, k FROM w"
        ]),
        ["1|seed", "2|seed2", "100|x", "200|y", "300|z"]
    );
    assert_eq!(
        insert_rows(&["WITH w AS (SELECT id, t FROM c) \
             INSERT INTO d SELECT w1.id + 1000, w2.t FROM w AS w1 JOIN w AS w2 ON w2.id = w1.id"]),
        ["1|seed", "2|seed2", "1001|icf", "1002|other"]
    );
    assert_eq!(
        insert_rows(&[
            "WITH x AS (SELECT id FROM a WHERE id > 1), y AS (SELECT id + 2000 AS id FROM x) \
             INSERT INTO d SELECT id, 'chain' FROM y"
        ]),
        ["1|seed", "2|seed2", "2002|chain", "2003|chain"]
    );
}

#[test]
fn with_before_insert_and_in_its_select_scope_like_sqlite() {
    // The SELECT's own CTE uses the outer one; a name defined in both
    // means the SELECT's own inside it.
    assert_eq!(
        insert_rows(&[
            "WITH o AS (SELECT k FROM a WHERE id = 3) \
             INSERT INTO d WITH i AS (SELECT k || '!' AS k FROM o) SELECT 3000, k FROM i",
            "WITH o AS (SELECT 1 AS v) \
             INSERT INTO d WITH o AS (SELECT 2 AS v) SELECT 3500 + v, 'clash' FROM o",
        ]),
        ["1|seed", "2|seed2", "3000|z!", "3502|clash"]
    );
}

#[test]
fn with_before_insert_over_the_target_sees_it_before_the_statement() {
    assert_eq!(
        insert_rows(&[
            "WITH n AS (SELECT count(*) AS n FROM d) INSERT INTO d SELECT 4000 + n, 'self' FROM n, a",
            "WITH s AS (SELECT * FROM d WHERE id < 10) \
             INSERT OR IGNORE INTO d SELECT id + 4500, t FROM s ORDER BY id DESC LIMIT 1",
        ]),
        ["1|seed", "2|seed2", "4002|self", "4002|self", "4002|self", "4502|seed2"]
    );
}

#[test]
fn with_before_values_update_or_delete_is_still_refused() {
    let (_db, mut e) = insert_fixture();
    for sql in [
        "WITH v AS (SELECT 1) INSERT INTO d VALUES (6000, 'values')",
        "WITH w AS (SELECT 1) UPDATE d SET t = 'x' WHERE id = 1",
        "WITH w AS (SELECT 1) DELETE FROM d WHERE id = 1",
    ] {
        let err = e.run_query(sql).unwrap_err();
        assert!(
            err.to_string().contains("not yet supported"),
            "{sql}: {err}"
        );
    }
    assert_eq!(
        rows(
            &mut e,
            "WITH w AS (SELECT id FROM a) SELECT count(*) FROM w",
            true
        ),
        ["3"]
    );
    assert_eq!(
        rows(&mut e, "SELECT * FROM d ORDER BY 1", true),
        ["1|seed", "2|seed2"]
    );
}

// #940: a SELECT core without FROM reads one row, as in sqlite3: with
// WHERE, GROUP BY/HAVING, DISTINCT, ORDER BY/LIMIT, as an aggregate, and
// as a UNION arm. The rows are sqlite3 3.54.1's.

#[test]
fn select_without_from_with_clauses_reads_one_row() {
    let (_db, mut e) = fixture();
    for (sql, expected) in [
        ("SELECT 1 WHERE EXISTS (SELECT 1 FROM a)", &["1"][..]),
        ("SELECT 1 WHERE NOT EXISTS (SELECT 1 FROM a)", &[][..]),
        (
            "SELECT 'x', (SELECT count(*) FROM c) WHERE (SELECT count(*) FROM b) > 3",
            &["x|2"][..],
        ),
        ("SELECT 1 WHERE NULL", &[][..]),
        ("SELECT count(*), max(3) WHERE 1", &["1|3"][..]),
        ("SELECT count(*) WHERE 0", &["0"][..]),
        ("SELECT sum(5), min('a') WHERE 0", &["NULL|NULL"][..]),
        ("SELECT DISTINCT 7", &["7"][..]),
        ("SELECT 3 ORDER BY 1 LIMIT 0", &[][..]),
        ("SELECT 4 AS v GROUP BY 1 HAVING v > 3", &["4"][..]),
        ("SELECT 5 LIMIT 1 OFFSET 1", &[][..]),
        ("SELECT 1, 'two', NULL", &["1|two|NULL"][..]),
    ] {
        assert_eq!(rows(&mut e, sql, true), expected, "{sql}");
    }
    // sqlite3: "no tables specified".
    assert!(e.run_query("SELECT * WHERE 1").is_err());
}

#[test]
fn union_with_select_without_from_arms() {
    let (_db, mut e) = fixture();
    for (sql, expected) in [
        ("SELECT 1 UNION ALL SELECT 2", &["1", "2"][..]),
        (
            "SELECT 1 UNION SELECT 1 UNION SELECT 2 ORDER BY 1 DESC",
            &["2", "1"][..],
        ),
        ("SELECT 2 UNION ALL SELECT 1 ORDER BY 1 LIMIT 1", &["1"][..]),
        (
            "SELECT k FROM a UNION ALL SELECT 'lit'",
            &["x", "y", "z", "lit"][..],
        ),
        (
            "SELECT 'first' UNION ALL SELECT k FROM a ORDER BY 1",
            &["first", "x", "y", "z"][..],
        ),
        (
            "SELECT k FROM a UNION SELECT 'x' ORDER BY 1",
            &["x", "y", "z"][..],
        ),
        (
            "SELECT abs(-2) + 1 AS n WHERE 1 UNION ALL SELECT id FROM c WHERE id > 1 ORDER BY n",
            &["2", "3"][..],
        ),
    ] {
        assert_eq!(rows(&mut e, sql, true), expected, "{sql}");
    }
}

// #940: a UNION as the body of a CTE, a view or a FROM-subquery runs every
// arm (UNION's dedup, its ORDER BY/LIMIT, FROM-less arms). sqlite3's rows.

#[test]
fn union_bodies_of_ctes_views_and_derived_tables() {
    let (_db, mut e) = fixture();
    e.run_query("CREATE VIEW vu AS SELECT k AS name FROM a UNION SELECT t FROM c")
        .unwrap();
    for (sql, expected) in [
        ("WITH u AS (SELECT t FROM b UNION SELECT t FROM c) SELECT count(*) FROM u", &["4"][..]),
        (
            "WITH u AS (SELECT k, t FROM b UNION ALL SELECT 'zz', t FROM c) \
             SELECT k, count(*) FROM u GROUP BY k ORDER BY k",
            &["w|1", "x|2", "y|1", "zz|2"][..],
        ),
        (
            "SELECT x FROM (SELECT t AS x FROM b UNION SELECT t FROM c ORDER BY 1 LIMIT 3) ORDER BY x",
            &["icf", "mcqf", "orphan"][..],
        ),
        (
            "SELECT x FROM (SELECT t AS x FROM b UNION ALL SELECT t FROM c) WHERE x > 'm' ORDER BY x",
            &["mcqf", "orphan", "other"][..],
        ),
        (
            "WITH v AS (SELECT 1 AS n UNION SELECT 1 UNION SELECT 2) SELECT n FROM v ORDER BY n",
            &["1", "2"][..],
        ),
        (
            "WITH u AS (SELECT id FROM a UNION SELECT id FROM c) \
             SELECT a.k, u.id FROM u JOIN a ON a.id = u.id ORDER BY 2",
            &["x|1", "y|2", "z|3"][..],
        ),
        (
            "WITH u AS (SELECT t FROM b UNION SELECT t FROM c), \
             w AS (SELECT t FROM u WHERE t <> 'icf') SELECT count(*), min(t) FROM w",
            &["3|mcqf"][..],
        ),
        ("SELECT name FROM vu ORDER BY name", &["icf", "other", "x", "y", "z"][..]),
        ("SELECT count(*) FROM vu WHERE name IN (SELECT t FROM b)", &["1"][..]),
    ] {
        assert_eq!(rows(&mut e, sql, true), expected, "{sql}");
    }
}

#[test]
fn insert_select_from_union_bodies_including_one_over_the_target() {
    let (_db, mut e) = fixture();
    for sql in [
        "CREATE TABLE d (id INTEGER, t TEXT)",
        "WITH u AS (SELECT t FROM b UNION SELECT t FROM c) INSERT INTO d SELECT 5000, t FROM u",
        // Reads `d` through a UNION derived table: sees it before the
        // statement (4 rows), plus `c`'s 2.
        "INSERT INTO d SELECT x.id + 100, x.t FROM \
         (SELECT id, t FROM d UNION ALL SELECT id, t FROM c) AS x",
    ] {
        e.run_query(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err}"));
    }
    assert_eq!(
        rows(&mut e, "SELECT * FROM d ORDER BY 1, 2", true),
        [
            "101|icf",
            "102|other",
            "5000|icf",
            "5000|mcqf",
            "5000|orphan",
            "5000|other",
            "5100|icf",
            "5100|mcqf",
            "5100|orphan",
            "5100|other"
        ]
    );
}
