// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Row engine `ATTACH`/`DETACH` (#924, ADR 0043): queries and writes
//! across `main` and attached files. Expectations are pinned from sqlite3
//! 3.51 on the same statements and data, so no sqlite3 binary is needed.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test code fails fast (db-core#230); clippy.toml's allow-*-in-tests does not reach helper fns outside #[test]"
)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use db_core::engine::row::RowEngine;
use db_core::engine::{Cell, Engine, ErrorKind};

/// A scratch directory with `m.db` (`t`: 1 m1, 2 m2; `only_main`) and
/// `x.db` (`t`: 1 x1, 3 x3; `u` indexed on `b`: 1 u1, 2 u2, 3 u3),
/// removed on drop.
struct Files(PathBuf);

impl Files {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "db-core-row-attach-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let files = Files(dir);
        create(
            &files.path("m.db"),
            "CREATE TABLE t(id INTEGER PRIMARY KEY, a TEXT);\
             INSERT INTO t VALUES (1, 'm1');\
             INSERT INTO t VALUES (2, 'm2');\
             CREATE TABLE only_main(k);",
        );
        create(
            &files.path("x.db"),
            "CREATE TABLE t(id INTEGER PRIMARY KEY, a TEXT);\
             INSERT INTO t VALUES (1, 'x1');\
             INSERT INTO t VALUES (3, 'x3');\
             CREATE TABLE u(id INTEGER PRIMARY KEY, b TEXT);\
             CREATE INDEX u_b ON u(b);\
             INSERT INTO u VALUES (1, 'u1');\
             INSERT INTO u VALUES (2, 'u2');\
             INSERT INTO u VALUES (3, 'u3');",
        );
        files
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    /// `m.db` with `x.db` attached as `x`.
    fn main_with_x(&self) -> RowEngine {
        let mut e = RowEngine::open(&self.path("m.db")).unwrap();
        e.run_query(&format!("ATTACH '{}' AS x", self.path("x.db").display()))
            .unwrap();
        e
    }
}

impl Drop for Files {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn create(path: &Path, schema: &str) {
    RowEngine::create(path, Default::default(), schema).expect("create");
}

fn rows(e: &mut RowEngine, sql: &str) -> Vec<Vec<Cell>> {
    e.run_query(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err}"))
        .rows
}

fn error(e: &mut RowEngine, sql: &str) -> String {
    e.run_query(sql).expect_err(sql).to_string()
}

fn text(s: &str) -> Cell {
    Cell::Text(s.into())
}

fn plan(e: &RowEngine, sql: &str) -> Vec<String> {
    e.explain_plan(sql)
        .unwrap()
        .into_iter()
        .map(|r| r.detail)
        .collect()
}

#[test]
fn a_join_reads_main_and_an_attached_file() {
    let f = Files::new();
    let mut e = f.main_with_x();
    let want = vec![vec![text("m1"), text("u1")], vec![text("m2"), text("u2")]];
    for sql in [
        "SELECT t.a, u.b FROM main.t JOIN x.u ON t.id = u.id ORDER BY t.id",
        "SELECT t.a, u.b FROM t JOIN u ON t.id = u.id ORDER BY t.id",
        "SELECT t.a, x.u.b FROM main.t JOIN x.u ON main.t.id = x.u.id ORDER BY t.id",
    ] {
        assert_eq!(rows(&mut e, sql), want, "{sql}");
    }
    assert_eq!(
        rows(
            &mut e,
            "SELECT m.a, a2.a FROM main.t AS m JOIN x.t AS a2 ON m.id = a2.id"
        ),
        vec![vec![text("m1"), text("x1")]]
    );
}

#[test]
fn a_bare_name_finds_main_first_then_the_attached_files() {
    let f = Files::new();
    let mut e = f.main_with_x();
    assert_eq!(
        rows(&mut e, "SELECT a FROM t ORDER BY id"),
        vec![vec![text("m1")], vec![text("m2")]]
    );
    assert_eq!(
        rows(&mut e, "SELECT a FROM x.t ORDER BY id"),
        vec![vec![text("x1")], vec![text("x3")]]
    );
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM u"),
        vec![vec![Cell::Int(3)]]
    );
    assert_eq!(
        rows(&mut e, "SELECT x.u.b FROM x.u WHERE x.u.id = 2"),
        vec![vec![text("u2")]]
    );
}

