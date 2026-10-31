// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! `RowEngine::run_query_with` (#915): `?`/`?N` parameters are data, never
//! SQL text, and a wrong parameter count fails before anything runs.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test code fails fast (db-core#230)"
)]

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};

use db_core::engine::row::{CreateOptions, RowEngine};
use db_core::engine::{Cell, Engine, ErrorKind};
use db_core::value::Value;

static NEXT: AtomicU32 = AtomicU32::new(0);

struct TempDb(PathBuf);

impl TempDb {
    fn engine() -> (TempDb, RowEngine) {
        let path = std::env::temp_dir().join(format!(
            "db-core-params-{}-{}.db",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let engine = RowEngine::create(
            &path,
            CreateOptions::default(),
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT, n INTEGER)",
        )
        .unwrap();
        (TempDb(path), engine)
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
        std::fs::remove_file(format!("{}-journal", self.0.display())).ok();
    }
}

fn text(s: &str) -> Value {
    Value::Text(Rc::from(s))
}

fn count(e: &mut RowEngine) -> Cell {
    e.run_query("SELECT COUNT(*) FROM t").unwrap().rows[0][0].clone()
}

#[test]
fn a_text_parameter_with_a_quote_and_a_semicolon_is_stored_as_data() {
    let (_db, mut e) = TempDb::engine();
    let nasty = "x'); DROP TABLE t; --";
    e.run_query_with(
        "INSERT INTO t(v, n) VALUES (?, ?)",
        &[text(nasty), Value::Integer(7)],
    )
    .unwrap();
    let rows = e.run_query("SELECT v, n FROM t").unwrap().rows;
    assert_eq!(
        rows,
        vec![vec![Cell::Text(nasty.to_string()), Cell::Int(7)]]
    );
}

#[test]
fn parameters_bind_in_selects_updates_deletes_and_numbered_form() {
    let (_db, mut e) = TempDb::engine();
    for (v, n) in [("a", 1), ("b", 2), ("c", 3)] {
        e.run_query_with(
            "INSERT INTO t(v, n) VALUES (?1, ?2)",
            &[text(v), Value::Integer(n)],
        )
        .unwrap();
    }
    let rows = e
        .run_query_with(
            "SELECT v FROM t WHERE n >= ? ORDER BY n",
            &[Value::Integer(2)],
        )
        .unwrap()
        .rows;
    assert_eq!(
        rows,
        vec![vec![Cell::Text("b".into())], vec![Cell::Text("c".into())]]
    );
    // One value used twice through `?1`.
    let rows = e
        .run_query_with("SELECT ?1 + ?1", &[Value::Integer(21)])
        .unwrap()
        .rows;
    assert_eq!(rows, vec![vec![Cell::Int(42)]]);
    e.run_query_with(
        "UPDATE t SET n = ? WHERE v = ?",
        &[Value::Integer(9), text("a")],
    )
    .unwrap();
    let rows = e
        .run_query_with("SELECT n FROM t WHERE v = ?", &[text("a")])
        .unwrap()
        .rows;
    assert_eq!(rows, vec![vec![Cell::Int(9)]]);
    e.run_query_with(
        "DELETE FROM t WHERE id IN (?, ?)",
        &[Value::Integer(1), Value::Integer(3)],
    )
    .unwrap();
    assert_eq!(count(&mut e), Cell::Int(1));
    // NULL binds as NULL.
    let rows = e
        .run_query_with("SELECT ? IS NULL", &[Value::Null])
        .unwrap()
        .rows;
    assert_eq!(rows, vec![vec![Cell::Int(1)]]);
}

#[test]
fn a_wrong_parameter_count_fails_before_anything_runs() {
    let (_db, mut e) = TempDb::engine();
    for params in [
        vec![],
        vec![text("a")],
        vec![text("a"), Value::Integer(1), Value::Null],
    ] {
        let err = e
            .run_query_with("INSERT INTO t(v, n) VALUES (?, ?)", &params)
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Execute, "{} values", params.len());
        assert!(err.message.contains("2 parameter"), "{err}");
    }
    assert_eq!(count(&mut e), Cell::Int(0));
    // A statement with no parameters takes none.
    e.run_query_with("INSERT INTO t(v, n) VALUES ('a', 1)", &[])
        .unwrap();
    let err = e
        .run_query_with("INSERT INTO t(v, n) VALUES ('b', 2)", &[Value::Null])
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Execute);
    assert_eq!(count(&mut e), Cell::Int(1));
}

#[test]
fn run_query_with_takes_exactly_one_statement() {
    let (_db, mut e) = TempDb::engine();
    let err = e.run_query_with("SELECT 1; SELECT 2", &[]).unwrap_err();
    assert_ne!(err.kind, ErrorKind::Open);
    assert_eq!(count(&mut e), Cell::Int(0));
}

#[test]
fn a_bound_rowid_in_list_still_seeks() {
    let (_db, mut e) = TempDb::engine();
    for n in 0..20 {
        e.run_query_with("INSERT INTO t(n) VALUES (?)", &[Value::Integer(n)])
            .unwrap();
    }
    let rows = e
        .run_query_with(
            "SELECT n FROM t WHERE id IN (?, ?, ?) ORDER BY id",
            &[Value::Integer(3), Value::Integer(1), Value::Integer(99)],
        )
        .unwrap()
        .rows;
    assert_eq!(rows, vec![vec![Cell::Int(0)], vec![Cell::Int(2)]]);
}

/// A bare `?` after `?N` takes `N + 1`, as in sqlite3 (`?2, ?, ?1` reads
/// slots 2, 3, 1).
#[test]
fn a_bare_parameter_after_a_numbered_one_takes_the_next_index() {
    let (_db, mut e) = TempDb::engine();
    let rows = e
        .run_query_with(
            "SELECT ?2, ?, ?1",
            &[Value::Integer(1), Value::Integer(2), Value::Integer(3)],
        )
        .unwrap()
        .rows;
    assert_eq!(rows, vec![vec![Cell::Int(2), Cell::Int(3), Cell::Int(1)]]);
}
