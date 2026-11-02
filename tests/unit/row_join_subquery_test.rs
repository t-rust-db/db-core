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