#[test]
fn an_attached_index_is_searched() {
    let f = Files::new();
    let mut e = f.main_with_x();
    assert_eq!(
        rows(&mut e, "SELECT id FROM x.u WHERE b = 'u3'"),
        vec![vec![Cell::Int(3)]]
    );
    assert_eq!(
        plan(&e, "SELECT b FROM x.u WHERE b = 'u3'"),
        // sqlite3 says `USING COVERING INDEX` here; db-core's row engine
        // says `USING INDEX` for this shape on `main` as well.
        vec!["SEARCH x.u USING INDEX u_b (b=?)"]
    );
    let join = plan(&e, "SELECT t.a, u.b FROM t JOIN x.u ON t.id = u.id");
    assert_eq!(
        join,
        vec!["SCAN t", "SEARCH x.u USING INTEGER PRIMARY KEY (rowid=?)"]
    );
}

#[test]
fn writes_to_an_attached_file_land_in_that_file() {
    let f = Files::new();
    let mut e = f.main_with_x();
    e.run_query(
        "INSERT INTO x.u VALUES (4, 'u4');\
         UPDATE x.t SET a = 'x3!' WHERE id = 3;\
         DELETE FROM x.u WHERE id = 1;",
    )
    .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT id, b FROM x.u ORDER BY id"),
        vec![
            vec![Cell::Int(2), text("u2")],
            vec![Cell::Int(3), text("u3")],
            vec![Cell::Int(4), text("u4")],
        ]
    );
    assert_eq!(
        rows(&mut e, "SELECT id FROM x.u WHERE b = 'u4'"),
        vec![vec![Cell::Int(4)]],
        "the index on u was maintained"
    );
    assert_eq!(
        rows(&mut e, "SELECT a FROM t ORDER BY id"),
        vec![vec![text("m1")], vec![text("m2")]],
        "main's t is untouched"
    );
    drop(e);
    let mut x = RowEngine::open(&f.path("x.db")).unwrap();
    assert_eq!(
        rows(&mut x, "SELECT a FROM t ORDER BY id"),
        vec![vec![text("x1")], vec![text("x3!")]]
    );
    assert_eq!(
        rows(&mut x, "PRAGMA integrity_check"),
        vec![vec![text("ok")]]
    );
}

#[test]
fn a_transaction_commits_and_rolls_back_every_file() {
    let f = Files::new();
    let mut e = f.main_with_x();
    let counts = "SELECT count(*), (SELECT count(*) FROM x.t) FROM main.t";
    e.run_query(
        "BEGIN; INSERT INTO t VALUES (5, 'm5'); INSERT INTO x.t VALUES (5, 'x5'); ROLLBACK;",
    )
    .unwrap();
    assert_eq!(rows(&mut e, counts), vec![vec![Cell::Int(2), Cell::Int(2)]]);
    e.run_query("BEGIN; INSERT INTO t VALUES (6, 'm6'); INSERT INTO x.t VALUES (6, 'x6'); COMMIT;")
        .unwrap();
    assert_eq!(rows(&mut e, counts), vec![vec![Cell::Int(3), Cell::Int(3)]]);
    drop(e);
    let mut x = RowEngine::open(&f.path("x.db")).unwrap();
    assert_eq!(
        rows(&mut x, "SELECT count(*) FROM t"),
        vec![vec![Cell::Int(3)]]
    );
}

#[test]
fn a_failed_statement_leaves_the_attached_file_as_it_was() {
    let f = Files::new();
    let mut e = f.main_with_x();
    // The second row collides with id 3 after the first went in.
    let err = error(&mut e, "INSERT INTO x.t VALUES (7, 'x7'), (3, 'dup')");
    assert!(err.contains("UNIQUE"), "{err}");
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM x.t"),
        vec![vec![Cell::Int(2)]]
    );
}

#[test]
fn main_and_temp_name_their_schemas() {
    let f = Files::new();
    let mut e = f.main_with_x();
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM main.only_main"),
        vec![vec![Cell::Int(0)]]
    );
    assert!(error(&mut e, "SELECT a FROM temp.t").contains("no such table: temp.t"));
    assert!(error(&mut e, "SELECT a FROM y.t").contains("no such table: y.t"));
    assert!(error(&mut e, "SELECT * FROM x.only_main").contains("no such table: x.only_main"));
}

#[test]
fn attach_and_detach_follow_sqlite3() {
    let f = Files::new();
    let mut e = f.main_with_x();
    let x = f.path("x.db");
    for (sql, want) in [
        (
            format!("ATTACH '{}' AS x", x.display()),
            "database x is already in use",
        ),
        (
            format!("ATTACH '{}' AS main", x.display()),
            "database main is already in use",
        ),
        (
            format!("ATTACH DATABASE '{}' AS temp", x.display()),
            "database temp is already in use",
        ),
        ("DETACH y".to_string(), "no such database: y"),
        ("DETACH main".to_string(), "cannot detach database main"),
    ] {
        let got = error(&mut e, &sql);
        assert!(got.contains(want), "{sql}: {got}");
    }
    e.run_query("BEGIN").unwrap();
    assert!(error(&mut e, "DETACH x").contains("database x is locked"));
    e.run_query("COMMIT").unwrap();
    e.run_query("DETACH DATABASE x").unwrap();
    assert!(error(&mut e, "SELECT * FROM x.u").contains("no such table: x.u"));
    let mut too_many = RowEngine::open(&f.path("m.db")).unwrap();
    for i in 0..10 {
        too_many
            .run_query(&format!("ATTACH '{}' AS d{i}", x.display()))
            .unwrap();
    }
    let got = error(&mut too_many, &format!("ATTACH '{}' AS d10", x.display()));
    assert!(
        got.contains("too many attached databases - max 10"),
        "{got}"
    );
}

#[test]
fn attaching_a_missing_file_creates_it() {
    let f = Files::new();
    let mut e = RowEngine::open(&f.path("m.db")).unwrap();
    let fresh = f.path("fresh.db");
    e.run_query(&format!(
        "ATTACH '{}' AS n; INSERT INTO t SELECT id + 10, a FROM t",
        fresh.display()
    ))
    .unwrap();
    assert!(fresh.exists());
    let err = e.run_query("ATTACH 1 AS k").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported, "{err}");
}

// codegen_row_select_eqp_eqp_display_name_a007f951 (#924):
// alias != unqualified(name) || alias == name -- a written alias shows as
// `name AS alias`; an attached table's implied alias (its bare name) does not.
#[test]
#[allow(non_snake_case)]
fn mcdc__codegen_row_select_eqp_eqp_display_name_a007f951__v1_written_alias_is_shown() {
    let f = Files::new();
    let e = f.main_with_x();
    assert_eq!(plan(&e, "SELECT b FROM x.u AS w"), vec!["SCAN x.u AS w"]);
}

#[test]
#[allow(non_snake_case)]
fn mcdc__codegen_row_select_eqp_eqp_display_name_a007f951__v2_alias_equal_to_the_name_is_shown() {
    let f = Files::new();
    let e = f.main_with_x();
    assert_eq!(plan(&e, "SELECT a FROM t AS t"), vec!["SCAN t AS t"]);
}

#[test]
#[allow(non_snake_case)]
fn mcdc__codegen_row_select_eqp_eqp_display_name_a007f951__v3_implied_alias_is_not_shown() {
    let f = Files::new();
    let e = f.main_with_x();
    assert_eq!(plan(&e, "SELECT b FROM x.u"), vec!["SCAN x.u"]);
}
